use std::sync::Arc;

use ackinacki_kit::contracts::dex::private_note::PrivateNote;
use ackinacki_kit::contracts::dex::private_note_events::DecodedPrivateNoteEvent;
use ackinacki_kit::contracts::dex::private_note_events::PrivateNoteEvent;
use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::contracts::traits::FromEvent;
use ackinacki_kit::tvm_client::ClientContext;
use serde::Deserialize;
use serde_json::json;

use crate::dapp::account_query_vars;
use crate::dapp::dex_contract_params;
use crate::errors::AppError;
use crate::errors::AppResult;

const GQL_EVENTS_QUERY: &str = r#"
    query($address: String!, $last: Int!, $before: String) {
      blockchain {
        account(address: $address) {
          events(last: $last, before: $before) {
            edges {
              node {
                msg_id
                created_at
                dst
                body
              }
            }
            pageInfo {
              startCursor
              hasPreviousPage
            }
          }
        }
      }
    }
"#;

const GQL_EVENTS_QUERY_V3: &str = r#"
    query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
      blockchain {
        account(account_id: $accountId, dapp_id: $dappId) {
          events(last: $last, before: $before) {
            edges {
              node {
                msg_id
                created_at
                dst
                body
              }
            }
            pageInfo {
              startCursor
              hasPreviousPage
            }
          }
        }
      }
    }
"#;

#[derive(Debug, Deserialize)]
struct GqlResponse {
    data: GqlData,
}
#[derive(Debug, Deserialize)]
struct GqlData {
    blockchain: GqlBlockchain,
}
#[derive(Debug, Deserialize)]
struct GqlBlockchain {
    account: GqlAccount,
}
#[derive(Debug, Deserialize)]
struct GqlAccount {
    events: GqlEvents,
}
#[derive(Debug, Deserialize)]
struct GqlEvents {
    edges: Vec<GqlEdge>,
    #[serde(rename = "pageInfo")]
    page_info: RawPageInfo,
}
#[derive(Debug, Deserialize)]
struct GqlEdge {
    node: GqlEventNode,
}
#[derive(Debug, Deserialize)]
struct GqlEventNode {
    msg_id: String,
    created_at: u64,
    dst: String,
    body: String,
}
#[derive(Debug, Deserialize)]
struct RawPageInfo {
    #[serde(rename = "startCursor")]
    start_cursor: Option<String>,
    #[serde(rename = "hasPreviousPage")]
    has_previous_page: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PageInfo {
    pub end_cursor: Option<String>,
    pub has_next_page: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NoteEvent {
    pub pn_address: String,
    pub event_type: String,
    pub created_at: u64,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NotesHistoryPage {
    pub events: Vec<NoteEvent>,
    pub page_info: PageInfo,
}

fn decode_event_to_note_event(
    pn_address: &str,
    event: &Event,
    pn_contract: &PrivateNote,
) -> Option<NoteEvent> {
    let _kind = PrivateNoteEvent::try_from(event.dst.clone()).ok()?;
    let decoded = DecodedPrivateNoteEvent::from_event(event, pn_contract).ok()?;

    let (event_type, data) = match decoded {
        DecodedPrivateNoteEvent::PmpDeployed { data, .. } => (
            "PmpDeployed",
            serde_json::json!({
                "event_id": data.event_id,
                "token_type": data.token_type,
                "pmp_address": data.pmp_address,
            }),
        ),
        DecodedPrivateNoteEvent::OwnerChanged { data, .. } => (
            "OwnerChanged",
            serde_json::json!({
                "old_pubkey": data.old_pubkey,
                "new_pubkey": data.new_pubkey,
            }),
        ),
        DecodedPrivateNoteEvent::StakeConfirmed { data, .. } => (
            "StakeConfirmed",
            serde_json::json!({
                "stake_controller": data.stake_controller,
                "outcome": data.outcome,
                "amount": data.amount,
                "bet_type": data.bet_type,
            }),
        ),
        DecodedPrivateNoteEvent::ClaimAccepted { data, .. } => (
            "ClaimAccepted",
            serde_json::json!({
                "stake_controller": data.stake_controller,
                "outcome": data.outcome,
                "payout": data.payout,
            }),
        ),
        DecodedPrivateNoteEvent::StakeCancelled { data, .. } => (
            "StakeCancelled",
            serde_json::json!({
                "stake_controller": data.stake_controller,
                "value": data.value,
            }),
        ),
        DecodedPrivateNoteEvent::FullSetStakeConfirmed { data, .. } => (
            "FullSetStakeConfirmed",
            serde_json::json!({
                "stake_controller": data.stake_controller,
                "amount": data.amount,
            }),
        ),
        DecodedPrivateNoteEvent::FullSetStakeCancelled { data, .. } => (
            "FullSetStakeCancelled",
            serde_json::json!({
                "stake_controller": data.stake_controller,
                "value": data.value,
            }),
        ),
        DecodedPrivateNoteEvent::TransferInitiated { data, .. } => (
            "TransferInitiated",
            serde_json::json!({
                "dest": data.dest,
                "token_type": data.token_type,
                "amount": data.amount,
            }),
        ),
        DecodedPrivateNoteEvent::TransferReceived { data, .. } => (
            "TransferReceived",
            serde_json::json!({
                "from": data.from,
                "token_type": data.token_type,
                "amount": data.amount,
            }),
        ),
        DecodedPrivateNoteEvent::OrderSubmitted { data, .. } => (
            "OrderSubmitted",
            serde_json::json!({
                "client_order_id": data.client_order_id.to_string(),
                "outcome_id": data.outcome_id,
                "is_buy": data.is_buy,
                "price": data.price,
                "amount": data.amount.to_string(),
                "flags": data.flags,
                "event_id": data.event_id,
                "token_type": data.token_type,
            }),
        ),
        DecodedPrivateNoteEvent::OrderPlacedConfirmed { data, .. } => (
            "OrderPlacedConfirmed",
            serde_json::json!({
                "order_book": data.order_book,
                "order_id": data.order_id.to_string(),
                "client_order_id": data.client_order_id.to_string(),
                "outcome_id": data.outcome_id,
                "is_buy": data.is_buy,
                "flags": data.flags,
                "price": data.price,
                "amount": data.amount.to_string(),
            }),
        ),
        DecodedPrivateNoteEvent::OrderFilledConfirmed { data, .. } => (
            "OrderFilledConfirmed",
            serde_json::json!({
                "order_book": data.order_book,
                "order_id": data.order_id.to_string(),
                "outcome_id": data.outcome_id,
                "filled_amount": data.filled_amount.to_string(),
                "clearing_price": data.clearing_price,
                "is_buy": data.is_buy,
                "fee_amount": data.fee_amount.to_string(),
                "is_rebate": data.is_rebate,
                "is_final": data.is_final,
            }),
        ),
        DecodedPrivateNoteEvent::OrderCancelledConfirmed { data, .. } => (
            "OrderCancelledConfirmed",
            serde_json::json!({
                "order_book": data.order_book,
                "order_id": data.order_id.to_string(),
                "outcome_id": data.outcome_id,
                "is_buy": data.is_buy,
                "return_amount": data.return_amount.to_string(),
            }),
        ),
        DecodedPrivateNoteEvent::OrderPlaceRejected { data, .. } => (
            "OrderPlaceRejected",
            serde_json::json!({
                "order_book": data.order_book,
                "event_id": data.event_id,
                "client_order_id": data.client_order_id.to_string(),
                "outcome_id": data.outcome_id,
                "is_buy": data.is_buy,
                "flags": data.flags,
                "price": data.price,
                "amount": data.amount.to_string(),
                "op_nonce": data.op_nonce.to_string(),
            }),
        ),
    };

    Some(NoteEvent {
        pn_address: pn_address.to_string(),
        event_type: event_type.to_string(),
        created_at: event.created_at,
        data,
    })
}

pub async fn get_notes_history(
    tvm_client: Arc<ClientContext>,
    pn_addresses: &[String],
    limit: u32,
    cursor: Option<String>,
) -> AppResult<NotesHistoryPage> {
    let mut all_events: Vec<NoteEvent> = Vec::new();
    let mut last_page_info = PageInfo { end_cursor: None, has_next_page: false };

    let dapp_id_api = tvm_client
        .supports_dapp_id()
        .await
        .map_err(|e| AppError::from(e).with_context("detect gateway version"))?;
    let query = if dapp_id_api { GQL_EVENTS_QUERY_V3 } else { GQL_EVENTS_QUERY };

    for pn_address in pn_addresses {
        let pn_contract = PrivateNote::new(tvm_client.clone(), dex_contract_params(pn_address));

        let mut variables = account_query_vars(dapp_id_api, pn_address);
        variables.insert("last".to_string(), json!(limit));
        variables.insert("before".to_string(), json!(cursor));
        let variables = serde_json::Value::Object(variables);

        let result = ackinacki_kit::tvm_client::net::query(
            tvm_client.clone(),
            ackinacki_kit::tvm_client::net::ParamsOfQuery {
                query: query.to_string(),
                variables: Some(variables),
            },
        )
        .await
        .map_err(|e| {
            AppError::from(e).with_context(format!("PN history query for {pn_address}"))
        })?;

        let resp: GqlResponse = serde_json::from_value(result.result)
            .map_err(|e| AppError::from(e).with_context("Parse PN history response"))?;

        let GqlEvents { edges, page_info } = resp.data.blockchain.account.events;

        for edge in &edges {
            let node = &edge.node;
            let event = Event {
                id: node.msg_id.clone(),
                dst: node.dst.clone(),
                created_at: node.created_at,
                body: node.body.clone(),
            };

            if let Some(note_event) = decode_event_to_note_event(pn_address, &event, &pn_contract) {
                all_events.push(note_event);
            }
        }

        if page_info.has_previous_page {
            last_page_info.has_next_page = true;
            last_page_info.end_cursor = page_info.start_cursor;
        }
    }

    // Sort by created_at desc
    all_events.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Truncate to limit
    all_events.truncate(limit as usize);

    Ok(NotesHistoryPage { events: all_events, page_info: last_page_info })
}
