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
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_sdk::Dex;
use dodex_sdk::DexConfig;
use serde::Deserialize;

const DEFAULT_ENDPOINT: &str = "shellnet.ackinacki.org";

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
       pmp-details   --market-address 0:<pmp> [--endpoint host]\n                  \
       Read a market's phase window, outcomes, and identity (read-only).\n\n\
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
        "pmp-details" => cmd_pmp_details(flags).await,
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
