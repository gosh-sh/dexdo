// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use dodex_chain::DEX_DAPP_ID;
use ackinacki_kit::tvm_client::account::get_account;
use ackinacki_kit::tvm_client::account::ParamsOfGetAccount;
use ackinacki_kit::tvm_client::net::ErrorCode;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

const EVENTS_QUERY: &str = r#"query Events($first: Int!, $after: String) {
  blockchain {
    events(first: $first, after: $after) {
      edges {
        cursor
        node {
          msg_id
          msg_chain_order
          src
          src_dapp_id
          dst
          body
          created_at
        }
      }
      pageInfo {
        endCursor
        hasNextPage
      }
    }
  }
}"#;

#[derive(Debug, Clone)]
pub struct GraphqlClient {
    http: Client,
    endpoint: String,
    // Lazily-built tvm_client context for account-state reads. Account
    // BOCs are NOT fetched over GraphQL: the >= 1.0.0 gateway's
    // `account(){info{boc}}` sub-resolver hangs server-side, while the
    // REST `/v2/account` route `tvm_client::account::get_account` uses
    // works on both old and new gateways (it picks the `address=` vs
    // `account_id=&dapp_id=` wire form from the server version itself).
    tvm_ctx: Arc<OnceLock<Arc<ClientContext>>>,
}

impl GraphqlClient {
    pub fn new(endpoint: impl Into<String>, request_timeout: Duration) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(request_timeout)
            .user_agent(concat!("dodex-indexer/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build http client")?;
        Ok(Self { http, endpoint: endpoint.into(), tvm_ctx: Arc::new(OnceLock::new()) })
    }

    pub async fn fetch_events(
        &self,
        first: u32,
        after: Option<&str>,
    ) -> anyhow::Result<EventsPage> {
        let payload = json!({
            "query": EVENTS_QUERY,
            "variables": { "first": first, "after": after },
        });

        let response: GraphqlResponse<EventsData> = self
            .http
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .context("graphql request failed")?
            .error_for_status()
            .context("graphql returned http error")?
            .json()
            .await
            .context("graphql response is not valid json")?;

        if let Some(errors) = response.errors
            && !errors.is_empty()
        {
            bail!("graphql errors: {errors:?}");
        }

        let data = response.data.context("graphql response missing data")?;
        Ok(data.blockchain.events)
    }

    /// Fetches the account state BOC (base64) for off-chain getter execution.
    /// Returns `None` if the account does not exist or has not been deployed yet.
    ///
    /// Goes through `tvm_client::account::get_account` (REST `/v2/account`),
    /// not GraphQL — see the `tvm_ctx` field doc. Reads DEX contracts, so it
    /// scopes the lookup to [`DEX_DAPP_ID`]: the PMP addresses the market
    /// reconciler refreshes, and the inference order books, which `PrivateNote`
    /// deploys as its own children and which therefore inherit its dApp.
    /// `account_id` is the address without its `0:` workchain prefix.
    pub async fn fetch_account_boc(&self, address: &str) -> anyhow::Result<Option<String>> {
        let ctx = self.tvm_context()?;
        let account_id = address.strip_prefix("0:").unwrap_or(address);
        let params = ParamsOfGetAccount {
            account_id: account_id.to_string(),
            dapp_id: DEX_DAPP_ID.to_string(),
        };
        match get_account(ctx, params).await {
            Ok(result) => Ok(Some(result.boc)),
            // A missing (account, dApp) pair surfaces as HTTP 404, which
            // tvm_client maps to `ErrorCode::NotFound` — that, and only
            // that, is this method's `None`. Matching on the code rather
            // than a substring of the message keeps a transient 5xx (whose
            // body tvm_client echoes verbatim, and which can itself read
            // "not found" from a proxy/CDN error page) propagating as an
            // error instead of masquerading as a not-deployed account.
            Err(e) if e.code() == ErrorCode::NotFound as u32 => Ok(None),
            Err(e) => Err(anyhow!("get_account for {address}: {e}")),
        }
    }

    /// tvm_client context for `get_account`, built once per client from the
    /// GraphQL endpoint's host (`https://host/graphql` → `host`).
    fn tvm_context(&self) -> anyhow::Result<Arc<ClientContext>> {
        if let Some(ctx) = self.tvm_ctx.get() {
            return Ok(ctx.clone());
        }
        let host = self
            .endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow!("cannot derive gateway host from `{}`", self.endpoint))?;
        let mut config = ClientConfig::default();
        config.network.endpoints = Some(vec![host.to_string()]);
        let ctx = Arc::new(ClientContext::new(config).context("build tvm client context")?);
        // Two clones racing here both build a context; the loser's copy is
        // dropped. Harmless — construction is cheap and side-effect free.
        let _ = self.tvm_ctx.set(ctx.clone());
        Ok(ctx)
    }
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse<T> {
    #[serde(default = "Option::default")]
    data: Option<T>,
    #[serde(default)]
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GraphqlError {
    pub message: String,
    #[serde(default)]
    pub path: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct EventsData {
    blockchain: Blockchain,
}

#[derive(Debug, Deserialize)]
struct Blockchain {
    events: EventsPage,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventsPage {
    #[serde(default)]
    pub edges: Vec<EventEdge>,
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventEdge {
    pub cursor: String,
    pub node: EventNode,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventNode {
    pub msg_id: String,
    /// Global lex-sortable chain order. Required for strict projection
    /// ordering — `created_at` timestamps collide within a second and drift
    /// across shards, so any reproject sweep that orders on time can apply
    /// `OrderFilled` before its parent `OrderPlaced` and corrupt
    /// `live_orders` state. The GraphQL gateway returns this on every
    /// message edge; an event without it is unusable and the indexer drops
    /// the row with a warning.
    #[serde(default)]
    pub msg_chain_order: Option<String>,
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub src_dapp_id: Option<String>,
    #[serde(default)]
    pub dst: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub created_at: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PageInfo {
    #[serde(rename = "endCursor")]
    pub end_cursor: Option<String>,
    #[serde(rename = "hasNextPage")]
    pub has_next_page: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_events_response() {
        let raw = serde_json::json!({
            "data": {
                "blockchain": {
                    "events": {
                        "edges": [
                            {
                                "cursor": "cursor-1",
                                "node": {
                                    "msg_id": "msg-1",
                                    "msg_chain_order": "5f8000000000000003",
                                    "src": "0:src",
                                    "src_dapp_id": "dapp-1",
                                    "dst": "0:dst",
                                    "body": "te6ccg...",
                                    "created_at": 1710000000
                                }
                            }
                        ],
                        "pageInfo": {
                            "endCursor": "cursor-1",
                            "hasNextPage": false
                        }
                    }
                }
            }
        });

        let parsed: GraphqlResponse<EventsData> = serde_json::from_value(raw).unwrap();
        let page = parsed.data.unwrap().blockchain.events;
        assert_eq!(page.edges.len(), 1);
        assert_eq!(page.edges[0].node.msg_id, "msg-1");
        assert_eq!(page.edges[0].cursor, "cursor-1");
        assert_eq!(page.page_info.end_cursor.as_deref(), Some("cursor-1"));
        assert!(!page.page_info.has_next_page);
    }

    #[test]
    fn deserializes_response_with_nullable_node_fields() {
        let raw = serde_json::json!({
            "data": {
                "blockchain": {
                    "events": {
                        "edges": [
                            {
                                "cursor": "c",
                                "node": {
                                    "msg_id": "msg",
                                    "src": null,
                                    "src_dapp_id": null,
                                    "dst": null,
                                    "body": null,
                                    "created_at": null
                                }
                            }
                        ],
                        "pageInfo": { "endCursor": null, "hasNextPage": true }
                    }
                }
            }
        });

        let parsed: GraphqlResponse<EventsData> = serde_json::from_value(raw).unwrap();
        let page = parsed.data.unwrap().blockchain.events;
        assert!(page.edges[0].node.src.is_none());
        assert!(page.page_info.end_cursor.is_none());
        assert!(page.page_info.has_next_page);
    }

    #[test]
    fn surfaces_graphql_errors() {
        let raw = serde_json::json!({
            "errors": [{ "message": "oops" }]
        });

        let parsed: GraphqlResponse<EventsData> = serde_json::from_value(raw).unwrap();
        assert!(parsed.data.is_none());
        let errors = parsed.errors.unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "oops");
    }
}
