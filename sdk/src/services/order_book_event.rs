//! GraphQL helpers for capturing live OrderBook ext-out events
//! (`OrderPlaced=143`, `OrderCancelled=144`, `OrderFilled=146`) and
//! waiting for a specific event by id.
//!
//! Events flow OB → external addresses (`0:000...{kind:064x}`); we
//! query `blockchain.account(address: $ob).messages(msg_type: ExtOut)`
//! and filter `dst` to the requested kind's synthetic address.
//!
//! Patterned after `voucher_event.rs`. Same retry/back-off behavior:
//! the indexer surfaces `id`/`dst`/`created_at` first and `body` a
//! moment later; we soft-fail decode (`Ok(None)`) while body is empty
//! and retry on next poll iteration.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use ackinacki_kit::contracts::dex::order_book::OrderBook;
use ackinacki_kit::contracts::dex::order_book_events::OrderBookEvent;
use ackinacki_kit::contracts::dex::order_book_events::OrderCancelledData;
use ackinacki_kit::contracts::dex::order_book_events::OrderFilledData;
use ackinacki_kit::contracts::dex::order_book_events::OrderPlacedData;
use ackinacki_kit::contracts::error::DexModule;
use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::contracts::KitResult;
use ackinacki_kit::tvm_client::net;
use ackinacki_kit::tvm_client::ClientContext;
use serde::Deserialize;

use crate::dapp::account_id_of;
use crate::dapp::dex_contract_params;
use crate::dapp::dex_dapp_id;

const MODULE: KitModule = KitModule::Dex(DexModule::OrderBook);

const GQL_EXTOUT_MESSAGES: &str = r#"
    query($address: String!, $last: Int!) {
      blockchain {
        account(address: $address) {
          messages(msg_type: [ExtOut], last: $last) {
            edges {
              node {
                id
                body
                dst
                created_at
              }
            }
          }
        }
      }
    }
"#;

const GQL_EXTOUT_MESSAGES_V3: &str = r#"
    query($accountId: String!, $dappId: String!, $last: Int!) {
      blockchain {
        account(account_id: $accountId, dapp_id: $dappId) {
          messages(msg_type: [ExtOut], last: $last) {
            edges {
              node {
                id
                body
                dst
                created_at
              }
            }
          }
        }
      }
    }
"#;

#[derive(Debug, Clone)]
pub struct OrderBookExtoutMessage {
    pub id: String,
    pub body: String,
    pub dst: String,
    pub created_at: u64,
    pub kind: OrderBookEvent,
}

/// `addr_extern` destination string for an OrderBook event kind, in the
/// format the chain actually puts in ext-out `dst`: leading colon, NO
/// workchain (ext-out destinations are `addr_extern`, not `addr_std`).
/// Kit's `OrderBookEvent::to_address()` returns `"0:..."` which is the
/// wrong format for this filter; its `Display` impl yields `":..."`
/// which matches what voucher_event uses for `RootPN.VoucherGenerated`.
fn ext_dst_for_kind(kind: OrderBookEvent) -> String {
    format!("{kind}") // uses Display impl: ":{value:064x}"
}

/// Fetch the latest `last` ExtOut messages from `ob_address` filtered to
/// the requested OrderBook event kind's synthetic destination.
pub async fn fetch_extout_events_for_kind(
    context: Arc<ClientContext>,
    ob_address: &str,
    kind: OrderBookEvent,
    last: u32,
) -> KitResult<Vec<OrderBookExtoutMessage>> {
    let target_dst = ext_dst_for_kind(kind);
    let dapp_id_api = ackinacki_kit::contracts::dapp::supports_dapp_id(&context, MODULE).await?;
    let query = if dapp_id_api { GQL_EXTOUT_MESSAGES_V3 } else { GQL_EXTOUT_MESSAGES };
    let variables = if dapp_id_api {
        serde_json::json!({
            "accountId": account_id_of(ob_address),
            "dappId": dex_dapp_id(),
            "last": last,
        })
    } else {
        serde_json::json!({
            "address": ob_address,
            "last": last,
        })
    };
    let raw = net::query(
        context,
        net::ParamsOfQuery {
            query: query.to_string(),
            variables: Some(variables),
        },
    )
    .await
    .map_err(|e| {
        KitError::new(MODULE, KitErrorCode::QueryEvents, "Query OrderBook ExtOut messages")
            .with_tvm_error(e)
    })?;

    let parsed: GqlExtoutResponse = serde_json::from_value(raw.result).map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::DeserializeFailed,
            format!("Deserialize ExtOut messages response ({e})"),
        )
    })?;

    Ok(parsed
        .data
        .blockchain
        .account
        .messages
        .edges
        .into_iter()
        .map(|e| e.node)
        .filter(|n| n.dst == target_dst)
        .map(|n| OrderBookExtoutMessage {
            id: n.id,
            body: n.body.unwrap_or_default(),
            dst: n.dst,
            created_at: n.created_at.unwrap_or(0),
            kind,
        })
        .collect())
}

fn decode<T: serde::de::DeserializeOwned>(
    msg: &OrderBookExtoutMessage,
    ob: &OrderBook,
) -> KitResult<Option<T>> {
    if msg.body.is_empty() {
        return Ok(None);
    }
    let event = Event {
        id: msg.id.clone(),
        dst: msg.dst.clone(),
        created_at: msg.created_at,
        body: msg.body.clone(),
    };
    event.decode::<T>(ob)
}

/// Wait for an `OrderPlaced` event whose decoded body carries the given
/// `client_order_id` AND whose `created_at` is `>= min_created_at` (set
/// to 0 to disable the filter).
///
/// `client_order_id` is unique per (deposit_hash, market) at any given
/// time, but the chain retains historical OrderPlaced events forever.
/// After cancel/fill, the slot frees up and a future test can reuse the
/// same coid — the historical match would then race ahead of the new
/// placement and the caller sees state that doesn't match the event.
/// Pass `now_unix()` as `min_created_at` at the start of each test to
/// scope the wait to the current run.
pub async fn wait_for_order_placed_by_client_id(
    context: Arc<ClientContext>,
    ob_address: &str,
    client_order_id: u128,
    min_created_at: u64,
    timeout: Duration,
) -> KitResult<OrderPlacedData> {
    let ob = OrderBook::new(context.clone(), dex_contract_params(ob_address));
    let start = Instant::now();
    loop {
        let events = fetch_extout_events_for_kind(
            context.clone(),
            ob_address,
            OrderBookEvent::OrderPlaced,
            200,
        )
        .await?;

        for ev in &events {
            if ev.created_at < min_created_at {
                continue;
            }
            match decode::<OrderPlacedData>(ev, &ob) {
                Ok(Some(d)) if d.client_order_id == client_order_id => return Ok(d),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }

        if start.elapsed() >= timeout {
            return Err(KitError::new(
                MODULE,
                KitErrorCode::QueryEvents,
                format!(
                    "Timed out waiting for OrderPlaced event with client_order_id={} on {} \
                     within {}s ({} OrderPlaced events scanned)",
                    client_order_id,
                    ob_address,
                    timeout.as_secs(),
                    events.len(),
                ),
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Wait for an `OrderCancelled` event for a given `client_order_id`,
/// filtered to `created_at >= min_created_at` (use 0 to disable).
/// See [`wait_for_order_placed_by_client_id`] for why the filter exists.
pub async fn wait_for_order_cancelled_by_client_id(
    context: Arc<ClientContext>,
    ob_address: &str,
    client_order_id: u128,
    min_created_at: u64,
    timeout: Duration,
) -> KitResult<OrderCancelledData> {
    let ob = OrderBook::new(context.clone(), dex_contract_params(ob_address));
    let start = Instant::now();
    loop {
        let events = fetch_extout_events_for_kind(
            context.clone(),
            ob_address,
            OrderBookEvent::OrderCancelled,
            200,
        )
        .await?;

        for ev in &events {
            if ev.created_at < min_created_at {
                continue;
            }
            match decode::<OrderCancelledData>(ev, &ob) {
                Ok(Some(d)) if d.client_order_id == client_order_id => return Ok(d),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }

        if start.elapsed() >= timeout {
            return Err(KitError::new(
                MODULE,
                KitErrorCode::QueryEvents,
                format!(
                    "Timed out waiting for OrderCancelled event with client_order_id={} on {} \
                     within {}s",
                    client_order_id,
                    ob_address,
                    timeout.as_secs(),
                ),
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Wait for an `OrderFilled` event for a given chain-assigned `order_id`.
/// `OrderFilled` payload doesn't carry `client_order_id` (it's tied to
/// the OB-internal `order_id`); call `Dex::get_order_id_by_client` or
/// `wait_for_order_placed_by_client_id` first to resolve.
///
/// Note: a single order can produce multiple `OrderFilled` events
/// (partial fills). This returns the FIRST event matching the
/// `order_id`. To collect all fills, call repeatedly with growing
/// timeouts or use `fetch_extout_events_for_kind` directly.
pub async fn wait_for_order_filled_by_order_id(
    context: Arc<ClientContext>,
    ob_address: &str,
    order_id: u128,
    min_created_at: u64,
    timeout: Duration,
) -> KitResult<OrderFilledData> {
    let ob = OrderBook::new(context.clone(), dex_contract_params(ob_address));
    let start = Instant::now();
    loop {
        let events = fetch_extout_events_for_kind(
            context.clone(),
            ob_address,
            OrderBookEvent::OrderFilled,
            200,
        )
        .await?;

        for ev in &events {
            if ev.created_at < min_created_at {
                continue;
            }
            match decode::<OrderFilledData>(ev, &ob) {
                Ok(Some(d)) if d.order_id == order_id => return Ok(d),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }

        if start.elapsed() >= timeout {
            return Err(KitError::new(
                MODULE,
                KitErrorCode::QueryEvents,
                format!(
                    "Timed out waiting for OrderFilled event with order_id={} on {} within {}s",
                    order_id,
                    ob_address,
                    timeout.as_secs(),
                ),
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── GraphQL response shape (private) ───────────────────────────────

#[derive(Deserialize)]
struct GqlExtoutResponse {
    data: GqlData,
}
#[derive(Deserialize)]
struct GqlData {
    blockchain: GqlBlockchain,
}
#[derive(Deserialize)]
struct GqlBlockchain {
    account: GqlAccount,
}
#[derive(Deserialize)]
struct GqlAccount {
    messages: GqlMessages,
}
#[derive(Deserialize)]
struct GqlMessages {
    edges: Vec<GqlEdge>,
}
#[derive(Deserialize)]
struct GqlEdge {
    node: GqlNode,
}
#[derive(Deserialize)]
struct GqlNode {
    id: String,
    body: Option<String>,
    dst: String,
    created_at: Option<u64>,
}
