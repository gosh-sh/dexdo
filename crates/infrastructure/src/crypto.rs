// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Envelope encryption for at-rest secrets (api_secret, pn_seckey).
//
// Format: version(1) || nonce(12) || ciphertext+tag(>=16).
// The version byte lets us roll over the KEK later without re-deriving
// the scheme: a future v2 row decrypts under a new KEK while v1 rows
// keep decrypting under the original.

use aes_gcm::aead::Aead;
use aes_gcm::aead::AeadCore;
use aes_gcm::aead::KeyInit;
use aes_gcm::aead::OsRng;
use aes_gcm::Aes256Gcm;
use aes_gcm::Key;
use aes_gcm::Nonce;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use hmac::Hmac;
use hmac::Mac;
use sha2::Sha256;
use zeroize::ZeroizeOnDrop;

const KEK_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 1 + NONCE_LEN;
const MIN_BLOB_LEN: usize = HEADER_LEN + TAG_LEN;
const CURRENT_VERSION: u8 = 1;

/// Master encryption key. Zeroes its bytes when dropped. Its `Debug`
/// implementation redacts the key material so accidental `tracing` /
/// `dbg!` calls cannot leak it.
#[derive(ZeroizeOnDrop)]
pub struct Kek([u8; KEK_LEN]);

impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kek(<redacted>)")
    }
}

impl Kek {
    /// Parse a KEK from a hex string. Must decode to exactly 32 bytes.
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s.trim()).context("KEK must be valid hex")?;
        if bytes.len() != KEK_LEN {
            bail!("KEK must be {KEK_LEN} bytes (got {} bytes)", bytes.len());
        }
        let mut buf = [0u8; KEK_LEN];
        buf.copy_from_slice(&bytes);
        Ok(Self(buf))
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0))
    }
}

/// Encrypt `plaintext` under `kek`. Returns the encoded envelope ready to
/// be stored in a `bytea` column.
pub fn seal(kek: &Kek, plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = kek.cipher();
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext =
        cipher.encrypt(&nonce, plaintext).map_err(|e| anyhow!("aes-gcm encrypt failed: {e}"))?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.push(CURRENT_VERSION);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt an envelope previously produced by [`seal`]. Fails on any
/// tampering of the nonce, ciphertext, tag, or version byte.
pub fn open(kek: &Kek, blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < MIN_BLOB_LEN {
        bail!("envelope too short: {} bytes (min {MIN_BLOB_LEN})", blob.len());
    }
    let version = blob[0];
    if version != CURRENT_VERSION {
        bail!("unsupported envelope version: {version}");
    }
    let nonce = Nonce::from_slice(&blob[1..HEADER_LEN]);
    let ciphertext = &blob[HEADER_LEN..];

    kek.cipher().decrypt(nonce, ciphertext).map_err(|e| anyhow!("aes-gcm decrypt failed: {e}"))
}

/// Domain separator for KEK-derived API secrets. Bumping the version
/// suffix rotates every derived secret at once.
const API_SECRET_INFO: &[u8] = b"dodex/api-secret/v1";

/// Deterministic per-index API secret, bound to the KEK. The same
/// `(kek, index)` always yields the same 32 bytes, and a different KEK
/// yields a disjoint secret — so two environments never share a credential
/// even at the same index, and the value stays as secret as the KEK.
/// Lets the seeder mint reproducible credentials from a notes file without
/// persisting the plaintext anywhere but the KEK-encrypted DB row.
pub fn derive_api_secret(kek: &Kek, index: u32) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(&kek.0).expect("HMAC accepts any key length");
    mac.update(API_SECRET_INFO);
    mac.update(&index.to_be_bytes());
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&tag);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_kek() -> Kek {
        Kek::from_hex(&"00".repeat(KEK_LEN)).unwrap()
    }

    #[test]
    fn from_hex_accepts_valid_32_bytes() {
        let s = "ab".repeat(KEK_LEN);
        assert_eq!(s.len(), 64);
        let _ = Kek::from_hex(&s).expect("valid kek hex");
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        let s = "ab".repeat(30); // 30 bytes, not 32
        let err = Kek::from_hex(&s).unwrap_err();
        assert!(err.to_string().contains("bytes"), "got: {err}");
    }

    #[test]
    fn from_hex_rejects_non_hex() {
        let err = Kek::from_hex("zzzz").unwrap_err();
        assert!(err.to_string().to_lowercase().contains("hex"), "got: {err}");
    }

    #[test]
    fn from_hex_tolerates_surrounding_whitespace() {
        // CLI exports often leave a trailing newline; we should tolerate it
        // so startup does not fail on an otherwise valid KEK.
        let s = format!("  {}\n", "ab".repeat(KEK_LEN));
        let _ = Kek::from_hex(&s).expect("trimmed hex");
    }

    #[test]
    fn seal_open_roundtrip() {
        let kek = test_kek();
        let plaintext = b"the api secret bytes";
        let blob = seal(&kek, plaintext).unwrap();
        let recovered = open(&kek, &blob).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn seal_produces_distinct_blobs_for_identical_plaintexts() {
        // Distinct nonces -> distinct envelopes even for identical
        // plaintexts. Without this, identical secrets across rows would
        // produce identical ciphertexts and leak equality.
        let kek = test_kek();
        let pt = b"same input";
        let a = seal(&kek, pt).unwrap();
        let b = seal(&kek, pt).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        let kek = test_kek();
        let mut blob = seal(&kek, b"payload").unwrap();
        blob[HEADER_LEN] ^= 0x01;
        assert!(open(&kek, &blob).is_err());
    }

    #[test]
    fn open_rejects_tampered_nonce() {
        let kek = test_kek();
        let mut blob = seal(&kek, b"payload").unwrap();
        blob[1] ^= 0x01;
        assert!(open(&kek, &blob).is_err());
    }

    #[test]
    fn open_rejects_tampered_tag() {
        // The GCM tag is the last 16 bytes of the ciphertext-with-tag the
        // crate emits; flipping any of them must fail authentication.
        let kek = test_kek();
        let mut blob = seal(&kek, b"payload").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(open(&kek, &blob).is_err());
    }

    #[test]
    fn open_rejects_unknown_version() {
        let kek = test_kek();
        let mut blob = seal(&kek, b"payload").unwrap();
        blob[0] = 99;
        let err = open(&kek, &blob).unwrap_err();
        assert!(err.to_string().contains("version"), "got: {err}");
    }

    #[test]
    fn open_rejects_truncated_blob() {
        let kek = test_kek();
        let blob = seal(&kek, b"payload").unwrap();
        let err = open(&kek, &blob[..MIN_BLOB_LEN - 1]).unwrap_err();
        assert!(err.to_string().contains("too short"), "got: {err}");
    }

    #[test]
    fn open_rejects_with_different_kek() {
        let kek_a = test_kek();
        let kek_b = Kek::from_hex(&"ff".repeat(KEK_LEN)).unwrap();
        let blob = seal(&kek_a, b"payload").unwrap();
        assert!(open(&kek_b, &blob).is_err());
    }

    #[test]
    fn derive_api_secret_is_stable_and_kek_bound() {
        // Known-answer test: pins the derivation so any change that would
        // silently invalidate every seeded credential fails here first.
        // The companion value lives in the api test consts
        // (`SEED_API_SECRET` in services/api/tests/common/mod.rs).
        let kek = Kek::from_hex(&"ab".repeat(KEK_LEN)).unwrap();
        assert_eq!(
            hex::encode(derive_api_secret(&kek, 0)),
            "86c223a600ce630f9abf62ea2244ca638a2e02bb16d73d74128bf31f5d3e1910",
        );
        // Distinct index and distinct KEK both yield distinct secrets.
        assert_ne!(derive_api_secret(&kek, 0), derive_api_secret(&kek, 1));
        let other = Kek::from_hex(&"00".repeat(KEK_LEN)).unwrap();
        assert_ne!(derive_api_secret(&kek, 0), derive_api_secret(&other, 0));
    }

    #[test]
    fn seal_handles_empty_plaintext() {
        // Sealing an empty plaintext still produces a verifiable envelope
        // exactly MIN_BLOB_LEN bytes long.
        let kek = test_kek();
        let blob = seal(&kek, b"").unwrap();
        assert_eq!(blob.len(), MIN_BLOB_LEN);
        let recovered = open(&kek, &blob).unwrap();
        assert!(recovered.is_empty());
    }
}
