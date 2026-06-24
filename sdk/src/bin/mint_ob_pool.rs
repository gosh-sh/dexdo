//! Pre-deploy a pool of "warmed" OrderBook markets and persist their
//! addresses + oracle/deployer keys to JSON. Each entry in the pool is a
//! market whose `OrderBook` is already Active on chain — tests/scripts
//! can grab one straight from JSON and start trading without paying the
//! oracle/event/PMP/freeze cost (~10–15 minutes per market) in the hot
//! path.
//!
//! Per-market lifecycle (mirrors `acki-nacki/tests/dex/orderbook_test.py`
//! phases 1–4 + the spec in `dodex_sdk/know_specs/`):
//!
//!   1. Deploy fresh oracle + EventList[0] + add 2-outcome event.
//!   2. Deploy a fresh deployer-`PrivateNote` (halo2 voucher → RootPN
//!      flow, same as `mint_pn_pool`) at N1000 nominal — enough for
//!      initial stakes + regular stakes + a 100 NACKL split collateral.
//!   3. `PN.deployPmp` with initial stakes on both outcomes; wait for
//!      `approved`.
//!   4. Oracle `submitSetTimings(resultStart = now + lifetime)`. The
//!      contract derives `stakeEnd = stakeStart + (resultStart -
//!      stakeStart) / 10`, fixing the bidding window at 10 % of total
//!      lifetime. Default `lifetime = 5h` ⇒ bidding ≈ 30 min, OrderBook
//!      live ≈ 4h30m.
//!   5. Deployer-PN `setStake` on outcome 0 + outcome 1 inside the
//!      bidding window (a few seconds in real time; we have ~30 min of
//!      headroom).
//!   6. Sleep until `now ≥ stakeEnd`.
//!   7. Deployer-PN `splitFullSet(SPLIT_COLLATERAL)` — triggers
//!      `_ensureFrozen()` on PMP which spawns the `OrderBook` and primes
//!      the deployer's outcome-token balance.
//!   8. Wait for the `OrderBook` account to become Active on chain.
//!   9. Append the market record to `ob_pool.json`.
//!
//! Output: `ob_pool.json` (default; override with `--output`). Re-running
//! against an existing file is **additive** — existing markets are
//! loaded, validated for endpoint compatibility, and `--count` more are
//! appended on top. Use `--output` to start a fresh pool. The file
//! contains oracle + deployer secret keys; treat as private (gitignore).
//!
//! Run: `cargo run --release --bin mint_ob_pool -- --count 2 --lifetime 5h`.
//! Release recommended — halo2 prover dominates wall-clock per market.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::giver::v3::send_currency_with_flag_from_default_giver;
use ackinacki_kit::contracts::giver::v3::top_up_native_with_giver_if_below;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::generate_random_sign_keys;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::oracle::Oracle;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::OracleEventList;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfSplitFullSet;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_contracts::dex::root_oracle::ParamsOfDeployOracle;
use dodex_contracts::dex::root_oracle::RootOracle;
use dodex_contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfGetPmpAddress;
use dodex_contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use dodex_contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::halo2::giver_voucher::mint_voucher_via_giver;
use dodex_sdk::halo2::Halo2Paths;
use dodex_sdk::proof;
use dodex_sdk::Dex;
use serde::Deserialize;
use serde::Serialize;

// ── Token / currency ───────────────────────────────────────────────
const CURRENCY_ID_NACKL: u32 = 1;
const CURRENCY_ID_SHELL: u32 = 2;
const TOKEN_TYPE_NACKL: u32 = 1;

// ── Deployer PN sizing (mirrors mint_pn_pool N1000) ────────────────
/// 1000 NACKL — enough for 2× initial stake (100 NACKL each) + 2× regular
/// stake (0.2 NACKL each) + 100 NACKL split collateral with margin.
const DEPLOYER_NOMINAL_LABEL: &str = "N1000";
const DEPLOYER_NOMINAL_RAW: u64 = 1_000_000_000_000;
/// SHELL gas voucher per deployer-PN (covers the orchestration messages).
const ECC_SHELL_DEPOSIT_RAW: u64 = 100_000_000_000;
/// Native vmshell top-up per deployer-PN.
const NATIVE_GAS_TOPUP_RAW: u64 = 20_000_000_000;

// ── Funding thresholds ─────────────────────────────────────────────
const ROOTPN_NATIVE_MIN: u64 = 120_000_000_000;
const ROOTPN_NATIVE_TOPUP: u64 = 50_000_000_000;
const ROOTPN_SHELL_BUDGET: u64 = 1_000_000_000_000;
const ROOTORACLE_NATIVE_MIN: u64 = 120_000_000_000;
const ROOTORACLE_NATIVE_TOPUP: u64 = 50_000_000_000;

// ── PMP constants ──────────────────────────────────────────────────
const ORACLE_FEE: u128 = 100;
const ORACLE_FEE_DEADLINE: u64 = 2_000_000_000;
/// Initial stake per outcome at `deployPMP`. 1 NACKL — mirrors upstream
/// `acki-nacki/tests/dex/orderbook_test.py::DEPLOYER_SEED_AMOUNT`.
const DEPLOYER_SEED_AMOUNT: u128 = 1_000_000_000;
/// Regular stake per outcome during bidding window. 20 NACKL — mirrors
/// `orderbook_test.py::STAKE_AMOUNT`. With seed=1 + regular=20 = 21 NACKL
/// per cleanPool, `21 % minInitialStake(1) == 0` → `_ensureFrozen` skips
/// the refund branch. Our previous values (100 + 0.2 → cleanPool=100.2,
/// `100.2 % 100 == 0.2`) triggered the refund branch and PMP.splitFullSet
/// aborted with compute exit_code=404 (no DEX ERR code matches; abort sits
/// between `_frozen=true` and `new OrderBook`).
const REGULAR_STAKE_AMOUNT: u128 = 20_000_000_000;
/// Collateral split into outcome tokens after freeze. 100 NACKL.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

// ── Lifetime constraints ───────────────────────────────────────────
const DEFAULT_LIFETIME_SECS: u64 = 5 * 60 * 60; // 5h
/// Floor on `lifetime`: gives at least ~30s of bidding window.
const MIN_LIFETIME_SECS: u64 = 5 * 60;
/// Ceiling: shellnet PMP/oracle state isn't designed for week-long
/// markets; clamp to 1 day to surface obvious typos.
const MAX_LIFETIME_SECS: u64 = 24 * 60 * 60;

// ── Args ───────────────────────────────────────────────────────────
#[derive(Debug)]
struct Args {
    count: usize,
    lifetime: Duration,
    output: PathBuf,
    endpoint: String,
    network_url: String,
    // When set, reuse pre-deployed PrivateNotes from this pn_pool.json as the
    // per-market deployer instead of minting a fresh halo2 voucher each time
    // (skips step 2/8's ZK proof + PN deploy). One note is consumed per market.
    deployer_pn_pool: Option<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut count: usize = 2;
        let mut lifetime = Duration::from_secs(DEFAULT_LIFETIME_SECS);
        let mut output = PathBuf::from("ob_pool.json");
        let mut endpoint = "shellnet.ackinacki.org".to_string();
        let mut deployer_pn_pool: Option<PathBuf> = None;

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--count" | "-n" => {
                    let v = argv.next().ok_or("--count requires a value")?;
                    count = v.parse().map_err(|e| format!("--count: {e}"))?;
                    if count == 0 {
                        return Err("--count must be ≥ 1".into());
                    }
                }
                "--lifetime" | "-l" => {
                    let v = argv.next().ok_or("--lifetime requires a value")?;
                    lifetime = parse_duration(&v)?;
                    let secs = lifetime.as_secs();
                    if secs < MIN_LIFETIME_SECS {
                        return Err(format!(
                            "--lifetime {v} is too short (minimum {MIN_LIFETIME_SECS}s = 5min)"
                        ));
                    }
                    if secs > MAX_LIFETIME_SECS {
                        return Err(format!(
                            "--lifetime {v} is too long (maximum {MAX_LIFETIME_SECS}s = 24h)"
                        ));
                    }
                }
                "--output" | "-o" => {
                    output = PathBuf::from(argv.next().ok_or("--output requires a value")?);
                }
                "--endpoint" | "-e" => {
                    endpoint = argv.next().ok_or("--endpoint requires a value")?;
                }
                "--deployer-pn-pool" => {
                    deployer_pn_pool = Some(PathBuf::from(
                        argv.next().ok_or("--deployer-pn-pool requires a path")?,
                    ));
                }
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown arg `{other}`\n\n{}", usage())),
            }
        }

        let network_url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.clone()
        } else {
            format!("https://{endpoint}")
        };

        Ok(Args { count, lifetime, output, endpoint, network_url, deployer_pn_pool })
    }
}

fn usage() -> String {
    "usage: mint_ob_pool [--count N] [--lifetime DUR] [--output path] [--endpoint host]\n\n  \
         --count     number of markets to deploy (default 2)\n  \
         --lifetime  total market lifetime, e.g. 5h / 90m / 1800s (default 5h, min 5m, max 24h)\n  \
         --output    JSON output path (default ./ob_pool.json)\n  \
         --endpoint  network host (default shellnet.ackinacki.org)\n  \
         --deployer-pn-pool PATH  reuse pre-deployed PNs from this pn_pool.json as \
         deployers (skips the halo2 voucher mint; consumes one note per market; \
         needs >= --count notes)\n\n\
         Bidding window = lifetime / 10 (contract-fixed). With default 5h: bidding ≈ 30min, \
         OrderBook live ≈ 4h30m."
        .to_string()
}

/// Parse strings like `"5h"`, `"90m"`, `"1800s"`, `"3600"` (raw seconds).
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num_str, unit_secs): (&str, u64) = if let Some(rest) = s.strip_suffix('h') {
        (rest, 3600)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, 1)
    } else {
        (s, 1)
    };
    let n: u64 = num_str.parse().map_err(|e| format!("parse duration `{s}`: {e}"))?;
    if n == 0 {
        return Err(format!("duration `{s}` must be > 0"));
    }
    Ok(Duration::from_secs(n * unit_secs))
}

// ── Persistence types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pool {
    endpoint: String,
    created_at_unix: u64,
    /// Lifetime used by the latest seeding run; informational only — each
    /// market record carries its own absolute timings.
    last_lifetime_secs: u64,
    deployer_nominal: String,
    deployer_raw_value: u64,
    token_type: u32,
    markets: Vec<PoolMarket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PoolMarket {
    // Oracle
    oracle_address: String,
    oracle_name: String,
    oracle_pubkey_hex: String,
    /// **Secret** — needed to drive `submitSetTimings` / `submitResolve` /
    /// `submitCancelEvent` from this seeder/test.
    oracle_secret_hex: String,
    oracle_list_address: String,
    event_id: String,
    outcome_names: HashMap<u32, String>,

    // Deployer PN (long-lived owner of the PMP)
    deployer_pn_address: String,
    deployer_pn_pubkey_hex: String,
    /// **Secret**.
    deployer_pn_secret_hex: String,
    deployer_deposit_identifier_hash: String,

    // Market addresses
    pmp_address: String,
    order_book_address: String,
    oracle_list_hash: String,
    /// Token type bound to this market (currently always NACKL = 1).
    /// Duplicated from `Pool.token_type` for ergonomic test access.
    token_type: u32,

    // Lifecycle timestamps (chain-authoritative)
    stake_start_unix: u64,
    stake_end_unix: u64,
    result_start_unix: u64,
    result_end_unix: u64,
    /// Wall-clock time we ran `splitFullSet` (≈ when OrderBook spawned).
    freeze_unix: u64,

    // Diagnostic / informational
    deployer_split_collateral: u128,
    lifetime_secs: u64,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

fn create_tvm_context(endpoint: &str) -> Arc<ClientContext> {
    let mut config = ClientConfig::default();
    config.network.endpoints = Some(vec![endpoint.to_string()]);
    // Match production: disable tvm_client's internal re-connect loop;
    // we don't have a retry policy layered around bin calls, but at
    // least we avoid turning a flaky shellnet into a self-amplifying storm.
    config.network.max_reconnect_timeout = 0;
    Arc::new(ClientContext::new(config).expect("create tvm client"))
}

fn save_pool(pool: &Pool, path: &Path) {
    let json = serde_json::to_string_pretty(pool).expect("serialize pool");
    std::fs::write(path, json).expect("write pool json");
}

fn load_or_init_pool(path: &Path, args: &Args) -> Result<Pool, String> {
    if !path.exists() {
        return Ok(Pool {
            endpoint: args.endpoint.clone(),
            created_at_unix: now_unix(),
            last_lifetime_secs: args.lifetime.as_secs(),
            deployer_nominal: DEPLOYER_NOMINAL_LABEL.to_string(),
            deployer_raw_value: DEPLOYER_NOMINAL_RAW,
            token_type: TOKEN_TYPE_NACKL,
            markets: Vec::with_capacity(args.count),
        });
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut existing: Pool = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse {} as Pool: {e}", path.display()))?;

    if existing.endpoint != args.endpoint {
        return Err(format!(
            "existing pool endpoint `{}` != requested `{}`; pass --output to a fresh path",
            existing.endpoint, args.endpoint,
        ));
    }
    if existing.token_type != TOKEN_TYPE_NACKL {
        return Err(format!(
            "existing pool token_type `{}` != expected `{TOKEN_TYPE_NACKL}`",
            existing.token_type,
        ));
    }
    if existing.deployer_nominal != DEPLOYER_NOMINAL_LABEL
        || existing.deployer_raw_value != DEPLOYER_NOMINAL_RAW
    {
        return Err(format!(
            "existing pool deployer nominal `{}/{}` != current `{DEPLOYER_NOMINAL_LABEL}/{DEPLOYER_NOMINAL_RAW}`",
            existing.deployer_nominal, existing.deployer_raw_value,
        ));
    }
    existing.last_lifetime_secs = args.lifetime.as_secs();
    Ok(existing)
}

// ── Pre-flight: top up RootOracle + RootPN ─────────────────────────

async fn ensure_root_oracle_funded(context: &Arc<ClientContext>) -> Result<(), String> {
    let root = RootOracle::new(context.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS));
    eprintln!("[ob-pool] waiting for RootOracle to be Active…");
    root.wait_account(ParamsOfWaitAccount {
        status: AccountStatus::Active,
        attempts: Some(60),
        attempts_timeout: Some(2_000),
    })
    .await
    .map_err(|e| format!("wait RootOracle active: {e:?}"))?;

    eprintln!("[ob-pool] topping up RootOracle native gas…");
    top_up_native_with_giver_if_below(
        context.clone(),
        &root,
        ROOTORACLE_NATIVE_MIN,
        ROOTORACLE_NATIVE_TOPUP,
        "RootOracle",
    )
    .await
    .map_err(|e| format!("top_up_native RootOracle: {e:?}"))?;
    Ok(())
}

async fn ensure_root_pn_funded(context: &Arc<ClientContext>) -> Result<(), String> {
    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    eprintln!("[ob-pool] waiting for RootPN to be Active…");
    root_pn
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(60),
            attempts_timeout: Some(2_000),
        })
        .await
        .map_err(|e| format!("wait RootPN active: {e:?}"))?;

    eprintln!("[ob-pool] topping up RootPN native gas…");
    top_up_native_with_giver_if_below(
        context.clone(),
        &root_pn,
        ROOTPN_NATIVE_MIN,
        ROOTPN_NATIVE_TOPUP,
        "RootPN",
    )
    .await
    .map_err(|e| format!("top_up_native RootPN: {e:?}"))?;

    eprintln!("[ob-pool] sending SHELL ECC budget to RootPN…");
    let mut ecc = HashMap::new();
    ecc.insert(CURRENCY_ID_SHELL, ROOTPN_SHELL_BUDGET);
    send_currency_with_flag_from_default_giver(
        context.clone(),
        RootPn::DEFAULT_ADDRESS,
        50_000_000_000,
        ecc,
        1,
    )
    .await
    .map_err(|e| format!("giver SHELL → RootPN: {e:?}"))?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    Ok(())
}

// ── PN deployment (mirrors mint_pn_pool::deploy_one_pn) ────────────

struct DeployedPn {
    address: String,
    deposit_identifier_hash_dec: String,
    keys: KeyPair,
}

async fn deploy_funded_deployer_pn(
    context: Arc<ClientContext>,
    network_url: &str,
    paths: &Halo2Paths,
) -> Result<DeployedPn, String> {
    let keys = generate_random_sign_keys(context.clone())
        .map_err(|e| format!("generate_random_sign_keys: {e:?}"))?;
    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));

    eprintln!("    halo2 NACKL deposit voucher…");
    let deposit_zk = mint_voucher_via_giver(
        context.clone(),
        network_url.to_string(),
        &keys.public,
        CURRENCY_ID_NACKL,
        DEPLOYER_NOMINAL_RAW,
        false,
        paths,
    )
    .await
    .map_err(|e| format!("mint_voucher_via_giver (deposit): {e:?}"))?;

    let dih_dec = proof::hex_u256_to_dec(&deposit_zk.deposit_identifier_hash_hex);
    let epk_dec = proof::pubkey_to_dec(&keys.public);

    eprintln!("    RootPN.deployPrivateNote…");
    root_pn
        .deploy_private_note(
            ParamsOfDeployPrivateNote {
                zkproof: deposit_zk.proof,
                deposit_identifier_hash: dih_dec.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &deposit_zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&deposit_zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&deposit_zk.token_type_fr_hex),
                ephemeral_pubkey: epk_dec,
                value: deposit_zk.voucher_value,
                token_type: deposit_zk.voucher_token_type,
                layer_number: deposit_zk.layer_number,
            },
            Signer::Keys { keys: keys.clone() },
        )
        .await
        .map_err(|e| format!("deploy_private_note: {e:?}"))?;

    let pn_address = root_pn
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: dih_dec.clone(),
        })
        .await
        .map_err(|e| format!("get_private_note_address: {e:?}"))?
        .private_note_address;

    let pn = PrivateNote::new(context.clone(), dex_contract_params(&pn_address));
    eprintln!("    waiting for PN {pn_address} Active…");
    pn.wait_account(ParamsOfWaitAccount {
        status: AccountStatus::Active,
        attempts: Some(60),
        attempts_timeout: Some(2_000),
    })
    .await
    .map_err(|e| format!("wait PN active: {e:?}"))?;

    eprintln!("    halo2 SHELL gas voucher…");
    let gas_zk = mint_voucher_via_giver(
        context.clone(),
        network_url.to_string(),
        &keys.public,
        CURRENCY_ID_SHELL,
        ECC_SHELL_DEPOSIT_RAW,
        true,
        paths,
    )
    .await
    .map_err(|e| format!("mint_voucher_via_giver (gas): {e:?}"))?;

    eprintln!("    RootPN.sendEccShellToPrivateNote…");
    root_pn
        .send_ecc_shell_to_private_note(
            ParamsOfSendEccShellToPrivateNote {
                proof: gas_zk.proof,
                nullifier_hash: proof::hex_u256_to_dec(&gas_zk.deposit_identifier_hash_hex),
                deposit_identifier_hash: dih_dec.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &gas_zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&gas_zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&gas_zk.token_type_fr_hex),
                value: gas_zk.voucher_value,
                layer_number: gas_zk.layer_number,
                recipient_ephemeral_pubkey: proof::pubkey_to_dec(&keys.public),
            },
            Signer::Keys { keys: keys.clone() },
        )
        .await
        .map_err(|e| format!("send_ecc_shell_to_private_note: {e:?}"))?;
    tokio::time::sleep(Duration::from_secs(5)).await;

    eprintln!("    giver native gas top-up…");
    send_currency_with_flag_from_default_giver(
        context.clone(),
        &pn_address,
        NATIVE_GAS_TOPUP_RAW,
        HashMap::new(),
        1,
    )
    .await
    .map_err(|e| format!("giver native top-up: {e:?}"))?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    Ok(DeployedPn { address: pn_address, deposit_identifier_hash_dec: dih_dec, keys })
}

// ── Deployer PN reuse (skip the halo2 voucher mint) ────────────────

/// A pre-deployed PrivateNote loaded from a `pn_pool.json` (the format
/// `mint_pn_pool` writes). Only the fields needed to drive the PN as a market
/// deployer are read; the rest are ignored.
#[derive(Deserialize, Clone)]
struct PnPoolNote {
    address: String,
    deposit_identifier_hash: String,
    owner_public_key_hex: String,
    owner_secret_key_hex: String,
}

#[derive(Deserialize)]
struct PnPoolFile {
    notes: Vec<PnPoolNote>,
}

fn load_pn_pool_notes(path: &std::path::Path) -> Result<Vec<PnPoolNote>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read pn_pool {}: {e}", path.display()))?;
    let file: PnPoolFile =
        serde_json::from_str(&raw).map_err(|e| format!("parse pn_pool {}: {e}", path.display()))?;
    Ok(file.notes)
}

/// Adopt a pn_pool note as the market deployer: top up its native gas from the
/// giver (it already holds NACKL + shell from `mint_pn_pool`) and hand back a
/// `DeployedPn` the rest of the flow drives exactly like a freshly-minted one.
async fn deployer_from_pool_note(
    context: Arc<ClientContext>,
    note: &PnPoolNote,
) -> Result<DeployedPn, String> {
    eprintln!("    reuse pn_pool note {} — giver native gas top-up…", note.address);
    send_currency_with_flag_from_default_giver(
        context.clone(),
        &note.address,
        NATIVE_GAS_TOPUP_RAW,
        HashMap::new(),
        1,
    )
    .await
    .map_err(|e| format!("giver native top-up (reuse): {e:?}"))?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    Ok(DeployedPn {
        address: note.address.clone(),
        deposit_identifier_hash_dec: note.deposit_identifier_hash.clone(),
        keys: KeyPair {
            public: note.owner_public_key_hex.clone(),
            secret: note.owner_secret_key_hex.clone(),
        },
    })
}

// ── Oracle + event setup ───────────────────────────────────────────

struct DeployedOracle {
    address: String,
    name: String,
    keys: KeyPair,
    event_list_address: String,
    event_id: String,
    outcome_names: HashMap<u32, String>,
}

async fn deploy_oracle_with_event(
    context: Arc<ClientContext>,
    dex: &Dex,
) -> Result<DeployedOracle, String> {
    let oracle_keys =
        generate_random_sign_keys(context.clone()).map_err(|e| format!("oracle keys: {e:?}"))?;
    let ephemeral_keys =
        generate_random_sign_keys(context.clone()).map_err(|e| format!("ephemeral keys: {e:?}"))?;
    let oracle_name = format!("BeeOB-{:x}", now_unix());

    dex.deploy_oracle(
        ParamsOfDeployOracle {
            oracle_pubkey: proof::pubkey_to_dec(&oracle_keys.public),
            oracle_name: oracle_name.clone(),
        },
        Signer::Keys { keys: ephemeral_keys },
    )
    .await
    .map_err(|e| format!("deploy_oracle: {e:?}"))?;

    let oracle_address = dex
        .get_oracle_address(oracle_name.clone())
        .await
        .map_err(|e| format!("get_oracle_address: {e:?}"))?;

    let oracle_handle = Oracle::new(context.clone(), dex_contract_params(&oracle_address));
    oracle_handle
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(60),
            attempts_timeout: Some(2_000),
        })
        .await
        .map_err(|e| format!("wait Oracle active: {e:?}"))?;

    let event_list_address = dex
        .get_event_list_address(&oracle_address, ParamsOfGetEventListAddress { index: 0 })
        .await
        .map_err(|e| format!("get_event_list_address: {e:?}"))?;
    let el_handle = OracleEventList::new(context, dex_contract_params(&event_list_address));
    el_handle
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(60),
            attempts_timeout: Some(2_000),
        })
        .await
        .map_err(|e| format!("wait EventList active: {e:?}"))?;

    let event_name = format!("BeeOBMatch_{:x}", now_unix());
    // addEvent requires dense 0-based outcome keys (`require(outcomeNames.exists(i))`
    // for i in 0..count); downstream setStake/splitFullSet index outcomes 0/1.
    let mut outcome_names = HashMap::new();
    outcome_names.insert(0_u32, "Team A".to_string());
    outcome_names.insert(1_u32, "Team B".to_string());

    dex.add_event(
        &event_list_address,
        ParamsOfAddEvent {
            event_name: event_name.clone(),
            oracle_fee: ORACLE_FEE,
            deadline: ORACLE_FEE_DEADLINE,
            describe: "OB seed".to_string(),
            outcome_names: outcome_names.clone(),
            trust_addr: None,
        },
        Signer::Keys { keys: oracle_keys.clone() },
    )
    .await
    .map_err(|e| format!("add_event: {e:?}"))?;

    let mut event_id = String::new();
    for _ in 0..30 {
        let events =
            dex.get_events(&event_list_address).await.map_err(|e| format!("get_events: {e:?}"))?;
        if let Some((id, _)) = events.events.iter().find(|(_, e)| {
            e.get("eventName").or_else(|| e.get("event_name")).and_then(|v| v.as_str())
                == Some(event_name.as_str())
        }) {
            event_id = id.clone();
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if event_id.is_empty() {
        return Err(format!("event `{event_name}` did not appear in EventList within 60s"));
    }

    Ok(DeployedOracle {
        address: oracle_address,
        name: oracle_name,
        keys: oracle_keys,
        event_list_address,
        event_id,
        outcome_names,
    })
}

// ── Per-market orchestration ───────────────────────────────────────

async fn deploy_one_market(
    context: Arc<ClientContext>,
    dex: &Dex,
    network_url: &str,
    paths: &Halo2Paths,
    lifetime: Duration,
    deployer_override: Option<&PnPoolNote>,
) -> Result<PoolMarket, String> {
    // 1. Oracle + event
    eprintln!("  [1/8] oracle + event…");
    let oracle = deploy_oracle_with_event(context.clone(), dex).await?;
    eprintln!(
        "        oracle={} name={} event_id={}",
        oracle.address, oracle.name, oracle.event_id
    );

    // 2. Deployer PN — reuse a pn_pool note when provided, else mint fresh.
    let deployer = match deployer_override {
        Some(note) => {
            eprintln!("  [2/8] deployer PN — reuse pn_pool note (skip halo2 voucher)…");
            deployer_from_pool_note(context.clone(), note).await?
        }
        None => {
            eprintln!("  [2/8] deployer PN ({DEPLOYER_NOMINAL_LABEL})…");
            deploy_funded_deployer_pn(context.clone(), network_url, paths).await?
        }
    };
    eprintln!("        pn={} dih={}", deployer.address, deployer.deposit_identifier_hash_dec);

    // 3. deployPMP + wait approved
    eprintln!("  [3/8] deployPMP + wait approval…");
    dex.deploy_pmp(
        &deployer.address,
        ParamsOfDeployPmp {
            event_id: oracle.event_id.clone(),
            oracle_fee: vec![ORACLE_FEE],
            token_type: TOKEN_TYPE_NACKL,
            names: vec![oracle.name.clone()],
            index: vec![0],
            initial_stakes: vec![DEPLOYER_SEED_AMOUNT, DEPLOYER_SEED_AMOUNT],
        },
        Signer::Keys { keys: deployer.keys.clone() },
    )
    .await
    .map_err(|e| format!("deploy_pmp: {e:?}"))?;
    tokio::time::sleep(Duration::from_secs(5)).await;

    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let pmp_address = root_pn
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id: oracle.event_id.clone(),
            names: vec![oracle.name.clone()],
            token_type: TOKEN_TYPE_NACKL,
        })
        .await
        .map_err(|e| format!("get_pmp_address: {e:?}"))?
        .pmp_address;

    let pmp_handle = Pmp::new(context.clone(), dex_contract_params(&pmp_address));
    pmp_handle
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(60),
            attempts_timeout: Some(2_000),
        })
        .await
        .map_err(|e| format!("wait PMP active: {e:?}"))?;

    // Wait for oracle quorum to land — this is the "approved by oracle"
    // stage, NOT the `approved` flag (the latter only flips to true after
    // `submitSetTimings`).
    let mut quorum_details = None;
    for _ in 0..40 {
        let d =
            dex.get_pmp_details(&pmp_address).await.map_err(|e| format!("pmp details: {e:?}"))?;
        if d.number_of_oracle_events > 0 && d.approved_oracle_events >= d.number_of_oracle_events {
            quorum_details = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let quorum = quorum_details.ok_or_else(|| {
        "PMP did not reach oracle quorum within 120s (approved_oracle_events < number_of_oracle_events)"
            .to_string()
    })?;
    // Settle a beat — sometimes the quorum-applied state needs a moment
    // before subsequent setTimings is accepted.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let oracle_list_hash = quorum.oracle_list_hash.clone();
    eprintln!(
        "        pmp={pmp_address} oracle_quorum={}/{} oracle_list_hash={oracle_list_hash}",
        quorum.approved_oracle_events, quorum.number_of_oracle_events,
    );

    // 4. submitSetTimings(resultStart = now + lifetime)
    eprintln!("  [4/8] submitSetTimings(resultStart=now+{}s)…", lifetime.as_secs());
    let result_start = now_unix() + lifetime.as_secs();
    dex.submit_set_timings(
        &pmp_address,
        ParamsOfSubmitSetTimings { result_start },
        Signer::Keys { keys: oracle.keys.clone() },
    )
    .await
    .map_err(|e| format!("submit_set_timings: {e:?}"))?;

    // Poll PMP details until contract has actually applied the timings.
    let mut pmp_with_timings = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let d =
            dex.get_pmp_details(&pmp_address).await.map_err(|e| format!("pmp details: {e:?}"))?;
        if d.stake_end > 0 && d.result_start > 0 {
            pmp_with_timings = Some(d);
            break;
        }
    }
    let timings =
        pmp_with_timings.ok_or_else(|| "PMP timings did not appear within 60s".to_string())?;
    eprintln!(
        "        stake_start={} stake_end={} result_start={} result_end={}",
        timings.stake_start, timings.stake_end, timings.result_start, timings.result_end
    );

    // 5. Two stakes inside [stake_start..stake_end]. Sequential, ~5–10s
    //    each — fits comfortably in the bidding window.
    eprintln!("  [5/8] setStake on outcome 0 + outcome 1…");
    for outcome in [0_u32, 1_u32] {
        dex.set_stake(
            &deployer.address,
            ParamsOfSetStake {
                event_id: oracle.event_id.clone(),
                oracle_list_hash: oracle_list_hash.clone(),
                token_type: TOKEN_TYPE_NACKL,
                outcome,
                amount: REGULAR_STAKE_AMOUNT,
                use_coupon: false,
            },
            Signer::Keys { keys: deployer.keys.clone() },
        )
        .await
        .map_err(|e| format!("set_stake outcome={outcome}: {e:?}"))?;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // 6. Wait until now ≥ stakeEnd. Add a small safety margin so the
    //    contract definitely sees us in the post-bidding window.
    let wait_secs = timings.stake_end.saturating_sub(now_unix()).saturating_add(5);
    eprintln!(
        "  [6/8] waiting {wait_secs}s for stake_end={} (now={})…",
        timings.stake_end,
        now_unix()
    );
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;

    // 7. splitFullSet — triggers `_ensureFrozen()` on PMP which spawns the
    //    OrderBook contract. Also primes the deployer's outcome-token
    //    balance, so a future test that needs a counterparty can use the
    //    deployer-PN itself for sanity scenarios.
    eprintln!("  [7/8] splitFullSet({SPLIT_COLLATERAL})…");
    let freeze_unix = now_unix();
    dex.split_full_set(
        &deployer.address,
        ParamsOfSplitFullSet {
            event_id: oracle.event_id.clone(),
            oracle_list_hash: oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
            collateral: SPLIT_COLLATERAL,
        },
        Signer::Keys { keys: deployer.keys.clone() },
    )
    .await
    .map_err(|e| format!("split_full_set: {e:?}"))?;

    // 8. Wait for OrderBook to become Active. PMP knows the address even
    //    before the contract code lands — poll until tvm reports Active.
    eprintln!("  [8/8] waiting for OrderBook Active…");
    let order_book_address = dex
        .get_order_book_address(&pmp_address)
        .await
        .map_err(|e| format!("get_order_book_address: {e:?}"))?;
    let ob_handle = dodex_contracts::dex::order_book::OrderBook::new(
        context.clone(),
        dex_contract_params(&order_book_address),
    );
    ob_handle
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(60),
            attempts_timeout: Some(3_000),
        })
        .await
        .map_err(|e| format!("wait OrderBook active: {e:?}"))?;

    let ob_details = dex
        .get_order_book_details(&order_book_address)
        .await
        .map_err(|e| format!("get_order_book_details: {e:?}"))?;
    eprintln!(
        "        ob={order_book_address} order_count={} next_order_id={}",
        ob_details.order_count, ob_details.next_order_id
    );

    Ok(PoolMarket {
        oracle_address: oracle.address,
        oracle_name: oracle.name,
        oracle_pubkey_hex: oracle.keys.public.clone(),
        oracle_secret_hex: oracle.keys.secret.clone(),
        oracle_list_address: oracle.event_list_address,
        event_id: oracle.event_id,
        outcome_names: oracle.outcome_names,

        deployer_pn_address: deployer.address,
        deployer_pn_pubkey_hex: deployer.keys.public.clone(),
        deployer_pn_secret_hex: deployer.keys.secret.clone(),
        deployer_deposit_identifier_hash: deployer.deposit_identifier_hash_dec,

        pmp_address,
        order_book_address,
        oracle_list_hash,
        token_type: TOKEN_TYPE_NACKL,

        stake_start_unix: timings.stake_start,
        stake_end_unix: timings.stake_end,
        result_start_unix: timings.result_start,
        result_end_unix: timings.result_end,
        freeze_unix,

        deployer_split_collateral: SPLIT_COLLATERAL,
        lifetime_secs: lifetime.as_secs(),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let bidding_secs = args.lifetime.as_secs() / 10;
    let trading_secs = args.lifetime.as_secs() - bidding_secs;
    eprintln!(
        "[ob-pool] minting {} market(s) on {} (lifetime={}s, bidding≈{}s, trading≈{}s)",
        args.count,
        args.endpoint,
        args.lifetime.as_secs(),
        bidding_secs,
        trading_secs,
    );
    eprintln!("[ob-pool] writing to {}", args.output.display());

    let context = create_tvm_context(&args.endpoint);
    let paths = Halo2Paths::from_env();
    if !paths.srs_exists() {
        eprintln!(
            "[ob-pool] SRS {} not found — generating it (~64 MB, one-time, CPU-bound)...",
            paths.srs_path().display()
        );
        paths.ensure_srs();
        eprintln!("[ob-pool] SRS ready at {}", paths.srs_path().display());
    }
    if let Err(e) = paths.validate() {
        eprintln!("[ob-pool] halo2 paths invalid: {e:?}");
        return ExitCode::FAILURE;
    }

    let dex = match Dex::new(dodex_sdk::DexConfig {
        endpoints: vec![args.endpoint.clone()],
        ..Default::default()
    }) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[ob-pool] create Dex: {e:?}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = ensure_root_oracle_funded(&context).await {
        eprintln!("[ob-pool] {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = ensure_root_pn_funded(&context).await {
        eprintln!("[ob-pool] {e}");
        return ExitCode::FAILURE;
    }

    let mut pool = match load_or_init_pool(&args.output, &args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[ob-pool] {e}");
            return ExitCode::FAILURE;
        }
    };
    let starting_count = pool.markets.len();
    if starting_count > 0 {
        eprintln!(
            "[ob-pool] loaded {} existing market(s); will append {} more (target {})",
            starting_count,
            args.count,
            starting_count + args.count,
        );
    }
    pool.markets.reserve(args.count);
    save_pool(&pool, &args.output);

    let deployer_notes = match &args.deployer_pn_pool {
        Some(path) => match load_pn_pool_notes(path) {
            Ok(notes) if notes.len() >= args.count => {
                eprintln!(
                    "[ob-pool] reusing {} pn_pool note(s) from {} as deployers \
                     (skipping halo2 voucher mint; one note consumed per market)",
                    args.count,
                    path.display(),
                );
                Some(notes)
            }
            Ok(notes) => {
                eprintln!(
                    "[ob-pool] pn_pool {} has {} note(s), --count needs {}",
                    path.display(),
                    notes.len(),
                    args.count,
                );
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("[ob-pool] {e}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let started = std::time::Instant::now();
    for i in 0..args.count {
        let t = std::time::Instant::now();
        eprintln!("\n[ob-pool] === market {}/{} ===", i + 1, args.count);

        match deploy_one_market(
            context.clone(),
            &dex,
            &args.network_url,
            &paths,
            args.lifetime,
            deployer_notes.as_ref().map(|n| &n[i]),
        )
        .await
        {
            Ok(market) => {
                eprintln!(
                    "[ob-pool] market {}/{} ready (pmp={}, ob={}) in {:.1}s (cumulative {:.1}s)",
                    i + 1,
                    args.count,
                    market.pmp_address,
                    market.order_book_address,
                    t.elapsed().as_secs_f64(),
                    started.elapsed().as_secs_f64(),
                );
                pool.markets.push(market);
                save_pool(&pool, &args.output);
            }
            Err(e) => {
                eprintln!(
                    "[ob-pool] market {}/{} FAILED after {:.1}s: {e}",
                    i + 1,
                    args.count,
                    t.elapsed().as_secs_f64(),
                );
                eprintln!(
                    "[ob-pool] partial pool ({} market(s)) saved to {}",
                    pool.markets.len(),
                    args.output.display(),
                );
                return ExitCode::FAILURE;
            }
        }
    }

    eprintln!(
        "\n[ob-pool] DONE — {} new market(s) deployed in {:.1}s; pool now has {} total, saved to {}",
        pool.markets.len() - starting_count,
        started.elapsed().as_secs_f64(),
        pool.markets.len(),
        args.output.display(),
    );
    ExitCode::SUCCESS
}
