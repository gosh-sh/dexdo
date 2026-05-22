// Best-effort `cancel_order_by_client` for the e2e tests. The
// placement-flow shape is record-then-cancel-then-panic: when a
// request returns OK, the test may already have live orders on the
// chain, so failures accumulate in a `Vec<String>`, cleanup runs
// unconditionally, and a single combined panic fires at the end. This
// helper is the cleanup step — without it a panic between OK-response
// and the explicit cancel would leak collateral on the trading PN.
//
// The helper never panics — it sits on the cleanup path and a panic
// here would mask the original test failure. It also never reports
// upward: every per-attempt failure goes to `eprintln!`, which
// `cargo test` only surfaces when the *outer* test fails (stderr is
// captured per-test and replayed only on FAIL). On a green run, all
// cancel errors here are silent. **Callers MUST follow this call with
// a `getOrdersByOwner` absence-poll** so a silently-leaked order
// turns into a real test failure instead of locked collateral.
//
// Per-attempt timing: each `cancel_order_by_client` call is wrapped
// in a 30 s `tokio::time::timeout` so an unreachable shellnet endpoint
// cannot stall the call indefinitely. The 30 s mirrors the per-op
// budgets `BeeDexChainSender` uses in the production e2e setup.

#![allow(dead_code)]

use std::time::Duration;

use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use bee_dex::Dex;

use super::deploy_market::EphemeralMarket;
use super::test_pns::TestPn;

/// Per-attempt timeout for `cancel_order_by_client`. Bounds each
/// retry against a hung or unreachable shellnet endpoint; without it
/// the SDK call has no inherent ceiling and a single attempt could
/// stall the entire cleanup path.
const CANCEL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Gap between retry attempts. Matches the 2 s tick used in the
/// surface / absence polls so a transient shellnet hiccup gets a
/// fresh attempt at the next likely-healthy moment.
const CANCEL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Maximum cancel attempts per coid. Five gives roughly a minute of
/// total cover (worst case: 5 × 30 s timeout + 4 × 2 s backoff =
/// ~158 s) before the helper gives up and moves on.
const CANCEL_MAX_ATTEMPTS: u32 = 5;

/// Fire `cancel_order_by_client` for every coid in `coids` against the
/// shellnet `Dex`. Each coid gets up to `CANCEL_MAX_ATTEMPTS` tries
/// wrapped in a `CANCEL_ATTEMPT_TIMEOUT`, with `CANCEL_RETRY_BACKOFF`
/// between attempts. Logs and continues on every failure shape (chain
/// error, timeout) — the caller is already on the cleanup path and a
/// panic here would swallow the real test failure.
///
/// **The caller is responsible for verifying the order is actually
/// gone**: every error here lands in captured-stderr that only shows
/// up on a failing test. Follow this call with a `getOrdersByOwner`
/// absence-poll and turn "still present" into a recorded failure.
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
