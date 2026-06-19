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
