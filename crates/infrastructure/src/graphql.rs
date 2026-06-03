// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use ackinacki_kit::contracts::dapp::SystemDapp;
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

const SERVER_INFO_QUERY: &str = r#"query ServerInfo {
  info {
    version
  }
}"#;

// A GraphQL gateway < 1.0.0 keys account lookups on the raw address; >= 1.0.0
// drops that form and keys on (account_id, dapp_id). The shape of the result
// is unchanged, so both share the same response decode below.
const ACCOUNT_BOC_QUERY: &str = r#"query AccountBoc($address: String!) {
  blockchain {
    account(address: $address) {
      info {
        boc
      }
    }
  }
}"#;

const ACCOUNT_BOC_QUERY_V3: &str = r#"query AccountBocV3($accountId: String!, $dappId: String!) {
  blockchain {
    account(account_id: $accountId, dapp_id: $dappId) {
      info {
        boc
      }
    }
  }
}"#;

// How long a resolved gateway version is trusted before re-probing. Bounds
// the window in which a live 0.9.x -> >= 1.0.0 upgrade keeps us on the stale
// account query form.
const VERSION_PROBE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct GraphqlClient {
    http: Client,
    endpoint: String,
    // Cached `info.version` verdict with the instant it was taken, shared
    // across clones. Re-probed after `VERSION_PROBE_TTL` (and dropped on an
    // account-fetch error) so a gateway that upgrades while the indexer runs
    // flips the query form at runtime without a restart.
    dapp_id_support: Arc<Mutex<Option<(bool, Instant)>>>,
}

impl GraphqlClient {
    pub fn new(endpoint: impl Into<String>, request_timeout: Duration) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(request_timeout)
            .user_agent(concat!("dodex-indexer/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build http client")?;
        Ok(Self { http, endpoint: endpoint.into(), dapp_id_support: Arc::new(Mutex::new(None)) })
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
    pub async fn fetch_account_boc(&self, address: &str) -> anyhow::Result<Option<String>> {
        let result = self.account_boc(address).await;
        if result.is_err() {
            // A live gateway version flip makes the in-flight query form wrong;
            // forget the cached probe so the next call re-detects instead of
            // repeating the failure for the whole TTL window.
            *self.dapp_id_support.lock().unwrap() = None;
        }
        result
    }

    async fn account_boc(&self, address: &str) -> anyhow::Result<Option<String>> {
        let payload = if self.supports_dapp_id().await? {
            // DEX contracts live under the System dApp (all-zero id); `account_id`
            // is the address without its `0:` workchain prefix.
            let account_id = address.strip_prefix("0:").unwrap_or(address);
            json!({
                "query": ACCOUNT_BOC_QUERY_V3,
                "variables": { "accountId": account_id, "dappId": SystemDapp::System.dapp_id() },
            })
        } else {
            json!({
                "query": ACCOUNT_BOC_QUERY,
                "variables": { "address": address },
            })
        };

        let response: GraphqlResponse<AccountBocData> = self
            .http
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .context("graphql account request failed")?
            .error_for_status()
            .context("graphql account returned http error")?
            .json()
            .await
            .context("graphql account response is not valid json")?;

        if let Some(errors) = response.errors
            && !errors.is_empty()
        {
            bail!("graphql account errors: {errors:?}");
        }

        let data = response.data.context("graphql account response missing data")?;
        Ok(data.blockchain.account.and_then(|a| a.info.and_then(|i| i.boc)))
    }

    /// Whether the connected gateway speaks the `>= 1.0.0` dApp-ID account
    /// API. Cached for `VERSION_PROBE_TTL` then re-probed, so a gateway that
    /// upgrades while the indexer runs is picked up without a restart. On a
    /// probe failure the last known verdict is reused; only a cold cache
    /// surfaces the error.
    async fn supports_dapp_id(&self) -> anyhow::Result<bool> {
        {
            let guard = self.dapp_id_support.lock().unwrap();
            if let Some((value, at)) = *guard
                && at.elapsed() < VERSION_PROBE_TTL
            {
                return Ok(value);
            }
        }
        match self.probe_dapp_id_support().await {
            Ok(value) => {
                *self.dapp_id_support.lock().unwrap() = Some((value, Instant::now()));
                Ok(value)
            }
            Err(e) => {
                let last = self.dapp_id_support.lock().unwrap().map(|(value, _)| value);
                last.ok_or(e)
            }
        }
    }

    async fn probe_dapp_id_support(&self) -> anyhow::Result<bool> {
        let payload = json!({ "query": SERVER_INFO_QUERY });

        let response: GraphqlResponse<ServerInfoData> = self
            .http
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .context("graphql server-info request failed")?
            .error_for_status()
            .context("graphql server-info returned http error")?
            .json()
            .await
            .context("graphql server-info response is not valid json")?;

        if let Some(errors) = response.errors
            && !errors.is_empty()
        {
            bail!("graphql server-info errors: {errors:?}");
        }

        let version = response
            .data
            .and_then(|d| d.info)
            .and_then(|i| i.version)
            .context("graphql server-info missing info.version")?;
        Ok(version_supports_dapp_id(&version))
    }
}

/// Encodes a `major.minor.patch` string the way the node does
/// (`major*1_000_000 + minor*1_000 + patch`) and tests the `1.0.0` cutover
/// at which the account query switches to the dApp-ID form. Missing or
/// non-numeric components count as zero, so a malformed version reads as
/// legacy rather than panicking the indexer.
fn version_supports_dapp_id(version: &str) -> bool {
    let mut parts = version.split('.');
    let mut next = || parts.next().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0);
    let major = next();
    let minor = next();
    let patch = next();
    major * 1_000_000 + minor * 1_000 + patch >= 1_000_000
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

#[derive(Debug, Deserialize)]
struct AccountBocData {
    blockchain: AccountBocBlockchain,
}

#[derive(Debug, Deserialize)]
struct AccountBocBlockchain {
    #[serde(default)]
    account: Option<AccountBocNode>,
}

#[derive(Debug, Deserialize)]
struct AccountBocNode {
    #[serde(default)]
    info: Option<AccountBocInfo>,
}

#[derive(Debug, Deserialize)]
struct AccountBocInfo {
    #[serde(default)]
    boc: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServerInfoData {
    #[serde(default)]
    info: Option<ServerInfo>,
}

#[derive(Debug, Deserialize)]
struct ServerInfo {
    #[serde(default)]
    version: Option<String>,
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
    fn version_cutover_at_one_zero_zero() {
        assert!(!version_supports_dapp_id("0.9.0"));
        assert!(!version_supports_dapp_id("0.999.999"));
        assert!(version_supports_dapp_id("1.0.0"));
        assert!(version_supports_dapp_id("1.2.3"));
        assert!(version_supports_dapp_id("2.0.0"));
        // Short and malformed strings degrade to legacy, never panic.
        assert!(!version_supports_dapp_id("0.9"));
        assert!(!version_supports_dapp_id(""));
        assert!(!version_supports_dapp_id("garbage"));
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
