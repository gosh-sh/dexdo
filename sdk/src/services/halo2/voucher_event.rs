//! GraphQL helpers for capturing live `RootPN.VoucherGenerated` ext-out
//! messages and waiting for the chain to reach a desired block height.
//!
//! Mirrors the Python helpers used by acki-nacki integration tests
//! (`tests/dex/generate_vouchers_with_live_event_proving.py`).

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::contracts::KitResult;
use ackinacki_kit::tvm_client::net;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_contracts::dex::root_pn_events::VoucherGeneratedData;
use serde::Deserialize;

use crate::dapp::account_query_vars;
use crate::dapp::dex_contract_params;

/// External destination address every `VoucherGenerated` event lands on,
/// computed by `RootPN.sol::generateVoucher` as
/// `address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress)`.
pub const VOUCHER_EVENT_DST: &str =
    ":0000000000000000000000000000000000000000000000000000000000000087";

const MODULE: KitModule = KitModule::External("dex.root_pn");

const GQL_EXTOUT_MESSAGES: &str = r#"
    query($address: String!, $last: Int!) {
      blockchain {
        account(address: $address) {
          messages(msg_type: [ExtOut], last: $last) {
            edges {
              node {
                id
                boc
                body
                dst
                created_at
                src_transaction { id }
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
                boc
                body
                dst
                created_at
                src_transaction { id }
              }
            }
          }
        }
      }
    }
"#;

const GQL_TRANSACTION_BLOCK_ID: &str = r#"
    query($hash: String!) {
      blockchain {
        transaction(hash: $hash) { block_id }
      }
    }
"#;

const GQL_BLOCK_BY_HASH: &str = r#"
    query($hash: String!) {
      blockchain {
        block(hash: $hash) { seq_no }
      }
    }
"#;

const GQL_LATEST_BLOCK: &str = r#"
    query {
      blockchain {
        blocks(last: 1) {
          edges { node { seq_no } }
        }
      }
    }
"#;

#[derive(Debug, Clone)]
pub struct VoucherExtoutMessage {
    pub id: String,
    pub boc: String,
    /// ABI-encoded message body. Required for `Event::decode` (e.g.
    /// extracting `skUCommit` to identify which voucher this event belongs
    /// to). Empty string when the indexer hasn't surfaced it yet.
    pub body: String,
    pub dst: String,
    pub created_at: u64,
    /// Set once the indexer has surfaced the source transaction's `block_id`.
    pub block_id: Option<String>,
}

/// Fetch the latest `last` ExtOut messages from `root_pn_address` filtered to
/// the `VoucherGenerated` destination. `block_id` is populated when
/// `with_block_id = true` (extra round-trip per message).
pub async fn fetch_extout_voucher_events(
    context: Arc<ClientContext>,
    root_pn_address: &str,
    last: u32,
    with_block_id: bool,
) -> KitResult<Vec<VoucherExtoutMessage>> {
    let dapp_id_api = context.supports_dapp_id().await.map_err(|e| {
        KitError::new(MODULE, KitErrorCode::QueryEvents, "Detect GraphQL server version")
            .with_tvm_error(e)
    })?;
    let query = if dapp_id_api { GQL_EXTOUT_MESSAGES_V3 } else { GQL_EXTOUT_MESSAGES };
    let mut variables = account_query_vars(dapp_id_api, root_pn_address);
    variables.insert("last".to_string(), serde_json::json!(last));
    let variables = serde_json::Value::Object(variables);
    let raw = net::query(
        context.clone(),
        net::ParamsOfQuery { query: query.to_string(), variables: Some(variables) },
    )
    .await
    .map_err(|e| {
        KitError::new(MODULE, KitErrorCode::QueryEvents, "Query RootPN ExtOut messages")
            .with_tvm_error(e)
    })?;

    let parsed: GqlExtoutResponse = serde_json::from_value(raw.result).map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::DeserializeFailed,
            format!("Deserialize ExtOut messages response ({e})"),
        )
    })?;

    let mut events: Vec<(VoucherExtoutMessage, Option<String>)> = parsed
        .data
        .blockchain
        .account
        .messages
        .edges
        .into_iter()
        .map(|e| e.node)
        .filter(|n| n.dst == VOUCHER_EVENT_DST)
        .map(|n| {
            let tx_id = n.src_transaction.and_then(|t| t.id);
            (
                VoucherExtoutMessage {
                    id: n.id,
                    boc: n.boc.unwrap_or_default(),
                    body: n.body.unwrap_or_default(),
                    dst: n.dst,
                    created_at: n.created_at.unwrap_or(0),
                    block_id: None,
                },
                tx_id,
            )
        })
        .collect();

    if with_block_id {
        // Indexer fills `src_transaction.id`/`block_id` a couple blocks behind
        // the message body, so the caller is expected to retry until block_id
        // is populated.
        for (msg, tx_id) in events.iter_mut() {
            let Some(tx_id) = tx_id.as_deref() else { continue };
            msg.block_id = fetch_transaction_block_id(context.clone(), tx_id).await?;
        }
    }

    Ok(events.into_iter().map(|(m, _)| m).collect())
}

async fn fetch_transaction_block_id(
    context: Arc<ClientContext>,
    tx_id: &str,
) -> KitResult<Option<String>> {
    let raw = net::query(
        context,
        net::ParamsOfQuery {
            query: GQL_TRANSACTION_BLOCK_ID.to_string(),
            variables: Some(serde_json::json!({ "hash": tx_id })),
        },
    )
    .await
    .map_err(|e| {
        KitError::new(MODULE, KitErrorCode::QueryEvents, "Query transaction block_id")
            .with_tvm_error(e)
    })?;

    let parsed: GqlTransactionResponse = serde_json::from_value(raw.result).map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::DeserializeFailed,
            format!("Deserialize transaction.block_id response ({e})"),
        )
    })?;

    Ok(parsed.data.blockchain.transaction.and_then(|t| t.block_id))
}

/// Decode a `VoucherExtoutMessage` body into `VoucherGeneratedData`.
/// Returns `Ok(None)` for messages whose `body` isn't yet surfaced by the
/// indexer or that don't carry a non-empty payload — the caller is expected
/// to retry. Returns `Err` only on real ABI decode failures (corrupt body,
/// schema drift between kit ABI and chain).
fn decode_voucher_generated(
    msg: &VoucherExtoutMessage,
    root_pn: &RootPn,
) -> KitResult<Option<VoucherGeneratedData>> {
    if msg.body.is_empty() {
        return Ok(None);
    }
    let event = Event {
        id: msg.id.clone(),
        dst: msg.dst.clone(),
        created_at: msg.created_at,
        body: msg.body.clone(),
    };
    event.decode::<VoucherGeneratedData>(root_pn)
}

/// Normalize a Poseidon commitment to canonical lowercase 0x-prefixed hex
/// for equality comparison. Voucher events store `skUCommit` as a decimal
/// or `0x`-prefixed hex string depending on encoding path; the caller may
/// hand us either format. Strip surrounding whitespace, drop optional
/// leading `0x`, lowercase. Decimal strings are returned as-is.
fn canonicalize_sk_u_commit(s: &str) -> String {
    let trimmed = s.trim();
    let bare = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")).unwrap_or(trimmed);
    bare.to_ascii_lowercase()
}

/// Find the `VoucherGenerated` ext-out event whose decoded body carries the
/// supplied `sk_u_commit` (Poseidon commitment of the caller's secret).
///
/// `sk_u_commit` is a 32-byte random Poseidon hash freshly generated for
/// every voucher mint, so this match is collision-free regardless of how
/// many other vouchers the chain produces concurrently. Use this in
/// preference to timestamp-window heuristics whenever the caller knows the
/// commitment up front (`make_voucher_proof` and `mint_voucher_via_multifactor`
/// both do).
///
/// Returns the event with `block_id` resolved (the halo2 prover needs it
/// for Stage A → Stage B handoff). Times out if either the indexer never
/// surfaces a matching event body or the source transaction's `block_id`
/// stays unindexed past `timeout`.
pub async fn wait_for_voucher_event_by_sk_u_commit(
    context: Arc<ClientContext>,
    root_pn_address: &str,
    sk_u_commit_hex: &str,
    timeout: Duration,
) -> KitResult<VoucherExtoutMessage> {
    let target = canonicalize_sk_u_commit(sk_u_commit_hex);
    let root_pn = RootPn::new(context.clone(), dex_contract_params(root_pn_address));
    let start = Instant::now();

    loop {
        // `with_block_id=true` so the returned event already carries the
        // src_transaction.block_id the halo2 prover needs.
        let events =
            fetch_extout_voucher_events(context.clone(), root_pn_address, 200, true).await?;

        for ev in &events {
            if ev.block_id.is_none() {
                continue;
            }
            // Soft-fail decode: indexer occasionally surfaces a message
            // before its body is indexed (empty `body`), which yields
            // `Ok(None)`. Retry on next poll iteration.
            let decoded = match decode_voucher_generated(ev, &root_pn) {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                // ABI decode error means the body doesn't match the kit's
                // VoucherGeneratedData schema — propagate so the test fails
                // loudly instead of looping forever.
                Err(e) => return Err(e),
            };
            if canonicalize_sk_u_commit(&decoded.sk_u_commit) == target {
                return Ok(ev.clone());
            }
        }

        if start.elapsed() >= timeout {
            return Err(KitError::new(
                MODULE,
                KitErrorCode::QueryEvents,
                format!(
                    "Timed out waiting for VoucherGenerated event with skUCommit={} \
                     within {}s ({} ext-out events scanned)",
                    sk_u_commit_hex,
                    timeout.as_secs(),
                    events.len(),
                ),
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Resolve a block hash to its sequence number (= block height).
pub async fn get_block_height_by_id(
    context: Arc<ClientContext>,
    block_id: &str,
) -> KitResult<Option<u64>> {
    let raw = net::query(
        context,
        net::ParamsOfQuery {
            query: GQL_BLOCK_BY_HASH.to_string(),
            variables: Some(serde_json::json!({ "hash": block_id })),
        },
    )
    .await
    .map_err(|e| {
        KitError::new(MODULE, KitErrorCode::QueryEvents, "Query block by hash").with_tvm_error(e)
    })?;

    let parsed: GqlBlockResponse = serde_json::from_value(raw.result).map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::DeserializeFailed,
            format!("Deserialize block-by-hash response ({e})"),
        )
    })?;

    Ok(parsed.data.blockchain.block.map(|b| b.seq_no))
}

/// Latest seq_no from `blockchain.blocks(last:1)`.
pub async fn get_latest_block_height(context: Arc<ClientContext>) -> KitResult<u64> {
    let raw = net::query(
        context,
        net::ParamsOfQuery { query: GQL_LATEST_BLOCK.to_string(), variables: None },
    )
    .await
    .map_err(|e| {
        KitError::new(MODULE, KitErrorCode::QueryEvents, "Query latest block height")
            .with_tvm_error(e)
    })?;

    let parsed: GqlLatestBlockResponse = serde_json::from_value(raw.result).map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::DeserializeFailed,
            format!("Deserialize latest-block response ({e})"),
        )
    })?;

    parsed.data.blockchain.blocks.edges.into_iter().next().map(|e| e.node.seq_no).ok_or_else(|| {
        KitError::new(MODULE, KitErrorCode::EmptyResult, "No blocks returned by GraphQL")
    })
}

/// Poll until chain height ≥ `target`. Returns the observed height.
pub async fn wait_for_block_height(
    context: Arc<ClientContext>,
    target: u64,
    timeout: Duration,
) -> KitResult<u64> {
    let start = Instant::now();
    loop {
        match get_latest_block_height(context.clone()).await {
            Ok(current) if current >= target => return Ok(current),
            Ok(_) | Err(_) => {}
        }
        if start.elapsed() >= timeout {
            return Err(KitError::new(
                MODULE,
                KitErrorCode::QueryEvents,
                format!("Timed out waiting for block height ≥ {target}"),
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── GraphQL response types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GqlExtoutResponse {
    data: GqlExtoutData,
}

#[derive(Debug, Deserialize)]
struct GqlExtoutData {
    blockchain: GqlExtoutBlockchain,
}

#[derive(Debug, Deserialize)]
struct GqlExtoutBlockchain {
    account: GqlExtoutAccount,
}

#[derive(Debug, Deserialize)]
struct GqlExtoutAccount {
    messages: GqlExtoutMessages,
}

#[derive(Debug, Deserialize)]
struct GqlExtoutMessages {
    edges: Vec<GqlExtoutEdge>,
}

#[derive(Debug, Deserialize)]
struct GqlExtoutEdge {
    node: GqlExtoutNode,
}

#[derive(Debug, Deserialize)]
struct GqlExtoutNode {
    id: String,
    boc: Option<String>,
    body: Option<String>,
    dst: String,
    created_at: Option<u64>,
    src_transaction: Option<GqlSrcTransaction>,
}

#[derive(Debug, Deserialize)]
struct GqlSrcTransaction {
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GqlTransactionResponse {
    data: GqlTransactionData,
}

#[derive(Debug, Deserialize)]
struct GqlTransactionData {
    blockchain: GqlTransactionBlockchain,
}

#[derive(Debug, Deserialize)]
struct GqlTransactionBlockchain {
    transaction: Option<GqlTransactionFields>,
}

#[derive(Debug, Deserialize)]
struct GqlTransactionFields {
    block_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GqlBlockResponse {
    data: GqlBlockData,
}

#[derive(Debug, Deserialize)]
struct GqlBlockData {
    blockchain: GqlBlockBlockchain,
}

#[derive(Debug, Deserialize)]
struct GqlBlockBlockchain {
    block: Option<GqlBlockFields>,
}

#[derive(Debug, Deserialize)]
struct GqlBlockFields {
    seq_no: u64,
}

#[derive(Debug, Deserialize)]
struct GqlLatestBlockResponse {
    data: GqlLatestBlockData,
}

#[derive(Debug, Deserialize)]
struct GqlLatestBlockData {
    blockchain: GqlLatestBlockBlockchain,
}

#[derive(Debug, Deserialize)]
struct GqlLatestBlockBlockchain {
    blocks: GqlLatestBlocks,
}

#[derive(Debug, Deserialize)]
struct GqlLatestBlocks {
    edges: Vec<GqlLatestBlockEdge>,
}

#[derive(Debug, Deserialize)]
struct GqlLatestBlockEdge {
    node: GqlBlockFields,
}
