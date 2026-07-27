//! `ChainReader` — the single read path for on-chain account state: raw BOC,
//! physical balance, and decoded storage fields.
//!
//! It owns one `Dex` + tvm client + GraphQL client trio, built from a single
//! endpoint, and the scenarios drive their writes through the same `dex`/`ctx`
//! rather than constructing a second pair: a scenario that wrote over one
//! connection set and verified over another would be comparing two views that
//! need not agree, and a disagreement would read as a defect in the contracts.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::Context;
use dodex_infrastructure::graphql::GraphqlClient;
use dodex_infrastructure::tvm_runner::account_boc_is_none;
use dodex_infrastructure::tvm_runner::decode_account_ecc;
use dodex_infrastructure::tvm_runner::decode_account_fields_json;
use dodex_infrastructure::tvm_runner::AccountEcc;
use dodex_sdk::Dex;
use dodex_sdk::DexConfig;
use serde_json::Value;

use crate::common::context::network_endpoint;

const GRAPHQL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ChainReader {
    pub dex: Dex,
    pub ctx: Arc<ClientContext>,
    pub gql: GraphqlClient,
    /// Copy of the constructed GraphQL endpoint. `GraphqlClient` keeps its
    /// endpoint private, so this is the only way to assert on it in tests.
    gql_endpoint: String,
}

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

    /// Whether `addr` holds no account. Two structurally different shapes
    /// mean the same thing: the node answers 404 (never deployed), or it
    /// answers with a BOC that decodes to `AccountNone` — which is what a
    /// contract that lived and then self-destructed keeps returning. A caller
    /// that only checked the fetch would read the second as "still alive" and
    /// wait out its whole timeout on a self-destruct that already happened.
    ///
    /// Decided structurally in both cases, never by matching an error string.
    pub async fn account_absent(&self, addr: &str) -> anyhow::Result<bool> {
        match self
            .account_boc(addr)
            .await
            .with_context(|| format!("fetch account BOC of {addr}"))?
        {
            None => Ok(true),
            Some(boc) => {
                account_boc_is_none(&boc).with_context(|| format!("decode account state of {addr}"))
            }
        }
    }

    /// Decoded contract storage fields (the ABI `fields` section), read
    /// straight off the account state — no getter call. `None` when the
    /// account is absent by [`ChainReader::account_absent`]'s definition:
    /// unlike the physical balance, storage has no well-defined value there,
    /// and callers differ on what absence means to them — a barrier waiting
    /// for a contract to disappear treats it as success, the preflight treats
    /// it as a broken stand. Reporting it structurally lets each say so at
    /// its own call site instead of deciding here for both.
    pub async fn storage_fields(
        &self,
        addr: &str,
        abi_json: &str,
    ) -> anyhow::Result<Option<Value>> {
        let Some(boc) =
            self.account_boc(addr).await.with_context(|| format!("fetch account BOC of {addr}"))?
        else {
            return Ok(None);
        };
        if account_boc_is_none(&boc).with_context(|| format!("decode account state of {addr}"))? {
            return Ok(None);
        }
        decode_account_fields_json(abi_json, &boc)
            .with_context(|| format!("decode storage fields of {addr}"))
            .map(Some)
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
