//! `skUCommit = poseidon([sk_u, 0])` — exact reproduction of the standalone
//! `sk-commit-tool` (source from @gorelyshev) the acki-nacki Python tests use:
//!
//! ```text
//! let sk_u = Fr::from_bytes(&sk_u_bytes).expect("invalid BN254 field element");
//! let sk_u_commit = poseidon_hash(&[sk_u, Fr::zero()]);
//! ```
//!
//! Although `RootPN.generateVoucher` accepts an arbitrary `uint256` and the
//! halo2 circuit uses whatever value lands in the event, reproducing the
//! poseidon commitment keeps the kit aligned with v2 attack scenarios that
//! rely on the canonical sk_u ↔ skUCommit binding.

use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::KitResult;
use gosh_dark_dex_halo2_new_circuit::poseidon::poseidon_hash;
use halo2_base::halo2_proofs::halo2curves::bn256::Fr;
use halo2_base::halo2_proofs::halo2curves::ff::Field;
use halo2_base::halo2_proofs::halo2curves::ff::PrimeField;

const MODULE: KitModule = KitModule::External("dex.root_pn");

/// Compute `skUCommit = poseidon_hash([sk_u, 0])` for the given 32-byte sk_u.
/// `sk_u_hex` may be prefixed with `0x` and is parsed as little-endian (BN254
/// `Fr::from_bytes` reads LE), matching `sk-commit-tool`.
pub fn compute_sk_u_commit(sk_u_hex: &str) -> KitResult<[u8; 32]> {
    let stripped =
        sk_u_hex.strip_prefix("0x").or_else(|| sk_u_hex.strip_prefix("0X")).unwrap_or(sk_u_hex);

    let bytes_vec = hex::decode(stripped).map_err(|e| {
        KitError::new(MODULE, KitErrorCode::InvalidInput, format!("sk_u hex decode failed: {e}"))
    })?;

    let sk_u_bytes: [u8; 32] = bytes_vec.try_into().map_err(|v: Vec<u8>| {
        KitError::new(
            MODULE,
            KitErrorCode::InvalidInput,
            format!("sk_u must decode to exactly 32 bytes, got {}", v.len()),
        )
    })?;

    let sk_u: Fr = Option::from(Fr::from_repr(sk_u_bytes)).ok_or_else(|| {
        KitError::new(MODULE, KitErrorCode::InvalidInput, "sk_u is not a valid BN254 field element")
    })?;

    let commit = poseidon_hash(&[sk_u, Fr::ZERO]);
    Ok(commit.to_repr())
}

/// Convenience wrapper returning the commit as lower-case hex (no `0x`).
pub fn compute_sk_u_commit_hex(sk_u_hex: &str) -> KitResult<String> {
    compute_sk_u_commit(sk_u_hex).map(hex::encode)
}
