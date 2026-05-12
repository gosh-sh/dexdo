// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Idempotent bootstrap-time insert of a fixed set of test credentials.
// Triggered by `auth.seed_accounts` in the API config; off by default.
// The credentials below come from `bee-engine-private/bee_dex/pn_pool_1.json`
// — the same PNs the bee-dex integration tests use. The whole thing is
// scoped to test/dev environments by config (devops controls the flag
// per environment); when the route is no longer needed the entire
// module and the `seed_accounts` config field can be removed without
// touching the rest of the auth pipeline.

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

use crate::crypto;
use crate::crypto::Kek;

/// Hard-coded test credentials baked into the binary. Each account
/// references one PN from `bee_dex/pn_pool_1.json`; the `pn_seckey_hex`
/// values mirror that file. The `api_key`/`api_secret_hex` pairs were
/// generated once and listed in the implementation handover — the
/// secret cannot be recovered from the DB after seeding, so this
/// literal is the only place to look them up.
const SEED_DATA: &str = r#"{
  "accounts": [
    {
      "label": "test-mm-001",
      "pn_address": "0:42781640a51593054f3cdaba2dc9f9bcd746a7847828864a7bec0a84c1a9a4ab",
      "pn_pubkey_dec": "19346934288648506821551876531437489357784615882602954281922232117079179800819",
      "pn_seckey_hex": "c328edabf61f9d9dac4bee58941c760738ab0c8a7d5c3c62b2bee8c582d9bffb",
      "pn_dih_dec": "91597984451254183122589084749522370730076015931247253901108057213988000261129",
      "api_keys": [
        {
          "api_key": "dk_live_test_001",
          "api_secret_hex": "1de6fc5cf8899e7f1dacf449fe46c3c88854478b7fcd9dd26c664535ee589966",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-002",
      "pn_address": "0:18f7d7f71f9c3235c295bb87006ab4850285ca55dd72dbe627c470f979adb5c0",
      "pn_pubkey_dec": "49232188003000907856126985913969344325080898484235719053644210260177664421806",
      "pn_seckey_hex": "77fd4239e6fc28eaaefdca37644f4b389d66ad50b18a10082dbd168edf59b108",
      "pn_dih_dec": "10252568042899048316154494109474523048725512570691861185709482663841516630275",
      "api_keys": [
        {
          "api_key": "dk_live_test_002",
          "api_secret_hex": "0353c808ebdf3f4d5074bc9d9465093acc28cf7ce4ef24d413dd98c4bc4191ef",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-003",
      "pn_address": "0:2032f18e320f24791ace5804087b1b6ca7d34a73983b3b594dd3c9d9085da4a7",
      "pn_pubkey_dec": "37485791715781044988672852718810939353444195984313611877460668917276066148133",
      "pn_seckey_hex": "ddef1eed6e3d61d482c1d0663ab092412d99915041bf91151063e7aa0d590ca5",
      "pn_dih_dec": "99512468708705165264103947768344128167405228400451870911220982718036282782252",
      "api_keys": [
        {
          "api_key": "dk_live_test_003",
          "api_secret_hex": "e84ad7681d4c4604948fc94ce40d7e243332b7315a6e631a06f2f128d971668d",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-004",
      "pn_address": "0:f0289b2052e384b01a34afc31c5a31ac08639626c57676e050d29c169846896f",
      "pn_pubkey_dec": "100944526204428366913325852514116799782582349658892154233666238814117718819592",
      "pn_seckey_hex": "29b20c41a61d19bc4f0fd67f2af3519814911777adddf37555ce0eb1b7eb46d1",
      "pn_dih_dec": "62691184560625273924201789457918575043804216849743569860874710337360089133082",
      "api_keys": [
        {
          "api_key": "dk_live_test_004",
          "api_secret_hex": "fd088668f8bac878564dc1d2a6ec0307c20e9f86ddb4328be457621a0bc291ed",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-005",
      "pn_address": "0:be4d08affd3bab2ab63ac34e730bd4e40455819248f4f708282250eeecf2feb3",
      "pn_pubkey_dec": "42001550860053245571612492802799158216186339659029386622389461444138365783995",
      "pn_seckey_hex": "9a703c764ba51ad38d0a82570ad4a53c6bd4bc1eab070d091853a2c878efcbbc",
      "pn_dih_dec": "111301083057630350512421830884652147082179619752976479073095658950496185745435",
      "api_keys": [
        {
          "api_key": "dk_live_test_005",
          "api_secret_hex": "d55f9fe6a9a61fdde23594935ea7587edb7c0d417fb08e8521a9dc2c019f917b",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-006",
      "pn_address": "0:f6693cca08a1189ed0751e72034331fc6c618fd6b48b8d9f4bcaec8cdc02acef",
      "pn_pubkey_dec": "62656270445111551015211246371519978300891601647002719671701260212101395932752",
      "pn_seckey_hex": "8e96c16ab3f3a3b3af334644068863a4043fb7add79df37f31f803c553cf430a",
      "pn_dih_dec": "64486072218031675043838168034247118859853163611294727377687979520830585878310",
      "api_keys": [
        {
          "api_key": "dk_live_test_006",
          "api_secret_hex": "958e800c6291aa0f09e60cdd25e18cb70badb847c08759c773a276c9611c5ea2",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-007",
      "pn_address": "0:16db530b3946eb5af86ec4782bede0eac128a022b2f4cc51ee2b29850f467c13",
      "pn_pubkey_dec": "70533864451812404305052884390335888001057892751326219657488468519275716877451",
      "pn_seckey_hex": "acc7dbcc128d601462f466c5aa9ea976711aa0863e46d7b4688f5dcb3c0313f2",
      "pn_dih_dec": "26990868131632782108603970237671974385047450994530538775387508498653147617280",
      "api_keys": [
        {
          "api_key": "dk_live_test_007",
          "api_secret_hex": "236aee6de99e14c223c7e25e251e7587ede5510e80753c6d6115088c5c7cb844",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-008",
      "pn_address": "0:e621ce9f99ad514db84eee4b444c869535be30a9f3b0e8f31cc81e77bbee4a7c",
      "pn_pubkey_dec": "99209826666586135046411981876493087881939938915982111987762628228799693121781",
      "pn_seckey_hex": "5171650f4946c5316d4758630a715847f06d5e344b1ef17bc54a06f0de6bcdc2",
      "pn_dih_dec": "114409880977100648999174829418568836856227681091305144086398305915708959623962",
      "api_keys": [
        {
          "api_key": "dk_live_test_008",
          "api_secret_hex": "d52b4c47a99929274e25d7f7a709e737cdb1373fbd49d36a71d7ec6bab4b03f6",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-009",
      "pn_address": "0:131093824ee79d800fe91c5b1e65db452129170d238e2876413315b02e49286b",
      "pn_pubkey_dec": "12768762252697969716835068471902406934784106292798806737127420154259895484381",
      "pn_seckey_hex": "9bae4b81332909eebd29039cf625386286437363f4b854b4781511324168da8b",
      "pn_dih_dec": "74100076590525750892507106808382633235984734319542541062311288467386743262465",
      "api_keys": [
        {
          "api_key": "dk_live_test_009",
          "api_secret_hex": "8b9897acae053e918b9ef2371361695de26317249803d26894996503b2746c73",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    },
    {
      "label": "test-mm-010",
      "pn_address": "0:053d69da328ed15fff87a9cfaac2ffb8d6c5b9f4f3027420eeac3b08b3b8dfb9",
      "pn_pubkey_dec": "93535152165785526843738682827961357499720450816081085955088881536397093451938",
      "pn_seckey_hex": "ceccbfe7f57ad9d6a9843d235aa8febec176a1d7fa558520e924112067183962",
      "pn_dih_dec": "45492844464306292814724934814229995476685304969392055421441253210607109927698",
      "api_keys": [
        {
          "api_key": "dk_live_test_010",
          "api_secret_hex": "a4b455b9c8355e383af377302d3b2179409edcb2d685f4ffeaeaee39d3c2c710",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    }
  ]
}"#;

// ---- Public shape of the seed payload. Exposed so integration tests
// can build their own UUID-prefixed fixtures and run them through
// `apply_seed` without colliding with the baked JSON; production code
// only uses the no-argument `seed_accounts` entry point. ----

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
// any malformed entry in the baked JSON aborts startup before the
// first INSERT and a partial DB state is impossible. ----

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

/// Production entry. Parses the binary-embedded JSON and applies it
/// through `apply_seed`. The pipeline is documented on `apply_seed`.
pub async fn seed_accounts(pool: &PgPool, kek: &Kek) -> Result<SeedReport> {
    let parsed: SeedData =
        serde_json::from_str(SEED_DATA).context("parse hard-coded seed_data.json")?;
    apply_seed(pool, kek, parsed).await
}

/// Apply an arbitrary `SeedData` payload against `pool`. Used by
/// `seed_accounts` for the production baked-in JSON and by integration
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

/// Walk the parsed JSON and reject any field that cannot be applied.
/// Pure function — no I/O, no encryption, no DB. The api will refuse
/// to start if this returns `Err`, which is the loudest possible
/// signal that someone edited `SEED_DATA` incorrectly.
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
           on conflict (pn_address) do nothing
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
    fn baked_seed_data_validates() {
        // The embedded JSON parses into the raw shape and survives the
        // full validation pipeline. Editing SEED_DATA into something
        // malformed gets a clear test failure here rather than a
        // confusing runtime error on first boot.
        let data: SeedData = serde_json::from_str(SEED_DATA).expect("seed_data.json must parse");
        let validated = validate(data).expect("seed_data.json must validate");
        assert_eq!(validated.accounts.len(), 10, "baked seed must contain ten accounts");
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
