// Best-effort `cancel_order_by_client` for the e2e tests — never
// panics, never reports upward. Per-attempt errors go to `eprintln!`,
// which `cargo test` captures and replays only on FAIL, so a green
// run swallows them silently. **Callers MUST follow each call with a
// `getOrdersByOwner` absence-poll**, otherwise a leaked order shows
// up only as locked collateral on the trading PN.

#![allow(dead_code)]

use std::time::Duration;

use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use bee_dex::Dex;

use super::deploy_market::EphemeralMarket;
use super::test_pns::TestPn;

/// Per-attempt timeout — bounds cleanup against a hung shellnet endpoint.
const CANCEL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

const CANCEL_RETRY_BACKOFF: Duration = Duration::from_secs(2);

const CANCEL_MAX_ATTEMPTS: u32 = 5;

/// Best-effort cleanup — logs and continues on every failure shape
/// (chain error, timeout); a panic here would swallow the real test
/// failure.
///
/// `label` disambiguates interleaved stderr across parallel e2e tests
/// when `cargo test --nocapture` bypasses per-test capture.
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
