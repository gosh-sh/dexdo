// Best-effort `cancel_order_by_client` for the e2e tests. Used by the
// placement → poll → cancel flow to defuse the leak path where an
// assertion fires after orders are live on the chain but before the
// test's explicit cancel loop would otherwise run, leaving collateral
// locked on the trading PN.
//
// The helper never panics — it sits on the cleanup path and a panic
// here would mask the original test failure.

#![allow(dead_code)]

use std::time::Duration;

use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use bee_dex::Dex;

use super::deploy_market::EphemeralMarket;
use super::test_pns::TestPn;

/// Fire `cancel_order_by_client` for every coid in `coids` against the
/// shellnet `Dex`. Retries each up to 5 times with a 2 s gap. Logs and
/// continues on failure — the caller is already in cleanup and a panic
/// here would swallow the real test failure.
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
        for attempt in 1..=5 {
            match raw_dex
                .cancel_order_by_client(&trader.address, params.clone(), signer.clone())
                .await
            {
                Ok(_) => break,
                Err(err) => {
                    eprintln!(
                        "[{label}] cleanup cancel coid={coid} attempt {attempt} failed: {err:?}",
                    );
                    if attempt < 5 {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
    }
}
