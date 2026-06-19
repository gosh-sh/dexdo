// Best-effort `cancel_order_by_client` plus the `getOrdersByOwner`
// poll helpers the e2e tests use to verify chain-side state. The
// cancel helper never panics and never reports upward — per-attempt
// errors go to `eprintln!`, which `cargo test` captures and replays
// only on FAIL, so a green run swallows them silently.
//
// Callers MUST follow `cancel_coids_best_effort` with an absence-poll
// (`poll_orders` returning `PollOutcome::Found` against the not-present
// predicate) when the call is the **canonical** cancel path under test
// — otherwise a leaked order shows up only as locked collateral on the
// trading PN. The one carve-out is a trailing **defence-in-depth**
// cleanup that runs after the test has already verified the orders are
// gone (e.g. a `cancelBatch` HTTP call followed by its own absence-poll
// upstream): there the helper is a no-op on the happy path and any
// leftover after a recorded failure is best-effort by design.

#![allow(dead_code)]

use std::time::Duration;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use dodex_chain::ChainError;
use dodex_chain::Dex;
use dodex_chain::OwnedOrder;
use dodex_contracts::dex::private_note::ParamsOfCancelOrderByClient;
use dodex_contracts::dex::private_note::ParamsOfMergeFullSet;
use dodex_infrastructure::tvm_hash::stake_hash;
use num_bigint::BigUint;

use super::deploy_market::EphemeralMarket;
use super::test_pns::TestPn;

/// Per-attempt timeout — bounds cleanup against a hung shellnet endpoint.
const CANCEL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

const CANCEL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

const CANCEL_MAX_ATTEMPTS: u32 = 5;

/// Total polling budget = `POLL_TICKS * POLL_TICK` = 60 s. Same shape
/// as the kit integration tests' chain-state polls.
const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 30;

/// Best-effort cleanup — logs and continues on every failure shape
/// (chain error, timeout); a panic here would swallow the real test
/// failure.
///
/// `label` disambiguates interleaved stderr across parallel e2e tests
/// when `cargo test --nocapture` bypasses per-test capture.
///
/// **The caller is responsible for verifying the order is actually
/// gone**: every error here lands in captured-stderr that only shows
/// up on a failing test. Follow this call with `poll_orders` against
/// the not-present predicate and turn `PollOutcome::NotFound` /
/// `PollOutcome::ChainSilent` into recorded failures.
pub async fn cancel_coids_best_effort(
    raw_dex: &Dex,
    trader: &TestPn,
    market: &EphemeralMarket,
    coids: &[u128],
    label: &str,
) {
    if coids.is_empty() {
        return;
    }
    let signer = Signer::Keys {
        keys: KeyPair {
            public: trader.owner_public_key_hex.clone(),
            secret: trader.owner_secret_key_hex.clone(),
        },
    };
    for &coid in coids {
        let params = ParamsOfCancelOrderByClient {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            client_order_id: coid,
        };
        for attempt in 1..=CANCEL_MAX_ATTEMPTS {
            let outcome = tokio::time::timeout(
                CANCEL_ATTEMPT_TIMEOUT,
                raw_dex.cancel_order_by_client(&trader.address, params.clone(), signer.clone()),
            )
            .await;
            match outcome {
                Ok(Ok(_)) => break,
                Ok(Err(err)) => {
                    eprintln!(
                        "[{label}] cleanup cancel coid={coid} attempt {attempt} failed: {err:?}",
                    );
                }
                Err(_elapsed) => {
                    eprintln!(
                        "[{label}] cleanup cancel coid={coid} attempt {attempt} timed out after {:?}",
                        CANCEL_ATTEMPT_TIMEOUT,
                    );
                }
            }
            if attempt < CANCEL_MAX_ATTEMPTS {
                tokio::time::sleep(CANCEL_RETRY_BACKOFF).await;
            }
        }
    }
}

/// Per-poll budget while waiting for `_busy` to clear after a merge: the
/// callback is a PN → PMP → `onMergeAccepted` round-trip, slower than a
/// single op, so allow ~120 s.
const BUSY_POLL_TICK: Duration = Duration::from_secs(2);
const BUSY_POLL_TICKS: u32 = 60;

/// Best-effort: merge the deployer note's leftover full-set outcome tokens
/// for `market` back into collateral, recovering the NACKL that the deploy's
/// `splitFullSet` locked into the throwaway market. The deployer note is
/// shared across the whole single-threaded suite, so without this each run
/// strands ~100+ NACKL and the note drains after a handful of runs.
///
/// Call AFTER the test's order cleanup, so canceled orders have already
/// returned their locked tokens to `_stakes[hash].amount`. Merges only
/// complete sets (`min` over outcomes); proceeds from filled orders already
/// sit in `_balance`. Never panics — every failure is logged to captured
/// stderr (surfaced only on a failing test). Waits for `_busy` to clear so
/// the next test's `deployPMP` is not rejected with `ERR_NOTE_BUSY`.
pub async fn reclaim_full_set_best_effort(
    raw_dex: &Dex,
    deployer: &TestPn,
    market: &EphemeralMarket,
    label: &str,
) {
    let (Some(event_id), Some(oracle_list_hash)) =
        (parse_uint256(&market.event_id), parse_uint256(&market.oracle_list_hash))
    else {
        eprintln!("[{label}] reclaim: unparseable market ids, skipping merge");
        return;
    };
    let key = match stake_hash(&event_id, &oracle_list_hash, market.token_type) {
        Ok(k) => k,
        Err(err) => {
            eprintln!("[{label}] reclaim: stake_hash failed: {err:?}");
            return;
        }
    };

    let stakes = match raw_dex.get_private_note_stakes(&deployer.address).await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("[{label}] reclaim: getStakes failed: {err:?}");
            return;
        }
    };
    let per_outcome: Vec<u128> = match stakes
        .stakes
        .get(&key)
        .and_then(|entry| entry.get("amount"))
        .and_then(|amount| amount.as_array())
    {
        Some(arr) => {
            let parsed: Option<Vec<u128>> =
                arr.iter().map(|v| v.as_str().and_then(|s| s.parse::<u128>().ok())).collect();
            match parsed {
                Some(v) if !v.is_empty() => v,
                _ => {
                    eprintln!("[{label}] reclaim: stake amounts unparseable, skipping merge");
                    return;
                }
            }
        }
        None => {
            eprintln!("[{label}] reclaim: no stake entry for this market — nothing to merge");
            return;
        }
    };

    let merge = per_outcome.iter().copied().min().unwrap_or(0);
    if merge == 0 {
        eprintln!("[{label}] reclaim: a leg holds 0 tokens — no full set to merge");
        return;
    }

    let signer = Signer::Keys {
        keys: KeyPair {
            public: deployer.owner_public_key_hex.clone(),
            secret: deployer.owner_secret_key_hex.clone(),
        },
    };
    eprintln!(
        "[{label}] reclaim: merging {merge} full set(s) across {} outcome(s) back to collateral",
        per_outcome.len(),
    );
    if let Err(err) = raw_dex
        .merge_full_set(
            &deployer.address,
            ParamsOfMergeFullSet {
                event_id: market.event_id.clone(),
                oracle_list_hash: market.oracle_list_hash.clone(),
                token_type: market.token_type,
                amount: vec![merge; per_outcome.len()],
            },
            signer,
        )
        .await
    {
        eprintln!("[{label}] reclaim: mergeFullSet failed: {err:?}");
        return;
    }

    // The shared note is single-threaded; wait for the merge callback to
    // clear `_busy` before the next test's deployPMP fires.
    for _ in 0..BUSY_POLL_TICKS {
        match raw_dex.get_private_note_details(&deployer.address).await {
            Ok(d) if d.busy_address.is_none() => return,
            Ok(_) => {}
            Err(err) => eprintln!("[{label}] reclaim: busy-poll getDetails failed: {err:?}"),
        }
        tokio::time::sleep(BUSY_POLL_TICK).await;
    }
    eprintln!("[{label}] reclaim: note still busy after merge — next deploy may hit ERR_NOTE_BUSY");
}

/// Parse a uint256 as `deploy_market` emits it on `EphemeralMarket`
/// (`0x`-prefixed hex; see `e2e_buy_full_set`), with a plain-decimal
/// fallback. `None` on malformed input — the caller then skips the merge.
fn parse_uint256(s: &str) -> Option<BigUint> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => BigUint::parse_bytes(hex.as_bytes(), 16),
        None => BigUint::parse_bytes(s.as_bytes(), 10),
    }
}

/// Three-way result of a chain-state poll. The `ChainSilent` variant
/// is what distinguishes "predicate not satisfied within budget" from
/// "we couldn't even reach the chain" — without it, a transient
/// shellnet outage across the whole 60 s window would silently degrade
/// to a "did not surface" message that points the finger at the wrong
/// thing.
#[must_use]
pub enum PollOutcome<T = ()> {
    Found(T),
    NotFound,
    ChainSilent,
}

/// Poll `getOrdersByOwner` every `POLL_TICK` for up to `POLL_TICKS`
/// ticks; return `Found(())` the first time `predicate` is true,
/// `NotFound` after the budget if the chain answered at least once,
/// `ChainSilent` if every call errored.
pub async fn poll_orders<P>(
    raw_dex: &Dex,
    market: &EphemeralMarket,
    trader: &TestPn,
    label: &str,
    predicate: P,
) -> PollOutcome
where
    P: Fn(&[OwnedOrder]) -> bool,
{
    poll_orders_find(raw_dex, market, trader, label, |orders| predicate(orders).then_some(())).await
}

/// Same loop as `poll_orders`, but the predicate returns the value to
/// extract from the matching order (e.g. a chain-assigned `order_id`).
/// `PollOutcome::Found(T)` carries the predicate's `Some(T)`.
pub async fn poll_orders_find<T, P>(
    raw_dex: &Dex,
    market: &EphemeralMarket,
    trader: &TestPn,
    label: &str,
    predicate: P,
) -> PollOutcome<T>
where
    P: Fn(&[OwnedOrder]) -> Option<T>,
{
    let mut observed_chain_state = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        let owned = match raw_dex
            .get_orders_by_owner(&market.order_book_address, trader.deposit_identifier_hash.clone())
            .await
        {
            Ok(o) => o,
            Err(err) => {
                // `Decode` means our DTO disagrees with the contract's
                // ABI shape — retrying the same call for 60s cannot
                // recover, so flag it distinctly from a transport/chain
                // error to keep debug sessions pointed at the schema.
                let kind = match &err {
                    ChainError::Decode(_) => "decode — server-state suspect",
                    ChainError::Kit(_) | ChainError::Client(_) => "transport/chain",
                };
                eprintln!("[{label}] get_orders_by_owner errored ({kind}, retry): {err:?}");
                continue;
            }
        };
        observed_chain_state = true;
        if let Some(value) = predicate(&owned.orders) {
            return PollOutcome::Found(value);
        }
    }
    if observed_chain_state {
        PollOutcome::NotFound
    } else {
        PollOutcome::ChainSilent
    }
}
