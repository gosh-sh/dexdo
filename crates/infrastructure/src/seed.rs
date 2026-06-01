// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Idempotent bootstrap-time insert of a fixed set of test credentials.
// Triggered by `auth.seed_accounts` in the API config; off by default
// and only flipped on in dev/test environments by devops. When the
// route is no longer needed the entire module and the `seed_accounts`
// config field can be removed without touching the rest of the auth
// pipeline.

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

/// Hard-coded test credentials baked into the binary. In the DB the
/// `api_secret_hex` and `pn_seckey_hex` are stored encrypted under the
/// KEK (`crypto::seal`), so recovering them requires that environment's
/// KEK — this literal is the readable reference for the dev/test secrets.
const SEED_DATA: &str = r#"{
  "accounts": [
    {
      "label": "test-mm-001",
      "pn_address": "0:20e8f91330d643c1c7d62f69f29daf0603bda050d3436f7d24096b5f567c0be9",
      "pn_pubkey_dec": "70969641521947521544907052554963635732660713948458019842077559075920641595863",
      "pn_seckey_hex": "483ee42add5d95d2fd00bc0b3aec9f25570ba62faf388f4f0b5897838d4baa8b",
      "pn_dih_dec": "41285154978381328375205393245656689700447731610705637362063089883892566741275",
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
      "pn_address": "0:0cc78899137e5a1f0a23a65f632dd6324121e5d4008a5b686950592f420ba2c5",
      "pn_pubkey_dec": "69923806751694534953804229510116762784772903739067523864343460953777067982700",
      "pn_seckey_hex": "b88c66666b410eef05bfb35018eecb22a48675374ac959fe65ff57807776e963",
      "pn_dih_dec": "14034034795317689084237356804772214244921992097377417543726221691419373484545",
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
      "pn_address": "0:6439f06c7f86e08a3f211cb115e22d716f077db6cf4dc2a18e47b29049ab6910",
      "pn_pubkey_dec": "111364943336038765597784218038152056463510706801151421672678766767429094304839",
      "pn_seckey_hex": "490ef80c0dad4cbc85cb287ba63b60273732650900ca87651dc3d655c8a5ac5c",
      "pn_dih_dec": "66829662673953715544742630549865807996682983234744558714375637659475983631403",
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
      "pn_address": "0:acdc13a1c154e4f9717ee15b6ee529007946a3616097caa1a6872cfab2cdbde5",
      "pn_pubkey_dec": "104493193872113324458927995886512650143617617567633653425083716267889540151578",
      "pn_seckey_hex": "3c539f966fb6a1174be4c1cd2bb90f32ebf2d4465d62ef8ebc280bb8a2a59c5f",
      "pn_dih_dec": "5853573793107541443653972510998809781932122309795273326119028137187770094881",
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
      "pn_address": "0:de7d9b9fc2fd8b30cffddf1d212f8ba6966f6332ebe39cbc79a42f7b5619614c",
      "pn_pubkey_dec": "96521214283964455061122373050499476900255520915326503684005820516581582658971",
      "pn_seckey_hex": "ef707d8351435b42a8fa01b1ba60479fe4b9df4d1041f03a8680ab45124ac9b5",
      "pn_dih_dec": "15531958371531797046693027872107917062798496196966881265973412715290931273997",
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
      "pn_address": "0:5d4b94314b2689795ef7075fbe1c50a07e0e93c4dd95eea0b85c1cd17f217c79",
      "pn_pubkey_dec": "49453966171427435895852250487079109655919413896439524994839794123673610400877",
      "pn_seckey_hex": "bb9e381bac20c87a4963cd949caf3b2c78645154ea83d8cb3d5ce4574db63300",
      "pn_dih_dec": "31527730002298787547173368052474382097551904457640528163082865640950886484740",
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
      "pn_address": "0:b536d8bc5737c910eff8b7bc458b41bc06ddd9044f82ec8d37948cc004c19476",
      "pn_pubkey_dec": "32140876268313782030789615539966278995440322965981863081060267309715172016955",
      "pn_seckey_hex": "2f21bc13483862a4322742df049a311832922696a76f93837963bfe3f7d1e776",
      "pn_dih_dec": "44619256069185850071980031855201858456095594341008969771714892444061499822854",
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
      "pn_address": "0:0f56e796e43062f70f570a69b10f68a4019fb5ef0b39435ed8c7afe65d112f5d",
      "pn_pubkey_dec": "21638739161140850916985450669329537446936769704953854348749707564285990842092",
      "pn_seckey_hex": "9d14540a1e483d0f5d70dad044cd6ca60208ab745bb57bbee111e447f7cabf94",
      "pn_dih_dec": "96359695240202148397281023347167783600879376291101820228698371886669250640393",
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
      "pn_address": "0:e3801de2ab42bf580210e6693f576b0d364f4a67edd467e454cbe86a995a988f",
      "pn_pubkey_dec": "30364196998626412815632694656262367403557856434407168618533435834350416050423",
      "pn_seckey_hex": "9688c8fa9af045bca8024120dfb5440cb38f33bf919e3d768cd53eea5d77edc2",
      "pn_dih_dec": "76450313048673825143239733960146111988268472826174533377596497005692755387660",
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
      "pn_address": "0:ce0957b062e225af7b4d7f6ca5b629359e9462bd12b95d8c56e3f14aa9bf040c",
      "pn_pubkey_dec": "46908564887420113469330906518120089191421597309021559778332592679992144811662",
      "pn_seckey_hex": "84c300b749fede351178a6f1ec08abebf9f84c971a03451193a0fbf03039dd1b",
      "pn_dih_dec": "77234129791555341448663699467864057445659330270914183380492483871370076311309",
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
