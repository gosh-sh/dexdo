// Deploy an ephemeral PMP + OrderBook on shellnet for one e2e test
// run. Mirrors the orchestration in `bee_dex/src/bin/mint_ob_pool.rs`
// (steps 1, 3–8) but strips the halo2 voucher path used to fund a
// fresh deployer-PN — we take a PN out of `tests/fixtures/test_pns.json`
// which is already on chain with enough NACKL for the stakes + split
// collateral.
//
// After this helper returns, the OrderBook is Active on chain and the
// deployer-PN holds outcome-token balance from the `splitFullSet`
// step — so the same PN can both place and cancel orders against the
// freshly-spawned book.
//
// Wall-clock budget per call ≈ 2–3 minutes on shellnet (dominated by
// the `now ≥ stake_end` sleep ≈ lifetime/10). All polling loops have
// explicit attempt counts so a hung chain surfaces as a clean
// `anyhow::Error` rather than a deadline-timer expiration.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::dex::oracle::Oracle;
use ackinacki_kit::contracts::dex::oracle::ParamsOfGetEventListAddress;
use ackinacki_kit::contracts::dex::oracle_event_list::OracleEventList;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfAddEvent;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::pmp::Pmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfDeployPmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSetStake;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSplitFullSet;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfDeployOracle;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPmpAddress;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::generate_random_sign_keys;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::net::NetworkConfig;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::anyhow;
use anyhow::Context;
use bee_dex::Dex;
use num_bigint::BigUint;

use super::test_pns::TestPn;

// ── On-chain constants (mirror mint_ob_pool.rs) ────────────────────
//
// All amounts are NACKL raw units (10^-9 scale): 1 NACKL = 1_000_000_000.

const TOKEN_TYPE_NACKL: u32 = 1;
const ORACLE_FEE: u128 = 100;
const ORACLE_FEE_DEADLINE: u64 = 2_000_000_000;
/// Initial stake per outcome at `deployPMP`. 100 NACKL.
const DEPLOYER_SEED_AMOUNT: u128 = 100_000_000_000;
/// Regular stake per outcome inside the bidding window. 0.2 NACKL.
const REGULAR_STAKE_AMOUNT: u128 = 200_000_000;
/// Collateral split into outcome tokens after freeze. 100 NACKL — this
/// is also what primes the deployer-PN's outcome-token balance for
/// subsequent placeOrder / cancelOrder calls.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

/// Default lifetime — bidding window = lifetime / 10 (contract-fixed).
/// 10min keeps bidding at 60s, which comfortably fits the two
/// sequential `setStake` calls + a few seconds of safety margin.
/// Override per test via `DeployOptions::lifetime`.
pub const DEFAULT_LIFETIME_SECS: u64 = 10 * 60;

// ── Public surface ─────────────────────────────────────────────────

/// Configuration knobs for `deploy_ephemeral_market`. All fields are
/// `Option` so call sites can spell out only what they care about.
pub struct DeployOptions {
    /// Total market lifetime (from `submitSetTimings` to `result_end`).
    /// Defaults to [`DEFAULT_LIFETIME_SECS`].
    pub lifetime: Duration,
}

impl Default for DeployOptions {
    fn default() -> Self {
        Self { lifetime: Duration::from_secs(DEFAULT_LIFETIME_SECS) }
    }
}

/// Everything the e2e tests need to upsert the read-model row + dispatch
/// orders to chain. Mirrors `ObMarket` from the old fixture loader so
/// the existing `upsert_market` SQL stays untouched.
#[derive(Debug, Clone)]
pub struct EphemeralMarket {
    pub pmp_address: String,
    pub order_book_address: String,
    /// uint256 decimal string — `mint_ob_pool` writes this format too.
    pub event_id: String,
    /// uint256 decimal string.
    pub oracle_list_hash: String,
    pub token_type: u32,
    /// Outcome 1 ("Team A") name — the test only ever trades one side.
    pub outcome_name: String,
    pub outcome_id: u32,
    pub stake_start_unix: i64,
    pub stake_end_unix: i64,
    pub result_start_unix: i64,
    pub result_end_unix: i64,
    pub freeze_unix: i64,
}

/// Deploy a fresh oracle → event → PMP → OrderBook against `endpoints`,
/// signed under the deployer-PN keys. Returns once the OrderBook
/// account is Active on chain and ready to accept `placeOrder`.
///
/// `endpoints` is the same shape `BeeDexChainSender::new` takes; pass
/// the same shellnet endpoint the test handler uses so both calls hit
/// the same network.
pub async fn deploy_ephemeral_market(
    endpoints: Vec<String>,
    deployer: &TestPn,
    options: DeployOptions,
) -> anyhow::Result<EphemeralMarket> {
    let dex = Dex::new(endpoints.clone()).map_err(|e| anyhow!("Dex::new: {e:?}"))?;
    let context = build_context(endpoints).context("ClientContext::new")?;

    // 1. Oracle + EventList[0] + add 2-outcome event.
    eprintln!("[deploy_market] 1/8 oracle + event…");
    let oracle = deploy_oracle_with_event(&dex, context.clone()).await?;
    eprintln!("[deploy_market]     oracle={} event_id={}", oracle.address, oracle.event_id);

    let deployer_keys = KeyPair {
        public: deployer.owner_public_key_hex.clone(),
        secret: deployer.owner_secret_key_hex.clone(),
    };
    let deployer_signer = || Signer::Keys { keys: deployer_keys.clone() };

    // 2. deployPMP + wait Active + wait oracle quorum.
    eprintln!("[deploy_market] 2/8 deployPMP + wait approval…");
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
        deployer_signer(),
    )
    .await
    .map_err(|e| anyhow!("deploy_pmp: {e:?}"))?;
    tokio::time::sleep(Duration::from_secs(5)).await;

    let root_pn = RootPn::new_default(context.clone());
    let pmp_address = root_pn
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id: oracle.event_id.clone(),
            names: vec![oracle.name.clone()],
            token_type: TOKEN_TYPE_NACKL,
        })
        .await
        .map_err(|e| anyhow!("get_pmp_address: {e:?}"))?
        .pmp_address;
    wait_active(Pmp::new(context.clone(), &pmp_address), 60, 2_000, "PMP").await?;

    // Oracle quorum lands a few seconds after deployPMP — poll PMP
    // details until `approved_oracle_events == number_of_oracle_events`.
    let oracle_list_hash = wait_oracle_quorum(&dex, &pmp_address).await?;
    // Settle a beat so subsequent submitSetTimings is accepted (matches
    // the 5s sleep in mint_ob_pool — empirical, not contract-required).
    tokio::time::sleep(Duration::from_secs(5)).await;
    eprintln!("[deploy_market]     pmp={pmp_address} oracle_list_hash={oracle_list_hash}");

    // 3. submitSetTimings(resultStart = now + lifetime).
    eprintln!(
        "[deploy_market] 3/8 submitSetTimings(resultStart=now+{}s)…",
        options.lifetime.as_secs()
    );
    let result_start = now_unix() + options.lifetime.as_secs();
    dex.submit_set_timings(
        &pmp_address,
        ParamsOfSubmitSetTimings { result_start },
        Signer::Keys { keys: oracle.keys.clone() },
    )
    .await
    .map_err(|e| anyhow!("submit_set_timings: {e:?}"))?;
    let timings = wait_pmp_timings(&dex, &pmp_address).await?;
    eprintln!(
        "[deploy_market]     stake_start={} stake_end={} result_start={} result_end={}",
        timings.stake_start, timings.stake_end, timings.result_start, timings.result_end
    );

    // 4. setStake on outcomes 0 and 1 (sequential, ~5–10s apart).
    eprintln!("[deploy_market] 4/8 setStake on outcomes 0 + 1…");
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
            deployer_signer(),
        )
        .await
        .map_err(|e| anyhow!("set_stake outcome={outcome}: {e:?}"))?;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // 5. Wait until `now ≥ stake_end` + 5s safety margin.
    let wait_secs = (timings.stake_end as u64).saturating_sub(now_unix()).saturating_add(5);
    eprintln!(
        "[deploy_market] 5/8 waiting {wait_secs}s for stake_end={} (now={})…",
        timings.stake_end,
        now_unix()
    );
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;

    // 6. splitFullSet → primes deployer's outcome-token balance AND
    //    triggers `_ensureFrozen()` on PMP which spawns the OrderBook.
    eprintln!("[deploy_market] 6/8 splitFullSet({SPLIT_COLLATERAL})…");
    let freeze_unix = now_unix() as i64;
    dex.split_full_set(
        &deployer.address,
        ParamsOfSplitFullSet {
            event_id: oracle.event_id.clone(),
            oracle_list_hash: oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
            collateral: SPLIT_COLLATERAL,
        },
        deployer_signer(),
    )
    .await
    .map_err(|e| anyhow!("split_full_set: {e:?}"))?;

    // 7. Wait for OrderBook account to become Active.
    eprintln!("[deploy_market] 7/8 waiting for OrderBook Active…");
    let order_book_address = dex
        .get_order_book_address(&pmp_address)
        .await
        .map_err(|e| anyhow!("get_order_book_address: {e:?}"))?;
    let ob = ackinacki_kit::contracts::dex::order_book::OrderBook::new(
        context.clone(),
        &order_book_address,
    );
    wait_active(ob, 60, 3_000, "OrderBook").await?;

    // 8. Convert numeric fields to the format `upsert_market` expects.
    //    `event_id` and `oracle_list_hash` come back from PMP as decimal
    //    `numeric(78,0)` strings; the old fixture stored them as `0x…`
    //    hex. We surface the decimal form and let the caller's
    //    `hex_to_decimal` helper be a no-op (it already accepts both
    //    `0x`-prefixed hex and pure decimal via `BigUint::parse_bytes`,
    //    but only the hex path was exercised by the JSON fixture).
    let outcome_name =
        oracle.outcome_names.get(&1).cloned().unwrap_or_else(|| "Team A".to_string());
    eprintln!("[deploy_market] 8/8 ok: ob={order_book_address} outcome=\"{outcome_name}\"");
    Ok(EphemeralMarket {
        pmp_address,
        order_book_address,
        event_id: oracle.event_id,
        oracle_list_hash,
        token_type: TOKEN_TYPE_NACKL,
        outcome_name,
        outcome_id: 1,
        stake_start_unix: timings.stake_start,
        stake_end_unix: timings.stake_end,
        result_start_unix: timings.result_start,
        result_end_unix: timings.result_end,
        freeze_unix,
    })
}

// ── Internals ──────────────────────────────────────────────────────

struct DeployedOracle {
    address: String,
    name: String,
    keys: KeyPair,
    event_list_address: String,
    event_id: String,
    outcome_names: HashMap<u32, String>,
}

async fn deploy_oracle_with_event(
    dex: &Dex,
    context: Arc<ClientContext>,
) -> anyhow::Result<DeployedOracle> {
    let oracle_keys =
        generate_random_sign_keys(context.clone()).map_err(|e| anyhow!("oracle keys: {e:?}"))?;
    let ephemeral_keys =
        generate_random_sign_keys(context.clone()).map_err(|e| anyhow!("ephemeral keys: {e:?}"))?;
    let oracle_name = format!("DodexE2E-{:x}", now_unix());

    dex.deploy_oracle(
        ParamsOfDeployOracle {
            oracle_pubkey: pubkey_hex_to_decimal(&oracle_keys.public),
            oracle_name: oracle_name.clone(),
        },
        Signer::Keys { keys: ephemeral_keys },
    )
    .await
    .map_err(|e| anyhow!("deploy_oracle: {e:?}"))?;

    let oracle_address = dex
        .get_oracle_address(oracle_name.clone())
        .await
        .map_err(|e| anyhow!("get_oracle_address: {e:?}"))?;
    wait_active(Oracle::new(context.clone(), &oracle_address), 60, 2_000, "Oracle").await?;

    let event_list_address = dex
        .get_event_list_address(&oracle_address, ParamsOfGetEventListAddress { index: 0 })
        .await
        .map_err(|e| anyhow!("get_event_list_address: {e:?}"))?;
    wait_active(OracleEventList::new(context.clone(), &event_list_address), 60, 2_000, "EventList")
        .await?;

    let event_name = format!("DodexE2EMatch_{:x}", now_unix());
    let mut outcome_names = HashMap::new();
    outcome_names.insert(1_u32, "Team A".to_string());
    outcome_names.insert(2_u32, "Team B".to_string());

    dex.add_event(
        &event_list_address,
        ParamsOfAddEvent {
            event_name: event_name.clone(),
            oracle_fee: ORACLE_FEE,
            deadline: ORACLE_FEE_DEADLINE,
            describe: "dodex e2e seed".to_string(),
            outcome_names: outcome_names.clone(),
            trust_addr: None,
        },
        Signer::Keys { keys: oracle_keys.clone() },
    )
    .await
    .map_err(|e| anyhow!("add_event: {e:?}"))?;

    // Poll EventList until `addEvent` shows up; the chain emits a
    // record into the `_events` map asynchronously. 30 × 2s = 60s
    // budget — same as `mint_ob_pool`.
    let mut event_id = String::new();
    for _ in 0..30 {
        let events =
            dex.get_events(&event_list_address).await.map_err(|e| anyhow!("get_events: {e:?}"))?;
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
        return Err(anyhow!("event `{event_name}` did not appear in EventList within 60s"));
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

struct AppliedTimings {
    stake_start: i64,
    stake_end: i64,
    result_start: i64,
    result_end: i64,
}

async fn wait_oracle_quorum(dex: &Dex, pmp_address: &str) -> anyhow::Result<String> {
    // 40 × 3s = 120s budget. Quorum normally lands within ~30s; the
    // wider budget covers transient gateway slowness on shellnet.
    for _ in 0..40 {
        let d = dex
            .get_pmp_details(pmp_address)
            .await
            .map_err(|e| anyhow!("get_pmp_details: {e:?}"))?;
        if d.number_of_oracle_events > 0 && d.approved_oracle_events >= d.number_of_oracle_events {
            return Ok(d.oracle_list_hash);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(anyhow!(
        "PMP did not reach oracle quorum within 120s (approved_oracle_events < number_of_oracle_events)"
    ))
}

async fn wait_pmp_timings(dex: &Dex, pmp_address: &str) -> anyhow::Result<AppliedTimings> {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let d = dex
            .get_pmp_details(pmp_address)
            .await
            .map_err(|e| anyhow!("get_pmp_details: {e:?}"))?;
        if d.stake_end > 0 && d.result_start > 0 {
            return Ok(AppliedTimings {
                stake_start: d.stake_start as i64,
                stake_end: d.stake_end as i64,
                result_start: d.result_start as i64,
                result_end: d.result_end as i64,
            });
        }
    }
    Err(anyhow!("PMP timings did not appear within 60s after submit_set_timings"))
}

async fn wait_active<A: AccountAccessor>(
    handle: A,
    attempts: u8,
    interval_ms: u64,
    label: &str,
) -> anyhow::Result<()> {
    handle
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(attempts),
            attempts_timeout: Some(interval_ms),
        })
        .await
        .map_err(|e| anyhow!("wait {label} active: {e:?}"))?;
    Ok(())
}

fn build_context(endpoints: Vec<String>) -> anyhow::Result<Arc<ClientContext>> {
    let network = NetworkConfig { endpoints: Some(endpoints), ..Default::default() };
    let config = ClientConfig { network, ..Default::default() };
    let ctx = ClientContext::new(config).map_err(|e| anyhow!("ClientContext::new: {e:?}"))?;
    Ok(Arc::new(ctx))
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Convert a `0x…` hex pubkey (the `KeyPair.public` format) into the
/// decimal uint256 string the contracts expect. `mint_ob_pool` uses
/// `bee_dex::proof::pubkey_to_dec`; we reproduce its behaviour here
/// without taking on `bee_dex::proof` as a test-side dependency.
fn pubkey_hex_to_decimal(hex: &str) -> String {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    BigUint::parse_bytes(stripped.as_bytes(), 16)
        .unwrap_or_else(|| panic!("invalid pubkey hex: {hex}"))
        .to_str_radix(10)
}
