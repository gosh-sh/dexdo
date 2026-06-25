//! `dexdo` — one CLI over the `dodex-sdk` `Dex` facade for the on-chain /
//! library operations the public REST API does **not** expose (staking and,
//! over time, the rest of the chain-side surface).
//!
//! This is deliberately **one binary with subcommands**, not a binary per
//! operation: the `Dex` facade has ~50 methods (`set_stake`, `claim`,
//! `cancel_stake`, `split_full_set`, `merge_full_set`, `get_pmp_details`,
//! `get_private_note_details`, `place_order`, …) and new ones land regularly.
//! Adding one is a `match` arm + a handler `fn` here — see "ADDING A SUBCOMMAND".
//!
//! Usage:
//!   dexdo <subcommand> [flags]
//!   dexdo --help
//!
//! Implemented subcommands:
//!   stake        Stake on one outcome during the STAKING phase (PrivateNote.setStake).
//!   stakes       Show a note's stakes across markets. (read-only)
//!   place-order  Place an order via the SDK, bypassing REST (surfaces the real error).
//!   pmp-details  Read a market's (PMP) phase, window, outcomes, identity. (read-only)
//!
//! Common flag: --endpoint <host>  (default shellnet.ackinacki.org)
//!
//! ADDING A SUBCOMMAND
//!   1. add a `"<name>" => cmd_<name>(rest).await,` arm in `dispatch`;
//!   2. write `async fn cmd_<name>(args: Flags) -> ExitCode` using `dex(...)`;
//!   3. list it in `usage()`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use dodex_contracts::dex::private_note::ParamsOfCancelAllOrders;
use dodex_contracts::dex::private_note::ParamsOfMergeFullSet;
use dodex_contracts::dex::private_note::ParamsOfPlaceOrder;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfStakeKey;
use dodex_contracts::dex::private_note::ParamsOfWithdrawTokens;
use dodex_sdk::Dex;
use dodex_sdk::DexConfig;
use serde::Deserialize;

// create-prediction-market deps (oracle + event + PMP deploy)
use std::sync::Arc;
use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::giver::v3::top_up_native_with_giver_if_below;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::tvm_client::crypto;
use ackinacki_kit::tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys;
use ackinacki_kit::tvm_client::crypto::ParamsOfMnemonicFromRandom;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::oracle::Oracle;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::OracleEventList;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::root_oracle::ParamsOfDeployOracle;
use dodex_contracts::dex::root_oracle::RootOracle;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof;

const DEFAULT_ENDPOINT: &str = "shellnet.ackinacki.org";

// create-prediction-market constants (mirror the e2e setup defaults).
const ORACLE_FEE: u128 = 100;
const DEPLOYER_SEED_AMOUNT: u128 = 100_000_000_000; // 100 NACKL seeded per outcome
const EVENT_DEADLINE: u64 = 2_000_000_000; // event service deadline (far future)
const ROOT_ORACLE_NATIVE_TARGET: u64 = 120_000_000_000;
const ROOT_ORACLE_NATIVE_THRESHOLD: u64 = 50_000_000_000;
// Timing constants taken from the contract (dex/modifiers/modifiers.sol + PMP.sol).
const MIN_RESULT_GAP: u64 = 120; // PMP.setTimings requires resultStart >= now + this
const DEFAULT_RESULT_GAP: u64 = 3000; // default resultStart = now + this when --result-start omitted

// ----------------------------- arg parsing -----------------------------

/// Minimal `--flag value` / `--flag` parser over the args after the subcommand.
/// Shared by every subcommand so they look and behave the same.
struct Flags {
    values: HashMap<String, String>,
    switches: Vec<String>,
}

impl Flags {
    /// Parse `--k v` pairs and bare `--switch` flags. A flag whose next token is
    /// missing or starts with `--` is treated as a switch.
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut values = HashMap::new();
        let mut switches = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let tok = &args[i];
            let key = tok.strip_prefix("--").ok_or_else(|| format!("unexpected arg `{tok}`"))?;
            match args.get(i + 1) {
                Some(v) if !v.starts_with("--") => {
                    values.insert(key.to_string(), v.clone());
                    i += 2;
                }
                _ => {
                    switches.push(key.to_string());
                    i += 1;
                }
            }
        }
        Ok(Flags { values, switches })
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.values.get(k).map(String::as_str)
    }
    fn require(&self, k: &str) -> Result<&str, String> {
        self.get(k).ok_or_else(|| format!("--{k} is required"))
    }
    fn has(&self, k: &str) -> bool {
        self.switches.iter().any(|s| s == k)
    }
    fn endpoint(&self) -> String {
        self.get("endpoint").unwrap_or(DEFAULT_ENDPOINT).to_string()
    }
}

fn dex(endpoint: &str) -> Result<Dex, String> {
    Dex::new(DexConfig { endpoints: vec![endpoint.to_string()], ..Default::default() })
        .map_err(|e| format!("cannot create Dex client: {e:?}"))
}

// ----------------------------- shared helpers -----------------------------

/// Subset of `pn_state.<tt>.json` (from onboarding) needed to sign as the note.
#[derive(Deserialize)]
struct PnStateFile {
    pn_address: Option<String>,
    owner_public_key_hex: Option<String>,
    owner_secret_key_hex: Option<String>,
}

fn read_pn_state(path: &str) -> Result<PnStateFile, String> {
    let path = PathBuf::from(path);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read pn state {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Full note identity + signing key — for write ops (stake).
fn load_pn(path: &str) -> Result<(String, KeyPair), String> {
    let st = read_pn_state(path)?;
    let pn_address = st.pn_address.ok_or("pn state has no pn_address (note not deployed?)")?;
    let public = st.owner_public_key_hex.ok_or("pn state has no owner_public_key_hex")?;
    let secret = st.owner_secret_key_hex.ok_or("pn state has no owner_secret_key_hex")?;
    Ok((pn_address, KeyPair { public, secret }))
}

/// Just the note address — for read ops (stakes). Accepts `--pn-address`
/// directly, or reads `pn_address` from `--pn-state-file`.
fn resolve_pn_address(f: &Flags) -> Result<String, String> {
    if let Some(a) = f.get("pn-address") {
        return Ok(a.to_string());
    }
    let path = f.get("pn-state-file").ok_or("need --pn-address or --pn-state-file")?;
    read_pn_state(path)?.pn_address.ok_or_else(|| "pn state has no pn_address".to_string())
}

/// On-chain `decimals` for a quote-asset token type (NACKL=1, SHELL=2, USDC=3).
fn decimals_for(token_type: u32) -> Result<u32, String> {
    match token_type {
        1 | 2 => Ok(9), // NACKL, SHELL
        3 => Ok(6),     // USDC
        other => Err(format!("unknown token_type {other}; cannot scale --amount")),
    }
}

fn token_label(token_type: u32) -> &'static str {
    match token_type {
        1 => "NACKL",
        2 => "SHELL",
        3 => "USDC",
        _ => "?",
    }
}

/// Parse a human decimal (e.g. "20", "0.5") into raw token units at `decimals`.
fn parse_amount_to_raw(amount: &str, decimals: u32) -> Result<u128, String> {
    let amount = amount.trim();
    let (int_part, frac_part) = amount.split_once('.').unwrap_or((amount, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return Err("empty --amount".into());
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!("--amount is not a decimal number: {amount}"));
    }
    if frac_part.len() as u32 > decimals {
        return Err(format!("--amount has more than {decimals} decimal places: {amount}"));
    }
    let mut digits = String::new();
    digits.push_str(int_part);
    digits.push_str(frac_part);
    for _ in 0..(decimals - frac_part.len() as u32) {
        digits.push('0');
    }
    let raw: u128 = digits.parse().map_err(|e| format!("--amount out of range: {e}"))?;
    if raw == 0 {
        return Err("--amount must be greater than 0".into());
    }
    Ok(raw)
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ----------------------------- subcommands -----------------------------

/// `dexdo stake` — stake on one outcome while the market is in STAKING.
/// On-chain `PrivateNote.setStake` → `PMP.acceptStake`, signed by the note key.
async fn cmd_stake(f: Flags) -> ExitCode {
    let market = match f.require("market-address") {
        Ok(v) => v.to_string(),
        Err(e) => return fail(&e),
    };
    let pn_state = match f.require("pn-state-file") {
        Ok(v) => v.to_string(),
        Err(e) => return fail(&e),
    };
    let outcome: u32 = match f.require("outcome").and_then(|v| {
        v.parse().map_err(|_| format!("--outcome not a u32: {v}"))
    }) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let amount_human = match f.require("amount") {
        Ok(v) => v.to_string(),
        Err(e) => return fail(&e),
    };
    let use_coupon = f.has("use-coupon");

    let dex = match dex(&f.endpoint()) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };

    // Read the market identity + staking window straight from the PMP.
    let d = match dex.get_pmp_details(&market).await {
        Ok(d) => d,
        Err(e) => return fail(&format!("get_pmp_details({market}) failed: {e:?}")),
    };

    // Validate phase / inputs before spending anything.
    if !d.approved {
        return fail("market not approved yet (oracle timings not accepted) — cannot stake");
    }
    if d.is_cancelled {
        return fail("market event was cancelled — cannot stake");
    }
    let now = now_unix();
    if now < d.stake_start {
        return fail(&format!(
            "staking not open yet (stake_start={}, now={}); wait {}s",
            d.stake_start,
            now,
            d.stake_start - now
        ));
    }
    if now >= d.stake_end {
        return fail(&format!(
            "staking window has closed (stake_end={}, now={}). The market is past STAKING; \
             take a position via the order book (the dexdo-trading skill) instead.",
            d.stake_end, now
        ));
    }
    if outcome >= d.num_outcomes {
        return fail(&format!(
            "outcome {} out of range (market has {} outcomes: {:?})",
            outcome, d.num_outcomes, d.outcome_names
        ));
    }

    let decimals = match decimals_for(d.token_type) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let amount = match parse_amount_to_raw(&amount_human, decimals) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let (pn_address, keys) = match load_pn(&pn_state) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };

    let outcome_label =
        d.outcome_names.get(&outcome).cloned().unwrap_or_else(|| format!("#{outcome}"));
    eprintln!(
        "[dexdo stake] {} {} (raw {}) on outcome {} ({}) of {} via note {}",
        amount_human,
        token_label(d.token_type),
        amount,
        outcome,
        outcome_label,
        market,
        pn_address,
    );

    match dex
        .set_stake(
            &pn_address,
            ParamsOfSetStake {
                event_id: d.event_id.clone(),
                oracle_list_hash: d.oracle_list_hash.clone(),
                token_type: d.token_type,
                outcome,
                amount,
                use_coupon,
            },
            Signer::Keys { keys },
        )
        .await
    {
        Ok(res) => {
            println!("[dexdo stake] DONE — stake submitted: {res:?}");
            eprintln!(
                "[dexdo stake] confirm shortly: the staked amount leaves the note's free balance \
                 once the chain settles (check `dexdo-market-data` account / pmp-details)."
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("set_stake failed: {e:?}")),
    }
}

/// `dexdo pmp-details` — read a market's phase window + outcomes + identity.
/// Useful before staking (resolve outcome ids, confirm STAKING window) and as a
/// template for further read subcommands.
async fn cmd_pmp_details(f: Flags) -> ExitCode {
    let market = match f.require("market-address") {
        Ok(v) => v.to_string(),
        Err(e) => return fail(&e),
    };
    let dex = match dex(&f.endpoint()) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    let d = match dex.get_pmp_details(&market).await {
        Ok(d) => d,
        Err(e) => return fail(&format!("get_pmp_details({market}) failed: {e:?}")),
    };
    let now = now_unix();
    let phase = if d.is_cancelled {
        "CANCELLED"
    } else if !d.approved {
        "PENDING/UNAPPROVED"
    } else if now < d.stake_start {
        "UPCOMING"
    } else if now < d.stake_end {
        "STAKING"
    } else if d.resolved_outcome.is_some() {
        "RESOLVED"
    } else {
        "POST-STAKING (AWAITING_FREEZE/TRADING/RESOLVING)"
    };
    let out = serde_json::json!({
        "marketAddress": market,
        "name": d.name,
        "phaseHint": phase,
        "approved": d.approved,
        "isCancelled": d.is_cancelled,
        "tokenType": d.token_type,
        "tokenLabel": token_label(d.token_type),
        "eventId": d.event_id,
        "oracleListHash": d.oracle_list_hash,
        "numOutcomes": d.num_outcomes,
        "outcomeNames": d.outcome_names,
        "stakeStart": d.stake_start,
        "stakeEnd": d.stake_end,
        "resultStart": d.result_start,
        "resultEnd": d.result_end,
        "resolvedOutcome": d.resolved_outcome,
        "totalPool": d.total_pool.to_string(),
        "now": now,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".into()));
    ExitCode::SUCCESS
}

/// `dexdo place-order` — place an order on the order book DIRECTLY via the SDK
/// (`PrivateNote.placeOrder`), bypassing the REST API. The REST `POST /order`
/// masks chain failures as a generic `-1000`; this path surfaces the real error.
/// price is a human probability 0..1 (converted to basis points); amount is the
/// outcome-token quantity (scaled by token decimals). tif → flags:
/// GTC=0, IOC=0x01, FOK=0x02, POST_ONLY=0x08.
async fn cmd_place_order(f: Flags) -> ExitCode {
    let market = match f.require("market-address") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let pn_state = match f.require("pn-state-file") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let outcome: u32 = match f.require("outcome").and_then(|v| v.parse().map_err(|_| format!("--outcome not a u32: {v}"))) {
        Ok(v) => v, Err(e) => return fail(&e),
    };
    let side = match f.require("side") { Ok(v) => v.to_uppercase(), Err(e) => return fail(&e) };
    let is_buy = match side.as_str() {
        "BUY" => true, "SELL" => false,
        other => return fail(&format!("--side must be BUY or SELL, got {other}")),
    };
    let price_human = match f.require("price") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let amount_human = match f.require("amount") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let flags: u8 = match f.get("tif").unwrap_or("GTC").to_uppercase().as_str() {
        "GTC" => 0x00, "IOC" => 0x01, "FOK" => 0x02, "POST_ONLY" => 0x08,
        other => return fail(&format!("--tif must be GTC|IOC|FOK|POST_ONLY, got {other}")),
    };

    let dex = match dex(&f.endpoint()) { Ok(d) => d, Err(e) => return fail(&e) };
    let d = match dex.get_pmp_details(&market).await {
        Ok(d) => d, Err(e) => return fail(&format!("get_pmp_details({market}) failed: {e:?}")),
    };
    if outcome >= d.num_outcomes {
        return fail(&format!("outcome {} out of range ({} outcomes)", outcome, d.num_outcomes));
    }
    let decimals = match decimals_for(d.token_type) { Ok(v) => v, Err(e) => return fail(&e) };
    let amount = match parse_amount_to_raw(&amount_human, decimals) { Ok(v) => v, Err(e) => return fail(&e) };
    let price_bps = match price_to_bps(&price_human) { Ok(v) => v, Err(e) => return fail(&e) };
    if price_bps > 10_000 {
        return fail(&format!("--price must be a probability in (0, 1] ({price_bps} bps > 10000)"));
    }
    // A bad --client-id must error, not silently fall back to a timestamp (that would
    // place the order under an id the caller can't track / dedup a retry against).
    let client_order_id: u128 = match f.get("client-id") {
        Some(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => return fail(&format!("--client-id must be a numeric u128, got `{v}`")),
        },
        None => now_unix() as u128,
    };

    let (pn_address, keys) = match load_pn(&pn_state) { Ok(v) => v, Err(e) => return fail(&e) };
    eprintln!(
        "[dexdo place-order] {} {} {} outcome {} @ {} bps x {} (raw {}) flags=0x{:02x} on {} via {}",
        side, token_label(d.token_type), if is_buy {"BUY"} else {"SELL"},
        outcome, price_bps, amount_human, amount, flags, market, pn_address,
    );
    match dex.place_order(
        &pn_address,
        ParamsOfPlaceOrder {
            event_id: d.event_id.clone(),
            oracle_list_hash: d.oracle_list_hash.clone(),
            token_type: d.token_type,
            outcome_id: outcome,
            is_buy,
            price: price_bps.to_string(),
            amount,
            flags,
            min_amount: 0,
            epoch_id: 0,
            client_order_id,
        },
        Signer::Keys { keys },
    ).await {
        Ok(res) => { println!("[dexdo place-order] DONE: {res:?}"); ExitCode::SUCCESS }
        Err(e) => fail(&format!("place_order failed (REAL error, not the REST -1000): {e:?}")),
    }
}

/// Human probability "0.30" → basis points (3000). tickSize 0.001 → 10 bps steps.
fn price_to_bps(price: &str) -> Result<u64, String> {
    let raw = parse_amount_to_raw(price, 4)?; // 4 dp == basis points
    Ok(raw as u64)
}

/// `dexdo stakes` — show a note's stakes across markets (read-only).
async fn cmd_stakes(f: Flags) -> ExitCode {
    let pn_address = match resolve_pn_address(&f) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let dex = match dex(&f.endpoint()) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    match dex.get_stakes(&pn_address).await {
        Ok(s) => {
            let out = serde_json::json!({ "pnAddress": pn_address, "stakes": s.stakes });
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".into()));
            if s.stakes.is_empty() {
                eprintln!("[dexdo stakes] no stakes for this note yet");
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("get_stakes({pn_address}) failed: {e:?}")),
    }
}

/// `dexdo cancel-stake` — recover a stake from a still-open STAKING market.
async fn cmd_cancel_stake(f: Flags) -> ExitCode {
    stake_key_op(f, "cancel-stake").await
}
/// `dexdo claim` — settle/claim a RESOLVED or CANCELLED market.
async fn cmd_claim(f: Flags) -> ExitCode {
    stake_key_op(f, "claim").await
}

/// Shared body for the two `ParamsOfStakeKey` ops (cancel-stake / claim): both
/// take {event_id, oracle_list_hash, token_type} read from the PMP on-chain.
async fn stake_key_op(f: Flags, which: &str) -> ExitCode {
    let market = match f.require("market-address") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let pn_state = match f.require("pn-state-file") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let dex = match dex(&f.endpoint()) { Ok(d) => d, Err(e) => return fail(&e) };
    let d = match dex.get_pmp_details(&market).await {
        Ok(d) => d, Err(e) => return fail(&format!("get_pmp_details({market}) failed: {e:?}")),
    };
    let (pn_address, keys) = match load_pn(&pn_state) { Ok(v) => v, Err(e) => return fail(&e) };
    let key = ParamsOfStakeKey {
        event_id: d.event_id.clone(),
        oracle_list_hash: d.oracle_list_hash.clone(),
        token_type: d.token_type,
    };
    eprintln!("[dexdo {which}] market {market} via note {pn_address}");
    let res = if which == "claim" {
        dex.claim(&pn_address, key, Signer::Keys { keys }).await
    } else {
        dex.cancel_stake(&pn_address, key, Signer::Keys { keys }).await
    };
    match res {
        Ok(r) => { println!("[dexdo {which}] DONE: {r:?}"); ExitCode::SUCCESS }
        Err(e) => fail(&format!("{which} failed: {e:?}")),
    }
}

/// `dexdo cancel-all-orders` — cancel all the note's resting orders on one market.
async fn cmd_cancel_all_orders(f: Flags) -> ExitCode {
    let market = match f.require("market-address") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let pn_state = match f.require("pn-state-file") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let dex = match dex(&f.endpoint()) { Ok(d) => d, Err(e) => return fail(&e) };
    let d = match dex.get_pmp_details(&market).await {
        Ok(d) => d, Err(e) => return fail(&format!("get_pmp_details({market}) failed: {e:?}")),
    };
    let (pn_address, keys) = match load_pn(&pn_state) { Ok(v) => v, Err(e) => return fail(&e) };
    eprintln!("[dexdo cancel-all-orders] market {market} via note {pn_address}");
    match dex.cancel_all_orders(
        &pn_address,
        ParamsOfCancelAllOrders {
            event_id: d.event_id.clone(),
            oracle_list_hash: d.oracle_list_hash.clone(),
            token_type: d.token_type,
        },
        Signer::Keys { keys },
    ).await {
        Ok(r) => { println!("[dexdo cancel-all-orders] DONE: {r:?}"); ExitCode::SUCCESS }
        Err(e) => fail(&format!("cancel_all_orders failed: {e:?}")),
    }
}

/// `dexdo merge-full-set` — merge held outcome tokens back into collateral on one
/// market. `--amounts` is a comma list of per-outcome human amounts (order = outcomeId).
async fn cmd_merge_full_set(f: Flags) -> ExitCode {
    let market = match f.require("market-address") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let pn_state = match f.require("pn-state-file") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let amounts_csv = match f.require("amounts") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let dex = match dex(&f.endpoint()) { Ok(d) => d, Err(e) => return fail(&e) };
    let d = match dex.get_pmp_details(&market).await {
        Ok(d) => d, Err(e) => return fail(&format!("get_pmp_details({market}) failed: {e:?}")),
    };
    let decimals = match decimals_for(d.token_type) { Ok(v) => v, Err(e) => return fail(&e) };
    let mut amount: Vec<u128> = Vec::new();
    for part in amounts_csv.split(',') {
        let p = part.trim();
        // A zero per-outcome amount is legitimate (merge fewer outcomes); accept any
        // spelling ("0", "0.0", "0.00"), which parse_amount_to_raw rejects as "> 0".
        if p.parse::<f64>().map(|f| f == 0.0).unwrap_or(false) {
            amount.push(0);
            continue;
        }
        match parse_amount_to_raw(p, decimals) {
            Ok(v) => amount.push(v),
            Err(e) => return fail(&format!("--amounts: {e}")),
        }
    }
    if amount.len() as u32 != d.num_outcomes {
        return fail(&format!("--amounts has {} entries but market has {} outcomes", amount.len(), d.num_outcomes));
    }
    let (pn_address, keys) = match load_pn(&pn_state) { Ok(v) => v, Err(e) => return fail(&e) };
    eprintln!("[dexdo merge-full-set] {amount:?} on {market} via {pn_address}");
    match dex.merge_full_set(
        &pn_address,
        ParamsOfMergeFullSet {
            event_id: d.event_id.clone(),
            oracle_list_hash: d.oracle_list_hash.clone(),
            token_type: d.token_type,
            amount,
        },
        Signer::Keys { keys },
    ).await {
        Ok(r) => { println!("[dexdo merge-full-set] DONE: {r:?}"); ExitCode::SUCCESS }
        Err(e) => fail(&format!("merge_full_set failed: {e:?}")),
    }
}

/// `dexdo withdraw` — sweep ALL the note's free collateral to a wallet (the
/// multisig). Close positions/orders/stakes first so collateral is unlocked.
async fn cmd_withdraw(f: Flags) -> ExitCode {
    let pn_state = match f.require("pn-state-file") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let dest = match f.require("dest") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    // dapp_id defaults to the destination's account-id (a multisig deploys under
    // its own account-id as dapp_id); override with --dapp-id if different.
    // It is a uint256 — pass it 0x-prefixed so the ABI parses it as hex, not as a
    // (failing) decimal of the bare account-id.
    let dapp_id = f.get("dapp-id").map(|s| s.to_string()).unwrap_or_else(|| {
        let bare = dest.strip_prefix("0:").unwrap_or(&dest);
        if bare.starts_with("0x") { bare.to_string() } else { format!("0x{bare}") }
    });
    let (pn_address, keys) = match load_pn(&pn_state) { Ok(v) => v, Err(e) => return fail(&e) };
    let dex = match dex(&f.endpoint()) { Ok(d) => d, Err(e) => return fail(&e) };
    eprintln!("[dexdo withdraw] note {pn_address} → wallet {dest} (dapp_id {dapp_id})");
    match dex.withdraw_tokens(
        &pn_address,
        ParamsOfWithdrawTokens { dest_wallet_addr: dest, dapp_id },
        Signer::Keys { keys },
    ).await {
        Ok(r) => { println!("[dexdo withdraw] DONE: {r:?}"); ExitCode::SUCCESS }
        Err(e) => fail(&format!("withdraw_tokens failed: {e:?}")),
    }
}

// ------------------- create-prediction-market helpers -------------------

fn create_tvm_context(endpoint: &str) -> Arc<ClientContext> {
    let mut config = ClientConfig::default();
    config.network.endpoints = Some(vec![endpoint.to_string()]);
    Arc::new(ClientContext::new(config).expect("create context"))
}

/// Fresh 24-word signing keypair (oracle / ephemeral deploy-signer).
fn gen_keys(context: Arc<ClientContext>) -> KeyPair {
    let phrase = crypto::mnemonic_from_random(
        context.clone(),
        ParamsOfMnemonicFromRandom { dictionary: None, word_count: Some(24) },
    )
    .expect("mnemonic")
    .phrase;
    crypto::mnemonic_derive_sign_keys(
        context,
        ParamsOfMnemonicDeriveSignKeys { phrase, path: None, dictionary: None, word_count: Some(24) },
    )
    .expect("derive keys")
}

async fn wait_active<T: AccountAccessor>(contract: &T, label: &str) -> Result<(), String> {
    contract
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(30),
            attempts_timeout: Some(2_000),
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("wait {label} active: {e:?}"))
}

/// `dexdo create-prediction-market` — deploy a fresh oracle + event + PMP
/// (one prediction market) using a funded deployer note, then print the
/// `predictionMarketAddress` + identity + staking window. The deployer note
/// seeds the initial stakes, so after this it already holds positions —
/// recover them with `cancel-stake` (while STAKING) + `withdraw`.
async fn cmd_create_prediction_market(f: Flags) -> ExitCode {
    let endpoint = f.endpoint();
    let state = match f.require("pn-state-file") { Ok(v) => v.to_string(), Err(e) => return fail(&e) };
    let (pn_address, pn_keys) = match load_pn(&state) { Ok(v) => v, Err(e) => return fail(&e) };
    let name_prefix = f.get("name").unwrap_or("dexdo-pm").to_string();
    let outcomes_arg = f.get("outcomes").unwrap_or("Yes,No").to_string();
    let outcome_list: Vec<String> =
        outcomes_arg.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if outcome_list.len() < 2 {
        return fail("need at least 2 outcomes (--outcomes \"Yes,No\")");
    }
    // The contract's own `submitSetTimings(resultStart)` parameter (unix seconds).
    // Optional override; if omitted we default to now + DEFAULT_RESULT_GAP. The
    // contract derives the rest: stakeEnd = stakeStart + (resultStart-stakeStart)/10,
    // resultEnd = resultStart + GRACE_PERIOD. We do NOT invent our own window.
    let result_start_override: Option<u64> = match f.get("result-start").map(str::parse::<u64>) {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => return fail("--result-start must be a unix timestamp (seconds)"),
        None => None,
    };

    let token_type = proof::TokenType::Nackl as u32;
    let context = create_tvm_context(&endpoint);
    let dx = match dex(&endpoint) { Ok(d) => d, Err(e) => return fail(&e) };

    let oracle_keys = gen_keys(context.clone());
    let ephemeral_keys = gen_keys(context.clone());
    let run_id = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let oracle_name = format!("{name_prefix}-{run_id:x}");
    eprintln!(
        "[dexdo create-prediction-market] oracle={oracle_name} outcomes={outcome_list:?} deployer={pn_address}"
    );

    // RootOracle needs gas to materialize the Oracle.
    let root_oracle =
        RootOracle::new(context.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS));
    if let Err(e) = wait_active(&root_oracle, "RootOracle").await { return fail(&e); }
    if let Err(e) = top_up_native_with_giver_if_below(
        context.clone(),
        &root_oracle,
        ROOT_ORACLE_NATIVE_TARGET,
        ROOT_ORACLE_NATIVE_THRESHOLD,
        "RootOracle",
    )
    .await
    {
        return fail(&format!("top up RootOracle: {e:?}"));
    }

    // 1. Deploy oracle (ephemeral signer; on-chain pubkey is the oracle key).
    if let Err(e) = dx
        .deploy_oracle(
            ParamsOfDeployOracle {
                oracle_pubkey: proof::pubkey_to_dec(&oracle_keys.public),
                oracle_name: oracle_name.clone(),
            },
            Signer::Keys { keys: ephemeral_keys },
        )
        .await
    {
        return fail(&format!("deploy_oracle: {e:?}"));
    }
    let oracle_address = match dx.get_oracle_address(oracle_name.clone()).await {
        Ok(a) => a,
        Err(e) => return fail(&format!("get_oracle_address: {e:?}")),
    };
    let oracle_contract = Oracle::new(context.clone(), dex_contract_params(&oracle_address));
    if let Err(e) = wait_active(&oracle_contract, "Oracle").await { return fail(&e); }
    let el_address = match dx
        .get_event_list_address(&oracle_address, ParamsOfGetEventListAddress { index: 0 })
        .await
    {
        Ok(a) => a,
        Err(e) => return fail(&format!("get_event_list_address: {e:?}")),
    };
    let el_contract = OracleEventList::new(context.clone(), dex_contract_params(&el_address));
    if let Err(e) = wait_active(&el_contract, "EventList").await { return fail(&e); }

    // 2. Add the event.
    let event_name = format!("Match {run_id:x}");
    let outcome_names: HashMap<u32, String> =
        outcome_list.iter().enumerate().map(|(i, n)| (i as u32, n.clone())).collect();
    if let Err(e) = dx
        .add_event(
            &el_address,
            ParamsOfAddEvent {
                event_name: event_name.clone(),
                oracle_fee: ORACLE_FEE,
                deadline: EVENT_DEADLINE,
                describe: "Who wins?".to_string(),
                outcome_names,
                trust_addr: None,
            },
            Signer::Keys { keys: oracle_keys.clone() },
        )
        .await
    {
        return fail(&format!("add_event: {e:?}"));
    }
    let mut event_id = String::new();
    for _ in 0..20 {
        if let Ok(evs) = dx.get_events(&el_address).await {
            if let Some((id, _)) = evs
                .events
                .iter()
                .find(|(_, e)| e.get("eventName").and_then(|v| v.as_str()) == Some(event_name.as_str()))
            {
                event_id = id.clone();
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    if event_id.is_empty() {
        return fail("event did not appear after add_event");
    }

    // 3. Deploy the PMP (prediction market), signed by the deployer note;
    //    it seeds the initial stakes for every outcome.
    let initial_stakes = vec![DEPLOYER_SEED_AMOUNT; outcome_list.len()];
    if let Err(e) = dx
        .deploy_pmp(
            &pn_address,
            ParamsOfDeployPmp {
                event_id: event_id.clone(),
                oracle_fee: vec![ORACLE_FEE],
                token_type,
                names: vec![oracle_name.clone()],
                index: vec![0u128],
                initial_stakes,
            },
            Signer::Keys { keys: pn_keys },
        )
        .await
    {
        return fail(&format!("deploy_pmp: {e:?}"));
    }
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let pmp_address = match dx.get_pmp_address(event_id.clone(), vec![oracle_name.clone()], token_type).await {
        Ok(a) => a,
        Err(e) => return fail(&format!("get_pmp_address: {e:?}")),
    };
    let pmp_contract = Pmp::new(context.clone(), dex_contract_params(&pmp_address));
    if let Err(e) = wait_active(&pmp_contract, "PMP").await { return fail(&e); }

    // 4. Wait until the PMP has confirmed the event with the oracle (the oracle
    //    is registered as trust-addr) — only then will submitSetTimings apply.
    eprintln!("[dexdo create-prediction-market] waiting for event confirmation…");
    let mut confirmed = false;
    for _ in 0..80 {
        if let Ok(d) = dx.get_pmp_details(&pmp_address).await {
            if d.number_of_oracle_events > 0 && d.approved_oracle_events >= d.number_of_oracle_events {
                confirmed = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    if !confirmed {
        return fail("event confirmation never landed (PMP not approved by oracle) — cannot set timings");
    }

    // 5. Oracle sets the staking timings → market opens for STAKING.
    //    result_start = now + window; the contract derives the stake window from it.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let result_start = result_start_override.unwrap_or(now + DEFAULT_RESULT_GAP);
    // Mirror the contract's first-call guard so we fail early with a clear message.
    if result_start < now + MIN_RESULT_GAP {
        return fail(&format!(
            "--result-start must be >= now + {MIN_RESULT_GAP}s (contract MIN_RESULT_GAP); got {result_start}, now {now}"
        ));
    }
    // Contract derives the STAKING window as (resultStart - stakeStart)/10.
    let approx_stake_window = result_start.saturating_sub(now) / 10;
    eprintln!(
        "[dexdo create-prediction-market] submitSetTimings(resultStart={result_start}) → ~{approx_stake_window}s STAKING window…"
    );
    let mut timings_ok = false;
    for attempt in 0..6 {
        match dx
            .submit_set_timings(
                &pmp_address,
                ParamsOfSubmitSetTimings { result_start },
                Signer::Keys { keys: oracle_keys.clone() },
            )
            .await
        {
            Ok(_) => { timings_ok = true; break; }
            Err(e) => {
                eprintln!("[dexdo create-prediction-market] submit_set_timings attempt {} failed ({e:?}); retrying…", attempt + 1);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
    if !timings_ok {
        return fail("submit_set_timings failed (could not open STAKING window)");
    }
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // 6. Re-read the market — should now be approved with an open STAKING window.
    let details = match dx.get_pmp_details(&pmp_address).await {
        Ok(d) => d,
        Err(e) => return fail(&format!("get_pmp_details after timings: {e:?}")),
    };

    let out = serde_json::json!({
        "predictionMarketAddress": pmp_address,
        "oracleAddress": oracle_address,
        "oracleName": oracle_name,
        // KEEP SECRET — the oracle key is needed to `submit_resolve` this market later.
        "oracleSecretHex": oracle_keys.secret,
        "eventId": event_id,
        "tokenType": token_type,
        "outcomes": outcome_list,
        "approved": details.approved,
        "oracleListHash": details.oracle_list_hash,
        "stakeStart": details.stake_start,
        "stakeEnd": details.stake_end,
        "resultStart": details.result_start,
        "resultEnd": details.result_end,
        "endpoint": endpoint,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".into()));
    eprintln!("[dexdo create-prediction-market] DONE predictionMarketAddress={pmp_address} approved={}", details.approved);
    ExitCode::SUCCESS
}

// ----------------------------- dispatch -----------------------------

fn fail(msg: &str) -> ExitCode {
    eprintln!("[dexdo] {msg}");
    ExitCode::FAILURE
}

fn usage() -> String {
    "dexdo — CLI over the dodex-sdk Dex facade (chain/library operations)\n\n\
     usage: dexdo <subcommand> [flags]\n\n\
     subcommands:\n  \
       stake         --market-address 0:<pmp> --pn-state-file <path> --outcome <id> \
     --amount <human> [--endpoint host] [--use-coupon]\n                  \
       Stake on one outcome during the STAKING phase (PrivateNote.setStake).\n  \
       stakes        (--pn-state-file <path> | --pn-address 0:<pn>) [--endpoint host]\n                  \
       Show a note's stakes across markets (read-only).\n  \
       place-order   --market-address 0:<pmp> --pn-state-file <path> --outcome <id> \
     --side BUY|SELL --price <0..1> --amount <human> [--tif GTC|IOC|FOK|POST_ONLY]\n                  \
       Place an order via the SDK (bypasses REST; shows the real chain error).\n  \
       cancel-all-orders  --market-address 0:<pmp> --pn-state-file <path>\n                  \
       Cancel all the note's resting orders on one market.\n  \
       cancel-stake  --market-address 0:<pmp> --pn-state-file <path>\n                  \
       Recover a stake from a still-open STAKING market.\n  \
       merge-full-set  --market-address 0:<pmp> --pn-state-file <path> --amounts a,b[,..]\n                  \
       Merge held outcome tokens back into collateral (per-outcome amounts).\n  \
       claim         --market-address 0:<pmp> --pn-state-file <path>\n                  \
       Settle/claim a RESOLVED or CANCELLED market.\n  \
       withdraw      --pn-state-file <path> --dest 0:<multisig> [--dapp-id <id>]\n                  \
       Sweep the note's free collateral to a wallet (the multisig).\n  \
       pmp-details   --market-address 0:<pmp> [--endpoint host]\n                  \
       Read a market's phase window, outcomes, and identity (read-only).\n  \
       create-prediction-market  --pn-state-file <path> [--name <prefix>] \
     [--outcomes \"A,B\"] [--result-start <unix>] [--endpoint host]\n                  \
       Deploy a fresh oracle + event + PMP (one prediction market) using the funded \
     deployer note, then open STAKING (oracle submitSetTimings). Prints \
     predictionMarketAddress + oracleSecretHex (keep — needed to resolve) + windows.\n                  \
       --result-start is the contract's resultStart param (unix seconds); must be \
     >= now + 120 (MIN_RESULT_GAP). The contract derives stakeEnd = stakeStart + \
     (resultStart-stakeStart)/10 and resultEnd = resultStart + 86400, so the STAKING \
     window is ~1/10 of (resultStart - now). Default: now + 3000 (~5-min window).\n\n\
     common flags:\n  \
       --endpoint    network host (default shellnet.ackinacki.org)\n"
        .to_string()
}

async fn dispatch(sub: &str, rest: &[String]) -> ExitCode {
    let flags = match Flags::parse(rest) {
        Ok(f) => f,
        Err(e) => return fail(&format!("{e}\n\n{}", usage())),
    };
    match sub {
        "stake" => cmd_stake(flags).await,
        "stakes" => cmd_stakes(flags).await,
        "place-order" => cmd_place_order(flags).await,
        "cancel-all-orders" => cmd_cancel_all_orders(flags).await,
        "cancel-stake" => cmd_cancel_stake(flags).await,
        "merge-full-set" => cmd_merge_full_set(flags).await,
        "claim" => cmd_claim(flags).await,
        "withdraw" => cmd_withdraw(flags).await,
        "pmp-details" => cmd_pmp_details(flags).await,
        "create-prediction-market" => cmd_create_prediction_market(flags).await,
        other => fail(&format!("unknown subcommand `{other}`\n\n{}", usage())),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.split_first() {
        None => {
            eprintln!("{}", usage());
            ExitCode::from(2)
        }
        Some((sub, _)) if sub == "--help" || sub == "-h" => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Some((sub, rest)) => dispatch(sub, rest).await,
    }
}
