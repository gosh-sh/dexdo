// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::time::Duration;

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

const ACCOUNT_BOC_QUERY: &str = r#"query AccountBoc($address: String!) {
  blockchain {
    account(address: $address) {
      info {
        boc
      }
    }
  }
}"#;

#[derive(Debug, Clone)]
pub struct GraphqlClient {
    http: Client,
    endpoint: String,
}

impl GraphqlClient {
    pub fn new(endpoint: impl Into<String>, request_timeout: Duration) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(request_timeout)
            .user_agent(concat!("dodex-indexer/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build http client")?;
        Ok(Self { http, endpoint: endpoint.into() })
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

        if let Some(errors) = response.errors {
            if !errors.is_empty() {
                bail!("graphql errors: {errors:?}");
            }
        }

        let data = response.data.context("graphql response missing data")?;
        Ok(data.blockchain.events)
    }

    /// Fetches the account state BOC (base64) for off-chain getter execution.
    /// Returns `None` if the account does not exist or has not been deployed yet.
    pub async fn fetch_account_boc(&self, address: &str) -> anyhow::Result<Option<String>> {
        let payload = json!({
            "query": ACCOUNT_BOC_QUERY,
            "variables": { "address": address },
        });

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

        if let Some(errors) = response.errors {
            if !errors.is_empty() {
                bail!("graphql account errors: {errors:?}");
            }
        }

        let data = response.data.context("graphql account response missing data")?;
        Ok(data.blockchain.account.and_then(|a| a.info.and_then(|i| i.boc)))
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
