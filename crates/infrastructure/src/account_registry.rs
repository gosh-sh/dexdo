// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Postgres-backed `AccountRegistry`: the write side of `POST
// /api/v1/accounts`. It mints a trading account plus its first API
// credential from a client-supplied PrivateNote, mirroring what the
// seeder does for one notes-file entry — except the API secret is a fresh
// random value rather than a KEK-derived one, because there is no
// notes-file index to reproduce. The secret is stored sealed and returned
// to the caller exactly once.
//
// Insert-only, like the seeder: a note whose `pn_address`/`pn_dih` already
// has an account is rejected with `NoteAlreadyRegistered` rather than
// re-credentialed. The credential mint + both inserts run behind this port
// (not in the application use case) because they need the KEK, which lives
// in infrastructure.

use std::sync::Arc;

use async_trait::async_trait;
use dodex_application::AccountRegistry;
use dodex_application::NewAccountNote;
use dodex_application::RegisteredAccount;
use dodex_domain::DomainError;
use dodex_domain::Permission;
use dodex_domain::SensitiveBytes;
use sqlx::PgPool;
use sqlx::Row;
use tracing::debug;
use tracing::error;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::crypto;
use crate::crypto::Kek;
use crate::seed::hex_to_dec_uint256;

/// Tags self-registered accounts in `accounts.label`, distinct from the
/// seeder's `test-mm-NNN`. Informational only — `label` carries no
/// constraint.
const REGISTERED_LABEL: &str = "registered";

/// Permissions every registered account gets, so it can both read its data
/// and trade immediately. Single source of truth for both the
/// `auth_permission[]` column and the echoed response.
const REGISTERED_PERMISSIONS: [Permission; 2] = [Permission::UserData, Permission::Trade];

pub struct PostgresAccountRegistry {
    pool: PgPool,
    kek: Arc<Kek>,
}

impl PostgresAccountRegistry {
    pub fn new(pool: PgPool, kek: Arc<Kek>) -> Self {
        Self { pool, kek }
    }
}

/// A note whose fields are already normalised into the form the DB writer
/// expects: hex public-key / dih converted to the decimal the
/// `numeric(78,0)` columns want, secret key hex-decoded into a zeroizing
/// [`SensitiveBytes`] (length-checked at construction, wiped on drop).
/// Produced by the pure [`validate_note`] so a malformed field is a typed
/// `InvalidParameter` before any DB statement runs.
struct ValidatedNote {
    pn_address: String,
    pn_pubkey_dec: String,
    pn_seckey: SensitiveBytes,
    pn_dih_dec: String,
}

/// Validate and normalise the wire note. Pure — no I/O, no crypto — so the
/// 400 path is unit-testable without a database. Every failure collapses
/// to `InvalidParameter` (-1130): the request reached us intact and a
/// field is shape-wrong.
fn validate_note(note: &NewAccountNote) -> Result<ValidatedNote, DomainError> {
    let pn_pubkey_dec = hex_to_dec_uint256(&note.pn_pubkey_hex).map_err(|e| {
        debug!(error = %format!("{e:#}"), "register: invalid pn_pubkey_hex");
        DomainError::InvalidParameter
    })?;
    let pn_dih_dec = hex_to_dec_uint256(&note.pn_dih_hex).map_err(|e| {
        debug!(error = %format!("{e:#}"), "register: invalid pn_dih_hex");
        DomainError::InvalidParameter
    })?;
    let pn_seckey_bytes = hex::decode(note.pn_seckey_hex.as_str().trim()).map_err(|_| {
        debug!("register: pn_seckey_hex is not valid hex");
        DomainError::InvalidParameter
    })?;
    // `SensitiveBytes::seckey` enforces the 32-byte length and zeroizes on
    // drop. Its own wrong-length error is `Unexpected` (the read path treats
    // a corrupt DB row as a 500); here the bytes are client-supplied, so a
    // wrong length is a 400 — remap to `InvalidParameter`.
    let pn_seckey = SensitiveBytes::seckey(pn_seckey_bytes).map_err(|_| {
        debug!("register: pn_seckey wrong length");
        DomainError::InvalidParameter
    })?;
    Ok(ValidatedNote { pn_address: note.pn_address.clone(), pn_pubkey_dec, pn_seckey, pn_dih_dec })
}

/// Build a `map_err` closure that logs the cause and collapses any
/// internal failure (DB error, sealing) to `Unexpected` (-1000).
fn unexpected<E: std::fmt::Display>(context: &'static str) -> impl FnOnce(E) -> DomainError {
    move |err| {
        error!(error = %format!("{err:#}"), context, "account registry: unexpected failure");
        DomainError::Unexpected
    }
}

#[async_trait]
impl AccountRegistry for PostgresAccountRegistry {
    async fn register(&self, note: NewAccountNote) -> Result<RegisteredAccount, DomainError> {
        let valid = validate_note(&note)?;

        // Random, not derived: registration has no notes-file index to
        // reproduce, and the secret is stored sealed regardless. 16 bytes
        // of key entropy is unguessable; the secret is a full 32 bytes.
        let mut secret = [0u8; 32];
        crypto::fill_random(&mut secret);
        let mut key_rand = [0u8; 16];
        crypto::fill_random(&mut key_rand);
        let api_key = format!("dk_live_{}", hex::encode(key_rand));
        let api_secret_hex = hex::encode(secret);

        let pn_seckey_enc = crypto::seal(&self.kek, valid.pn_seckey.as_slice())
            .map_err(unexpected("seal pn_seckey"))?;
        let api_secret_enc =
            crypto::seal(&self.kek, &secret).map_err(unexpected("seal api_secret"))?;

        // The raw secret now lives sealed (`api_secret_enc`) and hex-encoded
        // for the one-time response (`api_secret_hex`); the plaintext arrays
        // are no longer read, so wipe them rather than leave the long-lived
        // signing-key material resident until the frame unwinds. The response
        // String copy is out of reach here — defense in depth, not a seal.
        secret.zeroize();
        key_rand.zeroize();

        let mut tx = self.pool.begin().await.map_err(unexpected("begin tx"))?;

        // No arbiter target: `accounts` is unique on both `pn_address` and
        // `pn_dih`, so a bare `on conflict do nothing` suppresses a
        // conflict on either index and returns no row — which is exactly
        // the insert-only signal we want for both. A concurrent identical
        // registration blocks here until the first commits, then sees the
        // conflict and returns no row too.
        let row = sqlx::query(
            r#"insert into accounts (label, pn_address, pn_pubkey, pn_seckey_enc, pn_dih)
               values ($1, $2, $3::numeric, $4, $5::numeric)
               on conflict do nothing
               returning id"#,
        )
        .bind(REGISTERED_LABEL)
        .bind(&valid.pn_address)
        .bind(&valid.pn_pubkey_dec)
        .bind(&pn_seckey_enc)
        .bind(&valid.pn_dih_dec)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unexpected("insert account"))?;

        let Some(row) = row else {
            // pn_address or pn_dih already present. Drop (roll back) the tx
            // and reject — never re-credential or re-issue a secret.
            return Err(DomainError::NoteAlreadyRegistered);
        };
        let account_id: Uuid = row.try_get("id").map_err(unexpected("read account id"))?;

        // `auth_permission[]` binds as text; derive the wire strings from
        // the single `Permission` source of truth.
        let permission_strs: Vec<&str> =
            REGISTERED_PERMISSIONS.iter().map(Permission::as_str).collect();
        let inserted = sqlx::query(
            r#"insert into api_keys (account_id, api_key, api_secret_enc, permissions)
               values ($1, $2, $3, $4::auth_permission[])
               on conflict (api_key) where disabled_at is null do nothing"#,
        )
        .bind(account_id)
        .bind(&api_key)
        .bind(&api_secret_enc)
        .bind(&permission_strs[..])
        .execute(&mut *tx)
        .await
        .map_err(unexpected("insert api_key"))?;
        if inserted.rows_affected() == 0 {
            // The 128-bit random api_key collided with a live key —
            // astronomically unlikely. Fail closed (rolling back the
            // account insert) rather than hand back a credential that
            // points at someone else's account.
            error!(api_key = %api_key, "register: api_key collision on insert");
            return Err(DomainError::Unexpected);
        }

        tx.commit().await.map_err(unexpected("commit tx"))?;

        Ok(RegisteredAccount {
            account_id,
            pn_address: valid.pn_address,
            api_key,
            api_secret_hex: api_secret_hex.into(),
            permissions: REGISTERED_PERMISSIONS.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_note() -> NewAccountNote {
        NewAccountNote {
            pn_address: "0:fixture".into(),
            pn_pubkey_hex: "ab".repeat(32),
            pn_seckey_hex: "00".repeat(32).into(),
            pn_dih_hex: "cd".repeat(32),
        }
    }

    #[test]
    fn validate_accepts_well_formed_note() {
        let v = validate_note(&good_note()).expect("valid note");
        assert_eq!(v.pn_seckey.len(), dodex_domain::PN_SECKEY_BYTE_LEN);
        // hex -> decimal conversion produced a non-empty decimal string.
        assert!(v.pn_pubkey_dec.bytes().all(|b| b.is_ascii_digit()));
        assert!(v.pn_dih_dec.bytes().all(|b| b.is_ascii_digit()));
    }

    // `matches!` rather than `unwrap_err`: `ValidatedNote` holds the raw
    // secret key and deliberately has no `Debug`, so the `Ok` arm must not
    // be formatted.
    #[test]
    fn validate_rejects_non_hex_pubkey() {
        let mut n = good_note();
        n.pn_pubkey_hex = "not-hex".into();
        assert!(matches!(validate_note(&n), Err(DomainError::InvalidParameter)));
    }

    #[test]
    fn validate_rejects_oversized_dih() {
        // 33 bytes of hex = 264 bits, over the 256-bit numeric(78,0) ceiling.
        let mut n = good_note();
        n.pn_dih_hex = "ff".repeat(33);
        assert!(matches!(validate_note(&n), Err(DomainError::InvalidParameter)));
    }

    #[test]
    fn validate_rejects_non_hex_seckey() {
        let mut n = good_note();
        n.pn_seckey_hex = "zz".repeat(32).into();
        assert!(matches!(validate_note(&n), Err(DomainError::InvalidParameter)));
    }

    #[test]
    fn validate_rejects_wrong_length_seckey() {
        let mut n = good_note();
        n.pn_seckey_hex = "00".repeat(16).into(); // 16 bytes, not 32
        assert!(matches!(validate_note(&n), Err(DomainError::InvalidParameter)));
    }
}
