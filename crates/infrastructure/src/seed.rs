// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Idempotent bootstrap-time insert of API credentials read from a JSON
// notes file. Gated by `auth.seed_accounts` (+ `auth.seed_accounts_path`)
// in the API config; off by default and only flipped on in dev/test/
// staging by devops. Each note becomes one account whose API secret is
// derived from its position via `crypto::derive_api_secret`, so the file
// carries note custody keys only and never an API secret. When this
// seeding path is no longer needed the whole module and both config fields
// can be removed without touching the rest of the auth pipeline.

use std::path::Path;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use dodex_domain::Permission;
use num_bigint::BigUint;
use serde::Deserialize;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Row;
use sqlx::Transaction;
use tracing::debug;
use tracing::info;
use uuid::Uuid;

use crate::account_registry::ED25519_SECKEY_LEN;
use crate::crypto;
use crate::crypto::Kek;

/// External wire format of the seed notes file: a bare JSON array of
/// private-note records. `tokenType` and `value` describe what a note
/// holds and are intentionally not read here — they are not part of
/// account identity. The public keys arrive hex-encoded; the `numeric`
/// columns want decimal, so `note_to_seed_account` converts them.
#[derive(Debug, Deserialize)]
pub struct NoteEntry {
    pub pn_address: String,
    pub pn_pubkey_hex: String,
    pub pn_seckey_hex: String,
    pub pn_dih_hex: String,
}

// ---- Internal shape of the seed payload that `apply_seed` writes.
// `seed_accounts_from_notes` builds it by mapping each `NoteEntry`;
// integration tests build their own UUID-prefixed fixtures and run them
// through `apply_seed` directly. Public so those tests can construct it. ----

#[doc(hidden)]
#[derive(Debug, Deserialize)]
pub struct SeedData {
    pub accounts: Vec<SeedAccount>,
}

#[doc(hidden)]
#[derive(Debug, Deserialize)]
pub struct SeedAccount {
    pub label: Option<String>,
    pub pn_address: String,
    /// uint256 public key in decimal, ready for the `numeric(78,0)`
    /// `accounts.pn_pubkey` column.
    pub pn_pubkey_dec: String,
    /// 32-byte ed25519 private key, hex-encoded. Gets encrypted under
    /// the KEK at seed time.
    pub pn_seckey_hex: String,
    /// deposit_identifier_hash from `RootPN.PrivateNoteDeployed`, in
    /// decimal.
    pub pn_dih_dec: String,
    pub api_keys: Vec<SeedApiKey>,
}

#[doc(hidden)]
#[derive(Debug, Deserialize)]
pub struct SeedApiKey {
    pub api_key: String,
    /// 32-byte api_secret hex, encrypted under the KEK at seed time.
    pub api_secret_hex: String,
    pub permissions: Vec<String>,
}

// ---- Validated shape: every field is already normalised into the form
// the DB writer expects. `validate` is a pure function with no I/O, so
// any malformed entry aborts startup before the first INSERT and a
// partial DB state is impossible. ----

#[derive(Debug)]
struct ValidatedSeedData {
    accounts: Vec<ValidatedSeedAccount>,
}

#[derive(Debug)]
struct ValidatedSeedAccount {
    label: Option<String>,
    pn_address: String,
    /// Already verified to parse as a decimal uint256.
    pn_pubkey_dec: String,
    /// Hex-decoded; encrypted into a KEK envelope at insert time.
    pn_seckey: Vec<u8>,
    /// Already verified to parse as a decimal uint256.
    pn_dih_dec: String,
    api_keys: Vec<ValidatedSeedApiKey>,
}

#[derive(Debug)]
struct ValidatedSeedApiKey {
    api_key: String,
    /// Hex-decoded; encrypted into a KEK envelope at insert time.
    api_secret: Vec<u8>,
    permissions: Vec<Permission>,
}

/// Aggregate insert/skip counters returned to the caller for the
/// startup log line.
#[derive(Debug, Default)]
pub struct SeedReport {
    pub accounts_inserted: u64,
    pub accounts_skipped: u64,
    pub api_keys_inserted: u64,
    pub api_keys_skipped: u64,
}

/// Production/staging entry. Reads a JSON array of private-note records
/// from `path`, maps each to an account with a KEK-derived API credential
/// (`crypto::derive_api_secret`), and applies them through `apply_seed`.
/// A note's position in the file is its derivation index, so credential
/// `N` is stable as long as the file order is.
///
/// Fails before any DB write if the file is missing, is not valid JSON,
/// or carries a malformed field — startup aborts rather than half-seeding.
pub async fn seed_accounts_from_notes(pool: &PgPool, kek: &Kek, path: &Path) -> Result<SeedReport> {
    let notes = read_notes_file(path)?;

    let mut accounts = Vec::with_capacity(notes.len());
    for (index, note) in notes.into_iter().enumerate() {
        accounts.push(note_to_seed_account(kek, index as u32, note)?);
    }
    apply_seed(pool, kek, SeedData { accounts }).await
}

/// Read and parse the notes file. Split out from
/// [`seed_accounts_from_notes`] so the two startup-gating failure modes
/// (file missing / malformed JSON) are unit-testable without a database.
fn read_notes_file(path: &Path) -> Result<Vec<NoteEntry>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read seed notes file {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse seed notes file {}", path.display()))
}

/// Map one note to the internal `SeedAccount`, deriving its API credential
/// from `index`. `apply_seed`'s `validate` re-checks every field, so this
/// only needs to emit well-formed strings.
fn note_to_seed_account(kek: &Kek, index: u32, note: NoteEntry) -> Result<SeedAccount> {
    let pn_pubkey_dec = hex_to_dec_uint256(&note.pn_pubkey_hex)
        .with_context(|| format!("pn_pubkey_hex for {}", note.pn_address))?;
    let pn_dih_dec = hex_to_dec_uint256(&note.pn_dih_hex)
        .with_context(|| format!("pn_dih_hex for {}", note.pn_address))?;
    Ok(SeedAccount {
        label: Some(format!("test-mm-{:03}", index + 1)),
        pn_address: note.pn_address,
        pn_pubkey_dec,
        pn_seckey_hex: note.pn_seckey_hex,
        pn_dih_dec,
        api_keys: vec![SeedApiKey {
            api_key: format!("dk_live_test_{:03}", index + 1),
            api_secret_hex: hex::encode(crypto::derive_api_secret(kek, index)),
            permissions: vec!["USER_DATA".to_string(), "TRADE".to_string()],
        }],
    })
}

/// Convert an optionally `0x`-prefixed hex uint256 into the decimal string
/// the `numeric(78, 0)` columns expect, rejecting anything over 256 bits.
pub(crate) fn hex_to_dec_uint256(s: &str) -> Result<String> {
    let h = s.strip_prefix("0x").unwrap_or(s);
    let n = BigUint::parse_bytes(h.as_bytes(), 16).context("value must be valid hex")?;
    anyhow::ensure!(n.bits() <= 256, "value exceeds 256 bits");
    Ok(n.to_str_radix(10))
}

/// Apply an arbitrary `SeedData` payload against `pool`. Used by
/// `seed_accounts_from_notes` for the mapped notes file and by integration
/// tests for UUID-prefixed fixtures.
///
/// Pipeline:
///
/// 1. Validate every field (numerics, hex, permission labels) into
///    `ValidatedSeedData` — any failure here returns `Err` before a
///    single DB statement runs, so a malformed payload never produces
///    a partial DB state.
/// 2. Apply every INSERT inside a single Postgres transaction. A
///    mid-apply failure rolls everything back automatically when the
///    `Transaction` drops without `commit()`.
///
/// Idempotent on re-run: `ON CONFLICT DO NOTHING` on both tables, with
/// `*_skipped` counters reporting the no-ops.
#[doc(hidden)]
pub async fn apply_seed(pool: &PgPool, kek: &Kek, data: SeedData) -> Result<SeedReport> {
    let validated = validate(data).context("validate seed payload")?;

    let mut tx = pool.begin().await.context("begin seed transaction")?;
    let mut report = SeedReport::default();

    for account in &validated.accounts {
        let account_id = upsert_account(&mut tx, kek, account, &mut report)
            .await
            .with_context(|| format!("seed account {}", account.pn_address))?;

        for key in &account.api_keys {
            upsert_api_key(&mut tx, kek, account_id, key, &mut report)
                .await
                .with_context(|| format!("seed api_key {}", key.api_key))?;
        }
    }

    tx.commit().await.context("commit seed transaction")?;

    info!(
        accounts_inserted = report.accounts_inserted,
        accounts_skipped = report.accounts_skipped,
        api_keys_inserted = report.api_keys_inserted,
        api_keys_skipped = report.api_keys_skipped,
        "seeded credentials",
    );

    Ok(report)
}

/// Walk the parsed seed payload and reject any field that cannot be
/// applied. Pure function — no I/O, no encryption, no DB. The api will
/// refuse to start if this returns `Err`, which is the loudest possible
/// signal that a notes file carried a malformed field.
fn validate(parsed: SeedData) -> Result<ValidatedSeedData> {
    let mut accounts = Vec::with_capacity(parsed.accounts.len());
    for account in parsed.accounts {
        let pn_address = account.pn_address;
        parse_uint256_dec(&account.pn_pubkey_dec)
            .with_context(|| format!("pn_pubkey_dec for {pn_address} must fit numeric(78,0)"))?;
        parse_uint256_dec(&account.pn_dih_dec)
            .with_context(|| format!("pn_dih_dec for {pn_address} must fit numeric(78,0)"))?;
        let pn_seckey = hex::decode(&account.pn_seckey_hex)
            .with_context(|| format!("pn_seckey_hex for {pn_address} must be valid hex"))?;
        anyhow::ensure!(
            pn_seckey.len() == ED25519_SECKEY_LEN,
            "pn_seckey_hex for {pn_address} must be {ED25519_SECKEY_LEN} bytes (got {})",
            pn_seckey.len(),
        );

        let mut api_keys = Vec::with_capacity(account.api_keys.len());
        for key in account.api_keys {
            if key.permissions.is_empty() {
                bail!("api_key {} has no permissions", key.api_key);
            }
            let mut permissions = Vec::with_capacity(key.permissions.len());
            for label in &key.permissions {
                match Permission::parse(label) {
                    Some(p) => permissions.push(p),
                    None => bail!("api_key {}: unknown permission {label:?}", key.api_key),
                }
            }
            let api_secret = hex::decode(&key.api_secret_hex)
                .with_context(|| format!("api_secret_hex for {} must be valid hex", key.api_key))?;
            api_keys.push(ValidatedSeedApiKey { api_key: key.api_key, api_secret, permissions });
        }

        accounts.push(ValidatedSeedAccount {
            label: account.label,
            pn_address,
            pn_pubkey_dec: account.pn_pubkey_dec,
            pn_seckey,
            pn_dih_dec: account.pn_dih_dec,
            api_keys,
        });
    }
    Ok(ValidatedSeedData { accounts })
}

/// Parse a decimal `uint256` and reject values that would overflow
/// the column at INSERT. `numeric(78, 0)` holds 78 digits; 256-bit
/// values fit in 78 digits, but a longer-string input that happened
/// to be all digits would pass `BigUint::parse_bytes` and then fail
/// later with a much less clear Postgres-side error.
fn parse_uint256_dec(s: &str) -> Result<BigUint> {
    let n = BigUint::parse_bytes(s.as_bytes(), 10)
        .context("value must be a decimal non-negative integer")?;
    anyhow::ensure!(n.bits() <= 256, "value exceeds 256 bits");
    Ok(n)
}

async fn upsert_account(
    tx: &mut Transaction<'_, Postgres>,
    kek: &Kek,
    account: &ValidatedSeedAccount,
    report: &mut SeedReport,
) -> Result<Uuid> {
    let pn_seckey_enc = crypto::seal(kek, &account.pn_seckey).context("encrypt pn_seckey")?;

    let row = sqlx::query(
        r#"insert into accounts
               (label, pn_address, pn_pubkey, pn_seckey_enc, pn_dih)
           values ($1, $2, $3::numeric, $4, $5::numeric)
           -- No arbiter target: `accounts` is unique on both pn_address and
           -- pn_dih (accounts_pn_dih_key). A narrow (pn_address) arbiter lets a
           -- concurrent identical seed (parallel test setups) raise a pn_dih
           -- unique violation instead of skipping, since ON CONFLICT only
           -- suppresses conflicts on its arbiter index.
           on conflict do nothing
           returning id"#,
    )
    .bind(&account.label)
    .bind(&account.pn_address)
    .bind(&account.pn_pubkey_dec)
    .bind(&pn_seckey_enc)
    .bind(&account.pn_dih_dec)
    .fetch_optional(&mut **tx)
    .await
    .context("insert account")?;

    if let Some(row) = row {
        let id: Uuid = row.try_get("id").context("read inserted account id")?;
        report.accounts_inserted += 1;
        debug!(account_id = %id, pn_address = %account.pn_address, "seed: inserted account");
        Ok(id)
    } else {
        // Row already exists — fetch its id so the api_keys can attach.
        let id: Uuid = sqlx::query_scalar("select id from accounts where pn_address = $1")
            .bind(&account.pn_address)
            .fetch_one(&mut **tx)
            .await
            .context("look up existing account by pn_address")?;
        report.accounts_skipped += 1;
        debug!(account_id = %id, pn_address = %account.pn_address, "seed: account already exists");
        Ok(id)
    }
}

async fn upsert_api_key(
    tx: &mut Transaction<'_, Postgres>,
    kek: &Kek,
    account_id: Uuid,
    key: &ValidatedSeedApiKey,
    report: &mut SeedReport,
) -> Result<()> {
    let perms: Vec<String> = key.permissions.iter().map(|p| p.as_str().to_string()).collect();
    let api_secret_enc = crypto::seal(kek, &key.api_secret).context("encrypt api_secret")?;

    // Partial-unique index `(api_key) WHERE disabled_at IS NULL` is the
    // conflict target; PG 11+ supports it via the matching WHERE clause.
    let result = sqlx::query(
        r#"insert into api_keys
               (account_id, api_key, api_secret_enc, permissions)
           values ($1, $2, $3, $4::auth_permission[])
           on conflict (api_key) where disabled_at is null do nothing"#,
    )
    .bind(account_id)
    .bind(&key.api_key)
    .bind(&api_secret_enc)
    .bind(&perms)
    .execute(&mut **tx)
    .await
    .context("insert api_key")?;

    if result.rows_affected() > 0 {
        report.api_keys_inserted += 1;
        debug!(api_key = %key.api_key, account_id = %account_id, "seed: inserted api_key");
    } else {
        report.api_keys_skipped += 1;
        debug!(api_key = %key.api_key, account_id = %account_id, "seed: api_key already exists");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_good_account() -> SeedAccount {
        SeedAccount {
            label: Some("fixture".into()),
            pn_address: "0:fixture".into(),
            pn_pubkey_dec: "1".into(),
            pn_seckey_hex: "00".repeat(32),
            pn_dih_dec: "2".into(),
            api_keys: vec![SeedApiKey {
                api_key: "dk_live_fixture".into(),
                api_secret_hex: "00".repeat(32),
                permissions: vec!["USER_DATA".into(), "TRADE".into()],
            }],
        }
    }

    #[test]
    fn read_notes_file_rejects_missing_file() {
        // Startup gates on this read; a missing file must abort with a
        // clear cause before any DB work.
        let path = Path::new("/nonexistent/dodex-seed-notes-missing.json");
        let err = read_notes_file(path).unwrap_err();
        assert!(format!("{err:#}").contains("read seed notes file"), "got: {err:#}");
    }

    #[test]
    fn read_notes_file_rejects_malformed_json() {
        let path = std::env::temp_dir().join("dodex_seed_notes_malformed.json");
        std::fs::write(&path, b"{ not valid json ]").expect("write temp notes file");
        let result = read_notes_file(&path);
        let _ = std::fs::remove_file(&path);
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("parse seed notes file"), "got: {err:#}");
    }

    #[test]
    fn validate_rejects_non_decimal_pn_pubkey() {
        let mut acc = one_good_account();
        acc.pn_pubkey_dec = "not-a-number".into();
        let err = validate(SeedData { accounts: vec![acc] }).unwrap_err();
        assert!(format!("{err:#}").contains("pn_pubkey_dec"), "got: {err:#}");
    }

    #[test]
    fn validate_rejects_non_decimal_pn_dih() {
        let mut acc = one_good_account();
        acc.pn_dih_dec = "0xdeadbeef".into();
        let err = validate(SeedData { accounts: vec![acc] }).unwrap_err();
        assert!(format!("{err:#}").contains("pn_dih_dec"), "got: {err:#}");
    }

    #[test]
    fn validate_rejects_bad_hex_seckey() {
        let mut acc = one_good_account();
        acc.pn_seckey_hex = "not-hex-at-all".into();
        let err = validate(SeedData { accounts: vec![acc] }).unwrap_err();
        assert!(format!("{err:#}").contains("pn_seckey_hex"), "got: {err:#}");
    }

    #[test]
    fn validate_rejects_wrong_length_seckey() {
        // Valid hex but not 32 bytes: would seed "successfully" and only
        // fail on the account's first on-chain signature. Must reject at
        // seed time, matching the registration path.
        let mut acc = one_good_account();
        acc.pn_seckey_hex = "00".repeat(16);
        let err = validate(SeedData { accounts: vec![acc] }).unwrap_err();
        assert!(format!("{err:#}").contains("32 bytes"), "got: {err:#}");
    }

    #[test]
    fn validate_rejects_unknown_permission() {
        let mut acc = one_good_account();
        acc.api_keys[0].permissions = vec!["SUPER_ADMIN".into()];
        let err = validate(SeedData { accounts: vec![acc] }).unwrap_err();
        assert!(format!("{err:#}").contains("unknown permission"), "got: {err:#}");
    }

    #[test]
    fn validate_rejects_empty_permissions() {
        let mut acc = one_good_account();
        acc.api_keys[0].permissions = vec![];
        let err = validate(SeedData { accounts: vec![acc] }).unwrap_err();
        assert!(format!("{err:#}").contains("no permissions"), "got: {err:#}");
    }

    #[test]
    fn validate_rejects_bad_hex_secret() {
        let mut acc = one_good_account();
        acc.api_keys[0].api_secret_hex = "zz".into();
        let err = validate(SeedData { accounts: vec![acc] }).unwrap_err();
        assert!(format!("{err:#}").contains("api_secret_hex"), "got: {err:#}");
    }
}
