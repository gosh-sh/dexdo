use std::collections::HashSet;
use std::sync::Arc;

use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::contracts::dex::root_pn_events::PrivateNoteDeployedData;
use ackinacki_kit::contracts::dex::root_pn_events::RootPnEvent;
use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::tvm_client::ClientContext;
use serde::Deserialize;
use serde_json::json;

use crate::dapp::account_query_vars;
use crate::dapp::dex_contract_params;
use crate::errors::AppError;
use crate::errors::AppResult;

const GQL_DEPLOY_EVENTS_QUERY: &str = r#"
    query($address: String!, $dst: String!, $last: Int!, $before: String) {
      blockchain {
        account(address: $address) {
          events(dst: $dst, last: $last, before: $before) {
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

const GQL_DEPLOY_EVENTS_QUERY_V3: &str = r#"
    query($accountId: String!, $dappId: String!, $dst: String!, $last: Int!, $before: String) {
      blockchain {
        account(account_id: $accountId, dapp_id: $dappId) {
          events(dst: $dst, last: $last, before: $before) {
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
    page_info: GqlPageInfo,
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
struct GqlPageInfo {
    #[serde(rename = "startCursor")]
    start_cursor: Option<String>,
    #[serde(rename = "hasPreviousPage")]
    has_previous_page: bool,
}

/// Discovered PrivateNote matching a user's derived public key.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoveredNote {
    pub deposit_identifier_hash: String,
    pub note_address: String,
    pub initial_balance: u128,
    /// The ephemeral pubkey registered on this PN (decimal string).
    /// Match against your `pubkey_to_index` map to determine the derivation
    /// path index.
    pub ephemeral_pubkey: String,
    /// Token type of the PN deposit (1=NACKL, 2=SHELL, 3=USDC).
    /// Determined from the non-empty key in `balance` map.
    pub token_type: Option<u32>,
}

/// Scan RootPN deploy events and return notes whose ephemeral_pubkey matches
/// one of the provided derived public keys.
///
/// `derived_pubkeys_dec` — set of decimal pubkey strings derived from the seed
/// phrase.
pub async fn discover_private_notes(
    tvm_client: Arc<ClientContext>,
    derived_pubkeys_dec: &HashSet<String>,
) -> AppResult<Vec<DiscoveredNote>> {
    let root_pn = RootPn::new(tvm_client.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let dst_filter = RootPnEvent::PrivateNoteDeployed.to_address().replacen("0:", ":", 1);

    let dapp_id_api = tvm_client
        .supports_dapp_id()
        .await
        .map_err(|e| AppError::from(e).with_context("detect gateway version"))?;
    let query = if dapp_id_api { GQL_DEPLOY_EVENTS_QUERY_V3 } else { GQL_DEPLOY_EVENTS_QUERY };

    let mut discovered = Vec::new();
    let mut cursor: Option<String> = None;
    let page_size = 50;

    // Paginate through all deploy events
    loop {
        let mut variables = account_query_vars(dapp_id_api, RootPn::DEFAULT_ADDRESS);
        variables.insert("dst".to_string(), json!(dst_filter));
        variables.insert("last".to_string(), json!(page_size));
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
        .map_err(|e| AppError::from(e).with_context("RootPN deploy events query failed"))?;

        let resp: GqlResponse = serde_json::from_value(result.result)
            .map_err(|e| AppError::from(e).with_context("Parse deploy events GQL response"))?;

        let GqlEvents { edges, page_info } = resp.data.blockchain.account.events;

        for edge in &edges {
            let node = &edge.node;
            let event = Event {
                id: node.msg_id.clone(),
                dst: node.dst.clone(),
                created_at: node.created_at,
                body: node.body.clone(),
            };

            if let Ok(Some(data)) = event.decode::<PrivateNoteDeployedData>(&root_pn) {
                // Check if this note belongs to one of our keys by querying its details
                let pn = ackinacki_kit::contracts::dex::private_note::PrivateNote::new(
                    tvm_client.clone(),
                    dex_contract_params(&data.note_address),
                );
                if let Ok(details) = pn.get_details().await {
                    let epk_dec =
                        crate::services::proof::hex_u256_to_dec(&details.ephemeral_pubkey);
                    if derived_pubkeys_dec.contains(&epk_dec) {
                        let token_type = details
                            .balance
                            .iter()
                            .find(|(_, v)| **v > 0)
                            .and_then(|(k, _)| k.parse::<u32>().ok());
                        discovered.push(DiscoveredNote {
                            deposit_identifier_hash: data.deposit_identifier_hash,
                            note_address: data.note_address,
                            ephemeral_pubkey: epk_dec,
                            initial_balance: data.initial_balance,
                            token_type,
                        });
                    }
                }
            }
        }

        if !page_info.has_previous_page || edges.is_empty() {
            break;
        }
        cursor = page_info.start_cursor;
    }

    Ok(discovered)
}
