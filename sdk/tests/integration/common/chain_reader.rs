//! `ChainReader` — the single read path for on-chain account state: raw BOC,
//! physical balance, and decoded storage fields, all through one `Dex` + tvm
//! client + GraphQL client trio instead of assembling connections ad hoc per
//! test.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::anyhow;
use dodex_infrastructure::graphql::GraphqlClient;
use dodex_infrastructure::tvm_runner::decode_account_ecc;
use dodex_infrastructure::tvm_runner::decode_account_fields_json;
use dodex_infrastructure::tvm_runner::AccountEcc;
use dodex_sdk::Dex;
use dodex_sdk::DexConfig;

use crate::common::context::network_endpoint;

const GRAPHQL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[allow(dead_code)] // no test module consumes this yet; scenario tests land later
pub struct ChainReader {
    pub dex: Dex,
    pub ctx: Arc<ClientContext>,
    pub gql: GraphqlClient,
    /// Copy of the constructed GraphQL endpoint. `GraphqlClient` keeps its
    /// endpoint private, so this is the only way to assert on it in tests.
    gql_endpoint: String,
}

#[allow(dead_code)] // no test module consumes this yet; scenario tests land later
impl ChainReader {
    pub fn new() -> ChainReader {
        Self::with_endpoint(&network_endpoint())
    }

    /// Builds every reader from the same explicit endpoint rather than the
    /// process environment, so construction is testable without mutating
    /// `E2E_NETWORK_ENDPOINT` (unsafe under Rust 2024's `std::env::set_var`).
    pub fn with_endpoint(endpoint: &str) -> ChainReader {
        let mut config = ClientConfig::default();
        config.network.endpoints = Some(vec![endpoint.to_string()]);
        let ctx = Arc::new(ClientContext::new(config).expect("create tvm client context"));

        let dex =
            Dex::new(DexConfig { endpoints: vec![endpoint.to_string()], ..Default::default() })
                .expect("create Dex");

        let gql_endpoint = format!("{endpoint}/graphql");
        let gql = GraphqlClient::new(gql_endpoint.clone(), GRAPHQL_REQUEST_TIMEOUT)
            .expect("create GraphqlClient");

        ChainReader { dex, ctx, gql, gql_endpoint }
    }

    /// Raw account state BOC (base64). `Ok(None)` means the account does not
    /// exist on chain; that is `fetch_account_boc`'s only source of `None` (a
    /// 404), so any other failure still propagates as an error.
    pub async fn account_boc(&self, addr: &str) -> anyhow::Result<Option<String>> {
        self.gql.fetch_account_boc(addr).await
    }

    /// Physical balance (grams + ECC dictionary). A nonexistent account has a
    /// well-defined zero balance, so `None` maps to zero rather than erroring.
    pub async fn account_ecc(&self, addr: &str) -> anyhow::Result<AccountEcc> {
        match self.account_boc(addr).await? {
            Some(boc) => decode_account_ecc(&boc),
            None => Ok(AccountEcc { grams: 0, ecc: BTreeMap::new() }),
        }
    }

    /// Decoded contract storage fields (the ABI `fields` section), read
    /// straight off the account state — no getter call. Unlike the physical
    /// balance, storage has no well-defined value for a nonexistent account.
    pub async fn storage_fields(
        &self,
        addr: &str,
        abi_json: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let boc = self
            .account_boc(addr)
            .await?
            .ok_or_else(|| anyhow!("account {addr} does not exist"))?;
        decode_account_fields_json(abi_json, &boc)
    }
}

#[cfg(test)]
mod tests {
    use super::ChainReader;

    #[test]
    fn chain_reader_builds_gql_endpoint_from_explicit_endpoint() {
        let r = ChainReader::with_endpoint("http://127.0.0.1");
        assert_eq!(r.gql_endpoint, "http://127.0.0.1/graphql");
    }
}
