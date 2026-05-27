// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Chain-write/read facade over the public `ackinacki_kit` contract
// handles. Two consumers:
//
//   * `dodex-infrastructure::chain_sender` — production trader path
//     (`place_order` / `cancel_order` / `place_batch` / `cancel_batch`)
//     plus the order-book read used by e2e cleanup polling.
//
//   * `dodex-market-manager` + the api e2e tests — both spawn
//     ephemeral PMP + OrderBook setups via the deploy entry points
//     (`deploy_pmp`, `submit_set_timings`, `submit_resolve`, ...) and
//     pull this crate with `features = ["test-helpers"]`. The prod
//     api/infrastructure build leaves the feature off, so deploy
//     methods stay out of the request-handling binary.
//
// Each method instantiates the relevant kit contract handle on demand
// from a shared `Arc<ClientContext>`. No rate-limiter, no retries, no
// per-call deadline — those concerns live with the caller (e.g.
// `chain_sender` wraps each call in `tokio::time::timeout`).

mod client;
mod dto;
mod error;

#[cfg(feature = "test-helpers")]
mod test_helpers;

pub use client::Dex;
pub use dto::OwnedOrder;
pub use dto::OwnedOrders;
pub use error::ChainError;
pub use error::ChainResult;
#[cfg(feature = "test-helpers")]
pub use test_helpers::OracleEvents;
#[cfg(feature = "test-helpers")]
pub use test_helpers::PmpDetails;
