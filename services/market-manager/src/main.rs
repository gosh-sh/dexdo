//! Staging-only market manager. Keeps a small pool of live Acki Nacki
//! prediction-market OrderBooks on shellnet by cycling through a curated
//! event list. Each tick: tops up seeder PrivateNotes from the default
//! giver, advances in-flight markets through their lifecycle
//! (`PendingFreeze` → `Active` → `PendingResolve` → `Resolved`), and
//! adds new markets up to `target_active_markets`.
//!
//! Not deployed to prod. Lives entirely inside `services/market-manager`
//! (standalone Cargo workspace) so it cannot leak into the api/indexer
//! build pipeline.

use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfAddEvent;
use ackinacki_kit::contracts::dex::order_book::OrderBook;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::private_note::ParamsOfDeployPmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSetStake;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSplitFullSet;
use ackinacki_kit::contracts::giver::v3::send_currency_with_flag_from_default_giver;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::Context as _;
use anyhow::Result;
use num_bigint::BigInt;
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use tracing::error;
use tracing::info;
use tracing::warn;

mod dex;
use dex::Dex;
use dex::PmpDetails;

// =========================================================================
// ECC currency ids — match the chain's `tokenType` enum.
// =========================================================================

const CURRENCY_NACKL: u32 = 1;
const CURRENCY_SHELL: u32 = 2;
const CURRENCY_USDC: u32 = 3;

// =========================================================================
// Config — YAML pointed at by APP_CONFIG.
// =========================================================================

#[derive(Debug, Clone, Deserialize)]
struct Config {
    endpoint: String,
    tick_interval_secs: u64,
    target_active_markets: usize,
    /// Per-market lifetime is a random integer in [min..=max] hours. Bidding
    /// window is contract-fixed at lifetime / 10.
    lifetime_hours_min: u64,
    lifetime_hours_max: u64,
    state_file: PathBuf,
    events_file: PathBuf,
    secrets_file: PathBuf,
    market: MarketConfig,
    topup: TopupConfig,
    traders: TraderConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct MarketConfig {
    /// ECC currency id. MUST match the seeder PNs' token type — a USDC
    /// PN cannot deploy a NACKL PMP and vice versa.
    token_type: u32,
    /// Per-outcome stake at deployPMP (raw token units).
    deployer_seed_amount: u128,
    /// Per-outcome stake during the bidding window (raw token units).
    regular_stake_amount: u128,
    /// Collateral split into outcome tokens after freeze (raw token units).
    split_collateral: u128,
    oracle_fee: u128,
    oracle_fee_deadline: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct TopupConfig {
    native_min: u64,
    native_topup: u64,
    nackl_min: u64,
    nackl_topup: u64,
    shell_min: u64,
    shell_topup: u64,
    usdc_min: u64,
    usdc_topup: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct TraderConfig {
    /// Per tick, with this probability, one random PN places one
    /// random-outcome `setStake` on one random in-bidding market.
    stake_probability_per_tick: f64,
    stake_amount: u128,
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config = serde_yaml::from_slice(&bytes)
            .with_context(|| format!("parse config {}", path.display()))?;
        anyhow::ensure!(cfg.lifetime_hours_min >= 1, "lifetime_hours_min must be >= 1");
        anyhow::ensure!(
            cfg.lifetime_hours_max >= cfg.lifetime_hours_min,
            "lifetime_hours_max must be >= lifetime_hours_min"
        );
        anyhow::ensure!(cfg.target_active_markets >= 1, "target_active_markets must be >= 1");
        anyhow::ensure!(cfg.tick_interval_secs >= 5, "tick_interval_secs must be >= 5");
        anyhow::ensure!(
            matches!(cfg.market.token_type, CURRENCY_NACKL | CURRENCY_SHELL | CURRENCY_USDC),
            "market.token_type must be 1 (NACKL), 2 (SHELL) or 3 (USDC)"
        );
        Ok(cfg)
    }
}

// =========================================================================
// Secrets — testnet PN + oracle keypairs, committed to repo for the
// staging deployment per project decision (no real money on shellnet).
// =========================================================================

#[derive(Debug, Clone, Deserialize)]
struct Secrets {
    oracle: OracleSecret,
    /// Each market deploy picks one at random as deployer. Trader stakes
    /// also pick from this pool. No role assignment.
    private_notes: Vec<PnSecret>,
}

#[derive(Debug, Clone, Deserialize)]
struct OracleSecret {
    address: String,
    name: String,
    pubkey_hex: String,
    secret_hex: String,
    event_list_address: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PnSecret {
    address: String,
    pubkey_hex: String,
    secret_hex: String,
}

impl Secrets {
    fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("read secrets {}", path.display()))?;
        let s: Secrets = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse secrets {}", path.display()))?;
        anyhow::ensure!(!s.private_notes.is_empty(), "secrets.private_notes is empty");
        Ok(s)
    }
}

fn keypair_of(pubkey_hex: &str, secret_hex: &str) -> KeyPair {
    KeyPair { public: pubkey_hex.to_string(), secret: secret_hex.to_string() }
}

// =========================================================================
// Events — cyclic list. The cursor suffix on the on-chain `event_name`
// prevents collisions in the OracleEventList when we wrap around.
// =========================================================================

#[derive(Debug, Clone, Deserialize)]
struct EventsFile {
    events: Vec<EventDef>,
}

#[derive(Debug, Clone, Deserialize)]
struct EventDef {
    name: String,
    describe: Option<String>,
    /// Exactly two outcomes (label only — index 0 / 1).
    outcomes: [String; 2],
}

impl EventsFile {
    fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("read events {}", path.display()))?;
        let f: EventsFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse events {}", path.display()))?;
        anyhow::ensure!(!f.events.is_empty(), "events list is empty");
        Ok(f)
    }
}

// =========================================================================
// State — JSON on a mounted volume. Single writer (us).
// =========================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    /// Monotonic cursor — selects which event from the cyclic list to use
    /// next via `cursor % events.len()`. Uniqueness of the on-chain event
    /// name is handled separately by a unix-timestamp suffix at deploy
    /// time (see `Ctx::next_event`), so a state-loss reboot doesn't
    /// collide with historical EventList entries.
    next_event_cursor: u64,
    markets: Vec<MarketRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MarketRecord {
    event_id: String,
    event_name: String,
    /// The PN we chose at deploy time. Must drive `splitFullSet` from
    /// the same address — trader stakes can come from any PN.
    deployer_pn_address: String,
    pmp_address: String,
    /// `None` until status >= Active.
    order_book_address: Option<String>,
    oracle_list_hash: String,
    token_type: u32,
    stake_start_unix: u64,
    stake_end_unix: u64,
    result_start_unix: u64,
    result_end_unix: u64,
    lifetime_hours: u64,
    status: MarketStatus,
    /// Set when status == Resolved.
    resolved_outcome: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MarketStatus {
    /// PMP up, initial+regular stakes placed, waiting for `stake_end` so
    /// we can splitFullSet.
    PendingFreeze,
    /// splitFullSet done, OrderBook live. Trader PNs may stake on it.
    Active,
    /// `result_end_unix <= now`. Awaiting our `submitResolve`.
    PendingResolve,
    /// Terminal. Kept in state for history / debugging.
    Resolved,
}

impl State {
    fn load_or_init(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("read state {}", path.display()))?;
        let s: State = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse state {}", path.display()))?;
        Ok(s)
    }

    /// Atomic write — tmp file + rename. State is canonical for what's
    /// live on chain; a corrupt save orphans in-flight markets.
    fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("serialize state")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    fn active_or_pending_freeze_count(&self) -> usize {
        self.markets
            .iter()
            .filter(|m| matches!(m.status, MarketStatus::PendingFreeze | MarketStatus::Active))
            .count()
    }
}

// =========================================================================
// Ctx — long-lived runtime state assembled at startup.
// =========================================================================

struct Ctx {
    cfg: Config,
    secrets: Secrets,
    events: EventsFile,
    context: Arc<ClientContext>,
    dex: Dex,
    oracle_keys: KeyPair,
    /// Parallel to `secrets.private_notes`, same order. Cached to avoid
    /// re-allocating KeyPair { public, secret } on every chain call.
    pn_keys: Vec<KeyPair>,
}

impl Ctx {
    fn pick_random_pn(&self) -> (&PnSecret, &KeyPair) {
        let mut rng = rand::thread_rng();
        let idx = rng.gen_range(0..self.secrets.private_notes.len());
        (&self.secrets.private_notes[idx], &self.pn_keys[idx])
    }

    fn next_event(&self, cursor: u64) -> (String, &EventDef) {
        let event = &self.events.events[(cursor as usize) % self.events.events.len()];
        // Suffix is deploy-time unix-seconds. Tick cadence is 60s and we
        // deploy at most one market per tick, so seconds resolution is
        // plenty unique. Crucially: a state-loss reboot picks a fresh
        // timestamp instead of restarting cursor from 0 — so we never
        // collide with the OracleEventList entries from prior runs.
        let on_chain_name = format!("{}#{}", event.name, now_unix());
        (on_chain_name, event)
    }
}

// =========================================================================
// Tick loop.
// =========================================================================

async fn tick(ctx: &Ctx, state: &mut State, state_path: &Path) -> Result<()> {
    // 1. Top up PN balances from the default giver (no halo2 — giver can
    //    push native + ECC 1/2/3 directly to any address).
    if let Err(e) = refill_pns(ctx).await {
        warn!(error = ?e, "refill_pns failed; continuing");
    }

    // 2. PendingFreeze → Active for markets whose stake_end has passed.
    if let Err(e) = advance_pending_freeze(ctx, state, state_path).await {
        warn!(error = ?e, "advance_pending_freeze failed; continuing");
    }

    // 3. Active → PendingResolve for markets past result_end (state-only
    //    flip; the actual oracle submit happens in step 4 so the flag
    //    survives a crash between the two steps).
    flag_expired_for_resolve(state);

    // 4. PendingResolve → Resolved via submitResolve(random outcome).
    if let Err(e) = resolve_pending(ctx, state, state_path).await {
        warn!(error = ?e, "resolve_pending failed; continuing");
    }

    // 5. Top off the active pool — at most ONE new market per tick.
    if state.active_or_pending_freeze_count() < ctx.cfg.target_active_markets {
        if let Err(e) = deploy_one_market(ctx, state, state_path).await {
            warn!(error = ?e, "deploy_one_market failed; will retry next tick");
        }
    }

    // 6. Background trader activity — random setStake in the bidding window.
    if let Err(e) = maybe_trader_stake(ctx, state).await {
        warn!(error = ?e, "maybe_trader_stake failed; continuing");
    }

    Ok(())
}

// --- step 1: refill PNs --------------------------------------------------

async fn refill_pns(ctx: &Ctx) -> Result<()> {
    for pn in &ctx.secrets.private_notes {
        let mut acc = Account::new(ctx.context.clone(), &pn.address);
        if let Err(e) = acc.fetch().await {
            warn!(address = %pn.address, error = ?e, "fetch account failed; skip refill");
            continue;
        }

        let native = acc.balance.as_ref().map(big_to_u64).unwrap_or(0);
        let nackl = acc.ecc.get(&CURRENCY_NACKL).map(big_to_u64).unwrap_or(0);
        let shell = acc.ecc.get(&CURRENCY_SHELL).map(big_to_u64).unwrap_or(0);
        let usdc = acc.ecc.get(&CURRENCY_USDC).map(big_to_u64).unwrap_or(0);

        let mut native_topup = 0u64;
        let mut ecc_topup: HashMap<u32, u64> = HashMap::new();

        if ctx.cfg.topup.native_min > 0 && native < ctx.cfg.topup.native_min {
            native_topup = ctx.cfg.topup.native_topup;
        }
        if ctx.cfg.topup.nackl_min > 0 && nackl < ctx.cfg.topup.nackl_min {
            ecc_topup.insert(CURRENCY_NACKL, ctx.cfg.topup.nackl_topup);
        }
        if ctx.cfg.topup.shell_min > 0 && shell < ctx.cfg.topup.shell_min {
            ecc_topup.insert(CURRENCY_SHELL, ctx.cfg.topup.shell_topup);
        }
        if ctx.cfg.topup.usdc_min > 0 && usdc < ctx.cfg.topup.usdc_min {
            ecc_topup.insert(CURRENCY_USDC, ctx.cfg.topup.usdc_topup);
        }

        if native_topup == 0 && ecc_topup.is_empty() {
            continue;
        }

        info!(
            address = %pn.address,
            native, nackl, shell, usdc,
            native_topup, ecc_topup_keys = ?ecc_topup.keys().collect::<Vec<_>>(),
            "refilling PN",
        );

        // Flag=1 mirrors what bee-engine uses for giver pushes — credits
        // the destination without engaging contract code paths that
        // assume a specific call shape.
        if let Err(e) = send_currency_with_flag_from_default_giver(
            ctx.context.clone(),
            &pn.address,
            native_topup,
            ecc_topup,
            1,
        )
        .await
        {
            warn!(address = %pn.address, error = ?e, "giver send failed");
        }
        // Give the chain a beat between PNs so we don't pile messages.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

// --- step 2: advance PendingFreeze → Active ------------------------------

async fn advance_pending_freeze(ctx: &Ctx, state: &mut State, state_path: &Path) -> Result<()> {
    let now = now_unix();
    // Collect indices first to avoid borrowing state.markets while
    // mutating it inside the loop body.
    let pending: Vec<usize> = state
        .markets
        .iter()
        .enumerate()
        .filter(|(_, m)| m.status == MarketStatus::PendingFreeze && m.stake_end_unix <= now)
        .map(|(i, _)| i)
        .collect();

    for idx in pending {
        let m = state.markets[idx].clone();
        info!(
            pmp = %m.pmp_address,
            event = %m.event_name,
            "splitFullSet — freezing PMP and spawning OrderBook",
        );

        // Find the matching PN keys for the deployer address. If absent
        // (someone edited secrets between runs) we cannot drive this
        // market — log and leave it for manual intervention.
        let Some(deployer_keys) = pn_keys_for(ctx, &m.deployer_pn_address) else {
            warn!(
                pmp = %m.pmp_address,
                deployer = %m.deployer_pn_address,
                "deployer PN no longer in secrets — cannot splitFullSet, leaving in PendingFreeze",
            );
            continue;
        };

        let split_params = ParamsOfSplitFullSet {
            event_id: m.event_id.clone(),
            oracle_list_hash: m.oracle_list_hash.clone(),
            token_type: m.token_type,
            collateral: ctx.cfg.market.split_collateral,
        };

        if let Err(e) = ctx
            .dex
            .split_full_set(
                &m.deployer_pn_address,
                split_params,
                Signer::Keys { keys: deployer_keys.clone() },
            )
            .await
        {
            warn!(pmp = %m.pmp_address, error = ?e, "split_full_set failed");
            continue;
        }

        let ob_address = match ctx.dex.get_order_book_address(&m.pmp_address).await {
            Ok(addr) => addr,
            Err(e) => {
                warn!(pmp = %m.pmp_address, error = ?e, "get_order_book_address failed");
                continue;
            }
        };

        let ob_handle = OrderBook::new(ctx.context.clone(), &ob_address);
        if let Err(e) = ob_handle
            .wait_account(ParamsOfWaitAccount {
                status: AccountStatus::Active,
                attempts: Some(60),
                attempts_timeout: Some(3_000),
            })
            .await
        {
            warn!(ob = %ob_address, error = ?e, "OrderBook did not become Active");
            continue;
        }

        state.markets[idx].order_book_address = Some(ob_address.clone());
        state.markets[idx].status = MarketStatus::Active;
        state.save(state_path)?;
        info!(pmp = %m.pmp_address, ob = %ob_address, "market is now Active");
    }
    Ok(())
}

// --- step 3: Active → PendingResolve (state only) ------------------------

fn flag_expired_for_resolve(state: &mut State) {
    let now = now_unix();
    for m in &mut state.markets {
        if m.status == MarketStatus::Active && m.result_end_unix <= now {
            m.status = MarketStatus::PendingResolve;
        }
    }
}

// --- step 4: PendingResolve → Resolved -----------------------------------

async fn resolve_pending(ctx: &Ctx, state: &mut State, state_path: &Path) -> Result<()> {
    let pending: Vec<usize> = state
        .markets
        .iter()
        .enumerate()
        .filter(|(_, m)| m.status == MarketStatus::PendingResolve)
        .map(|(i, _)| i)
        .collect();

    for idx in pending {
        let pmp = state.markets[idx].pmp_address.clone();
        let outcome = rand::thread_rng().gen_range(0..2u32);

        info!(pmp = %pmp, outcome, "submitResolve");
        if let Err(e) = ctx
            .dex
            .submit_resolve(
                &pmp,
                ParamsOfSubmitResolve { outcome_id: outcome },
                Signer::Keys { keys: ctx.oracle_keys.clone() },
            )
            .await
        {
            warn!(pmp = %pmp, error = ?e, "submit_resolve failed");
            continue;
        }

        state.markets[idx].status = MarketStatus::Resolved;
        state.markets[idx].resolved_outcome = Some(outcome);
        state.save(state_path)?;
    }
    Ok(())
}

// --- step 5: deploy ONE new market ---------------------------------------

async fn deploy_one_market(ctx: &Ctx, state: &mut State, state_path: &Path) -> Result<()> {
    // Pick deployer + event. Bump cursor and persist BEFORE any chain op
    // so a crash mid-deploy never replays the same on-chain event name.
    let cursor = state.next_event_cursor;
    let (event_name, event_def) = ctx.next_event(cursor);
    let (deployer, deployer_keys) = ctx.pick_random_pn();
    let deployer_address = deployer.address.clone();
    let deployer_keys = deployer_keys.clone();
    let lifetime_hours =
        rand::thread_rng().gen_range(ctx.cfg.lifetime_hours_min..=ctx.cfg.lifetime_hours_max);
    let lifetime_secs = lifetime_hours * 3600;

    state.next_event_cursor = cursor.wrapping_add(1);
    state.save(state_path)?;

    info!(
        cursor,
        event_name = %event_name,
        deployer = %deployer_address,
        lifetime_hours,
        "deploying new market",
    );

    // 1. addEvent.
    let mut outcome_names: HashMap<u32, String> = HashMap::new();
    outcome_names.insert(0, event_def.outcomes[0].clone());
    outcome_names.insert(1, event_def.outcomes[1].clone());

    ctx.dex
        .add_event(
            &ctx.secrets.oracle.event_list_address,
            ParamsOfAddEvent {
                event_name: event_name.clone(),
                oracle_fee: ctx.cfg.market.oracle_fee,
                deadline: ctx.cfg.market.oracle_fee_deadline,
                describe: event_def.describe.clone().unwrap_or_default(),
                outcome_names: outcome_names.clone(),
                trust_addr: None,
            },
            Signer::Keys { keys: ctx.oracle_keys.clone() },
        )
        .await
        .context("add_event")?;

    // 2. Poll EventList for the new event_id.
    let event_id = poll_for_event_id(ctx, &event_name).await?;

    // 3. deployPmp.
    ctx.dex
        .deploy_pmp(
            &deployer_address,
            ParamsOfDeployPmp {
                event_id: event_id.clone(),
                oracle_fee: vec![ctx.cfg.market.oracle_fee],
                token_type: ctx.cfg.market.token_type,
                names: vec![ctx.secrets.oracle.name.clone()],
                index: vec![0],
                initial_stakes: vec![
                    ctx.cfg.market.deployer_seed_amount,
                    ctx.cfg.market.deployer_seed_amount,
                ],
            },
            Signer::Keys { keys: deployer_keys.clone() },
        )
        .await
        .context("deploy_pmp")?;

    tokio::time::sleep(Duration::from_secs(5)).await;

    // 4. Resolve PMP address from (event_id, oracle name, token_type).
    let pmp_address = ctx
        .dex
        .get_pmp_address(
            event_id.clone(),
            vec![ctx.secrets.oracle.name.clone()],
            ctx.cfg.market.token_type,
        )
        .await
        .context("get_pmp_address")?;

    // 5. Wait for oracle quorum to land on the PMP.
    let quorum = wait_pmp_quorum(ctx, &pmp_address).await?;
    let oracle_list_hash = quorum.oracle_list_hash.clone();

    // 6. submitSetTimings(resultStart = now + lifetime).
    let result_start = now_unix() + lifetime_secs;
    ctx.dex
        .submit_set_timings(
            &pmp_address,
            ParamsOfSubmitSetTimings { result_start },
            Signer::Keys { keys: ctx.oracle_keys.clone() },
        )
        .await
        .context("submit_set_timings")?;

    let timings = wait_pmp_timings(ctx, &pmp_address).await?;

    // 7. setStake on outcome 0 + outcome 1.
    for outcome in [0u32, 1u32] {
        ctx.dex
            .set_stake(
                &deployer_address,
                ParamsOfSetStake {
                    event_id: event_id.clone(),
                    oracle_list_hash: oracle_list_hash.clone(),
                    token_type: ctx.cfg.market.token_type,
                    outcome,
                    amount: ctx.cfg.market.regular_stake_amount,
                    use_coupon: false,
                },
                Signer::Keys { keys: deployer_keys.clone() },
            )
            .await
            .with_context(|| format!("set_stake outcome={outcome}"))?;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    state.markets.push(MarketRecord {
        event_id,
        event_name,
        deployer_pn_address: deployer_address,
        pmp_address: pmp_address.clone(),
        order_book_address: None,
        oracle_list_hash,
        token_type: ctx.cfg.market.token_type,
        stake_start_unix: timings.stake_start,
        stake_end_unix: timings.stake_end,
        result_start_unix: timings.result_start,
        result_end_unix: timings.result_end,
        lifetime_hours,
        status: MarketStatus::PendingFreeze,
        resolved_outcome: None,
    });
    state.save(state_path)?;
    info!(pmp = %pmp_address, "market deployed; status=PendingFreeze");
    Ok(())
}

// --- step 6: random trader stake -----------------------------------------

async fn maybe_trader_stake(ctx: &Ctx, state: &State) -> Result<()> {
    if rand::random::<f64>() >= ctx.cfg.traders.stake_probability_per_tick {
        return Ok(());
    }
    let now = now_unix();
    let candidates: Vec<&MarketRecord> = state
        .markets
        .iter()
        .filter(|m| {
            m.status == MarketStatus::PendingFreeze
                && m.stake_start_unix <= now
                && now < m.stake_end_unix
        })
        .collect();
    let Some(target) = candidates.choose(&mut rand::thread_rng()) else {
        return Ok(());
    };

    let (pn, pn_keys) = ctx.pick_random_pn();
    let outcome = rand::thread_rng().gen_range(0..2u32);

    info!(
        pmp = %target.pmp_address,
        pn = %pn.address,
        outcome,
        amount = ctx.cfg.traders.stake_amount,
        "trader setStake",
    );
    if let Err(e) = ctx
        .dex
        .set_stake(
            &pn.address,
            ParamsOfSetStake {
                event_id: target.event_id.clone(),
                oracle_list_hash: target.oracle_list_hash.clone(),
                token_type: target.token_type,
                outcome,
                amount: ctx.cfg.traders.stake_amount,
                use_coupon: false,
            },
            Signer::Keys { keys: pn_keys.clone() },
        )
        .await
    {
        warn!(pmp = %target.pmp_address, pn = %pn.address, error = ?e, "trader set_stake failed");
    }
    Ok(())
}

// =========================================================================
// Polling helpers.
// =========================================================================

async fn poll_for_event_id(ctx: &Ctx, event_name: &str) -> Result<String> {
    for _ in 0..30 {
        let events = ctx
            .dex
            .get_events(&ctx.secrets.oracle.event_list_address)
            .await
            .context("get_events")?;
        if let Some((id, _)) = events.events.iter().find(|(_, e)| {
            e.get("eventName")
                .or_else(|| e.get("event_name"))
                .and_then(|v| v.as_str())
                == Some(event_name)
        }) {
            return Ok(id.clone());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    anyhow::bail!("event `{event_name}` did not appear in EventList within 60s")
}

async fn wait_pmp_quorum(ctx: &Ctx, pmp_address: &str) -> Result<PmpDetails> {
    for _ in 0..40 {
        let d = ctx.dex.get_pmp_details(pmp_address).await.context("get_pmp_details")?;
        if d.number_of_oracle_events > 0 && d.approved_oracle_events >= d.number_of_oracle_events {
            // Settle a beat — quorum-applied state needs a moment before
            // subsequent setTimings is accepted.
            tokio::time::sleep(Duration::from_secs(5)).await;
            return Ok(d);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    anyhow::bail!("PMP {pmp_address} did not reach oracle quorum within 120s")
}

async fn wait_pmp_timings(ctx: &Ctx, pmp_address: &str) -> Result<PmpDetails> {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let d = ctx.dex.get_pmp_details(pmp_address).await.context("get_pmp_details")?;
        if d.stake_end > 0 && d.result_start > 0 {
            return Ok(d);
        }
    }
    anyhow::bail!("PMP {pmp_address} timings did not appear within 60s")
}

// =========================================================================
// Small helpers.
// =========================================================================

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

fn big_to_u64(b: &BigInt) -> u64 {
    // Clamp at u64::MAX — for refill comparisons "way above threshold"
    // and "the actual value" are interchangeable.
    u64::try_from(b).unwrap_or(u64::MAX)
}

fn pn_keys_for<'a>(ctx: &'a Ctx, address: &str) -> Option<&'a KeyPair> {
    ctx.secrets
        .private_notes
        .iter()
        .position(|pn| pn.address == address)
        .map(|i| &ctx.pn_keys[i])
}

fn build_client_context(endpoint: &str) -> Result<Arc<ClientContext>> {
    let mut config = ClientConfig::default();
    let url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("https://{endpoint}")
    };
    config.network.endpoints = Some(vec![url]);
    // Match bee-engine: disable internal reconnect-loop so a flaky
    // shellnet doesn't self-amplify into a storm of retries on our side.
    config.network.max_reconnect_timeout = 0;
    ClientContext::new(config).map(Arc::new).context("ClientContext::new")
}

// =========================================================================
// Entrypoint.
// =========================================================================

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = env::var("APP_CONFIG")
        .unwrap_or_else(|_| "config/market-manager.stage.yaml".to_string());
    let cfg = Config::load(Path::new(&config_path))?;
    let events = EventsFile::load(&cfg.events_file)?;
    let secrets = Secrets::load(&cfg.secrets_file)?;
    let state_path = cfg.state_file.clone();
    let tick_interval = Duration::from_secs(cfg.tick_interval_secs);

    let context = build_client_context(&cfg.endpoint)?;
    let dex = Dex::new(context.clone());
    let oracle_keys = keypair_of(&secrets.oracle.pubkey_hex, &secrets.oracle.secret_hex);
    let pn_keys: Vec<KeyPair> = secrets
        .private_notes
        .iter()
        .map(|pn| keypair_of(&pn.pubkey_hex, &pn.secret_hex))
        .collect();

    info!(
        endpoint = %cfg.endpoint,
        target = cfg.target_active_markets,
        lifetime_hours = format!("{}..={}", cfg.lifetime_hours_min, cfg.lifetime_hours_max),
        token_type = cfg.market.token_type,
        pns = secrets.private_notes.len(),
        events = events.events.len(),
        oracle = %secrets.oracle.address,
        event_list = %secrets.oracle.event_list_address,
        "market-manager starting",
    );

    let ctx = Ctx { cfg, secrets, events, context, dex, oracle_keys, pn_keys };
    let mut state = State::load_or_init(&state_path)?;

    let mut ticker = tokio::time::interval(tick_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        if let Err(e) = tick(&ctx, &mut state, &state_path).await {
            error!(error = ?e, "tick failed; will retry next interval");
        }
        if let Err(e) = state.save(&state_path) {
            error!(error = ?e, "failed to persist state at end of tick");
        }
    }
}
