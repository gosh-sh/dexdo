use std::sync::Arc;

use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::oracle::Oracle;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::OracleEventList;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::root_oracle::RootOracle;
use dodex_contracts::dex::root_oracle_events::OracleDeployedData;
use dodex_contracts::dex::root_oracle_events::RootOracleEvent;
use dodex_contracts::dex::root_pn::ParamsOfGetPmpAddress;
use dodex_contracts::dex::root_pn::RootPn;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use crate::dapp::account_query_vars;
use crate::dapp::dex_contract_params;
use crate::errors::AppError;
use crate::errors::AppResult;

// ── GQL types (same pattern as discovery.rs) ─────────────────────

const GQL_EVENTS_QUERY: &str = r#"
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

const GQL_EVENTS_QUERY_V3: &str = r#"
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

// ── Public types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct OracleInfo {
    pub name: String,
    pub address: String,
    pub pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketInfo {
    pub pmp_address: String,
    pub event_id: String,
    pub event_name: String,
    pub oracle_name: String,
    pub oracle_fee: u128,
    pub token_type: u32,
    pub total_pool: u128,
    pub num_outcomes: u32,
    pub outcome_names: std::collections::HashMap<u32, String>,
    pub approved: bool,
    pub is_cancelled: bool,
    pub resolved_outcome: Option<u32>,
    pub stake_start: u64,
    pub stake_end: u64,
    pub result_start: u64,
    pub result_end: u64,
    pub frozen: bool,
}

// ── Discover oracles ─────────────────────────────────────────────

pub async fn discover_oracles(tvm_client: Arc<ClientContext>) -> AppResult<Vec<OracleInfo>> {
    let root_oracle =
        RootOracle::new(tvm_client.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS));
    let dst_filter = RootOracleEvent::OracleDeployed.to_address().replacen("0:", ":", 1);

    let dapp_id_api = tvm_client
        .supports_dapp_id()
        .await
        .map_err(|e| AppError::from(e).with_context("detect gateway version"))?;
    let query = if dapp_id_api { GQL_EVENTS_QUERY_V3 } else { GQL_EVENTS_QUERY };

    let mut oracles = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut variables = account_query_vars(dapp_id_api, RootOracle::DEFAULT_ADDRESS);
        variables.insert("dst".to_string(), json!(dst_filter));
        variables.insert("last".to_string(), json!(50));
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
        .map_err(|e| AppError::from(e).with_context("RootOracle events query failed"))?;

        let resp: GqlResponse = serde_json::from_value(result.result)
            .map_err(|e| AppError::from(e).with_context("Parse RootOracle events"))?;

        let GqlEvents { edges, page_info } = resp.data.blockchain.account.events;

        for edge in &edges {
            let node = &edge.node;
            let event = Event {
                id: node.msg_id.clone(),
                dst: node.dst.clone(),
                created_at: node.created_at,
                body: node.body.clone(),
            };

            if let Ok(Some(data)) = event.decode::<OracleDeployedData>(&root_oracle) {
                oracles.push(OracleInfo {
                    name: data.name,
                    address: data.oracle,
                    pubkey: data.pubkey,
                });
            }
        }

        if !page_info.has_previous_page || edges.is_empty() {
            break;
        }
        cursor = page_info.start_cursor;
    }

    Ok(oracles)
}

// ── Discover markets ─────────────────────────────────────────────

fn parse_event_fee(val: &serde_json::Value) -> Option<u128> {
    val.get("oracleFee")?.as_str()?.parse().ok()
}

fn parse_event_name(val: &serde_json::Value) -> Option<String> {
    val.get("eventName").and_then(|v| v.as_str()).map(|s| s.to_string())
}

pub async fn discover_markets(
    tvm_client: Arc<ClientContext>,
    token_type: u32,
) -> AppResult<Vec<MarketInfo>> {
    let oracles = discover_oracles(tvm_client.clone()).await?;
    let root_pn = RootPn::new(tvm_client.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));

    let mut markets = Vec::new();

    for oracle_info in &oracles {
        let oracle = Oracle::new(tvm_client.clone(), dex_contract_params(&oracle_info.address));

        // Get event list 0 address
        let el_address =
            match oracle.get_event_list_address(ParamsOfGetEventListAddress { index: 0 }).await {
                Ok(r) => r.address,
                Err(_) => continue,
            };

        let event_list = OracleEventList::new(tvm_client.clone(), dex_contract_params(&el_address));
        let events = match event_list.get_events().await {
            Ok(e) => e,
            Err(_) => continue,
        };

        for (event_id, event_val) in &events.events {
            let event_name = match parse_event_name(event_val) {
                Some(n) => n,
                None => continue,
            };
            let oracle_fee = parse_event_fee(event_val).unwrap_or(0);

            // Derive PMP address
            let pmp_address = match root_pn
                .get_pmp_address(ParamsOfGetPmpAddress {
                    event_id: event_id.clone(),
                    names: vec![oracle_info.name.clone()],
                    token_type,
                })
                .await
            {
                Ok(r) => r.pmp_address,
                Err(_) => continue,
            };

            // Check if PMP is deployed
            let pmp = Pmp::new(tvm_client.clone(), dex_contract_params(&pmp_address));
            let details = match pmp.get_details().await {
                Ok(d) => d,
                Err(_) => continue, // PMP not deployed for this event
            };

            markets.push(MarketInfo {
                pmp_address,
                event_id: event_id.clone(),
                event_name,
                oracle_name: oracle_info.name.clone(),
                oracle_fee,
                token_type,
                total_pool: details.total_pool,
                num_outcomes: details.num_outcomes,
                outcome_names: details.outcome_names,
                approved: details.approved,
                is_cancelled: details.is_cancelled,
                resolved_outcome: details.resolved_outcome,
                stake_start: details.stake_start,
                stake_end: details.stake_end,
                result_start: details.result_start,
                result_end: details.result_end,
                frozen: details.frozen,
            });
        }
    }

    Ok(markets)
}

/// Returns only active markets (approved, not cancelled, not resolved).
pub async fn discover_active_markets(
    tvm_client: Arc<ClientContext>,
    token_type: u32,
) -> AppResult<Vec<MarketInfo>> {
    let all = discover_markets(tvm_client, token_type).await?;
    Ok(all
        .into_iter()
        .filter(|m| m.approved && !m.is_cancelled && m.resolved_outcome.is_none())
        .collect())
}
