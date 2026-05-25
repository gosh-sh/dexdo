// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// On-demand reader for PrivateNote chain state. Implements
// `dodex_application::PnStateReader` by fetching the PN BOC through the
// GraphQL gateway and running the requested getter off-chain through
// `tvm_runner::run_getter`.
//
// Detokenization helpers are intentionally pulled out as free functions
// (`details_from_value`, `stake_from_value`) so they can be unit-tested
// without a live network — the BOC-fetch path itself is covered by the
// /api/v1/account integration tests.

use std::io::Cursor;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Context;
use async_trait::async_trait;
use dodex_application::PnDetails;
use dodex_application::PnStake;
use dodex_application::PnStateReader;
use serde_json::json;
use serde_json::Value;
use tvm_abi::Contract;

use crate::graphql::GraphqlClient;
use crate::tvm_runner::run_getter;

const PN_ABI: &str = include_str!("../../../contracts/abi/dex/PrivateNote.abi.json");

#[derive(Clone)]
pub struct PostgresPnStateReader {
    graphql: Arc<GraphqlClient>,
    abi: Arc<Contract>,
}

impl PostgresPnStateReader {
    pub fn new(graphql: Arc<GraphqlClient>) -> anyhow::Result<Self> {
        let abi = Contract::load(Cursor::new(PN_ABI)).context("load PrivateNote ABI")?;
        Ok(Self { graphql, abi: Arc::new(abi) })
    }

    async fn fetch_boc(&self, pn_address: &str) -> anyhow::Result<String> {
        self.graphql
            .fetch_account_boc(pn_address)
            .await
            .with_context(|| format!("fetch BOC for {pn_address}"))?
            .ok_or_else(|| anyhow!("account is None — PN not deployed at {pn_address}"))
    }
}

#[async_trait]
impl PnStateReader for PostgresPnStateReader {
    async fn get_details(&self, pn_address: &str) -> anyhow::Result<PnDetails> {
        let boc = self.fetch_boc(pn_address).await?;
        let v = run_getter(&self.abi, &boc, "getDetails", &json!({}))
            .with_context(|| format!("getDetails for {pn_address}"))?;
        details_from_value(&v)
    }

    async fn get_stake(
        &self,
        pn_address: &str,
        stake_hash: &str,
    ) -> anyhow::Result<Option<PnStake>> {
        let boc = self.fetch_boc(pn_address).await?;
        let v = run_getter(&self.abi, &boc, "_stakes", &json!({}))
            .with_context(|| format!("_stakes for {pn_address}"))?;
        stake_from_value(&v, stake_hash)
    }
}

/// Parse the detokenized `getDetails()` reply into `PnDetails`. The
/// ABI shape (see contracts/abi/dex/PrivateNote.abi.json) emits
/// `map(uint32,uint128)` as a JSON object keyed by uint32 strings.
fn details_from_value(v: &Value) -> anyhow::Result<PnDetails> {
    let balance = read_uint_map(v, "balance")?;
    let locked = read_uint_map(v, "lockedInOrders")?;
    Ok(PnDetails { balance, locked_in_orders: locked })
}

/// Locate `stake_hash` inside the detokenized `_stakes` map and parse
/// it into `PnStake`. Returns `Ok(None)` when the key is absent.
fn stake_from_value(v: &Value, stake_hash: &str) -> anyhow::Result<Option<PnStake>> {
    let map = v
        .get("_stakes")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("_stakes reply has no `_stakes` object"))?;
    // tvm_abi detokenizes uint256 map keys as decimal strings AND/OR
    // 0x-prefixed hex strings depending on the encoder version. The
    // hash we pass in is 0x-prefixed; try that first, fall back to a
    // decimal-stringified BigUint conversion.
    let entry = if let Some(e) = map.get(stake_hash) {
        Some(e)
    } else if let Some(stripped) = stake_hash.strip_prefix("0x") {
        let as_decimal = num_bigint::BigUint::parse_bytes(stripped.as_bytes(), 16)
            .map(|b| b.to_string());
        as_decimal.as_deref().and_then(|k| map.get(k))
    } else {
        None
    };
    let Some(entry) = entry else { return Ok(None) };
    let amount = read_uint_array(entry, "amount")?;
    let debt = read_uint_array(entry, "debtAmount")?;
    let coupons = read_uint_array(entry, "couponsAmount")?;
    Ok(Some(PnStake { amount, debt_amount: debt, coupons_amount: coupons }))
}

fn read_uint_map(v: &Value, key: &str) -> anyhow::Result<Vec<(i32, String)>> {
    let obj = v
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing `{key}` in getDetails reply"))?;
    let mut out = Vec::with_capacity(obj.len());
    for (k, val) in obj {
        let token_type: i32 = k.parse().with_context(|| format!("parse `{key}` key: {k}"))?;
        let amount = val
            .as_str()
            .ok_or_else(|| anyhow!("`{key}[{k}]` is not a string"))?
            .to_string();
        out.push((token_type, amount));
    }
    Ok(out)
}

fn read_uint_array(v: &Value, key: &str) -> anyhow::Result<Vec<String>> {
    let arr = v
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing array `{key}` in stake reply"))?;
    arr.iter()
        .map(|x| {
            x.as_str()
                .ok_or_else(|| anyhow!("`{key}` element is not a string"))
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn details_from_value_parses_two_token_types() {
        let v = json!({
            "balance": { "1": "10000000000", "3": "25000000000" },
            "lockedInOrders": { "1": "1500000000" }
        });
        let d = details_from_value(&v).unwrap();
        // HashMap-order independent assertions.
        let bal: std::collections::HashMap<_, _> = d.balance.into_iter().collect();
        assert_eq!(bal.get(&1), Some(&"10000000000".to_string()));
        assert_eq!(bal.get(&3), Some(&"25000000000".to_string()));
        let lock: std::collections::HashMap<_, _> =
            d.locked_in_orders.into_iter().collect();
        assert_eq!(lock.get(&1), Some(&"1500000000".to_string()));
        assert_eq!(lock.get(&3), None);
    }

    #[test]
    fn stake_from_value_returns_none_for_missing_key() {
        let v = json!({ "_stakes": {} });
        assert!(stake_from_value(&v, "0xdeadbeef").unwrap().is_none());
    }

    #[test]
    fn stake_from_value_finds_hex_keyed_entry() {
        let v = json!({
            "_stakes": {
                "0xdeadbeef": {
                    "amount": ["1", "2"],
                    "debtAmount": ["0", "0"],
                    "couponsAmount": ["0", "0"],
                    "candidateAmount": "0",
                    "candidateOutcome": "0",
                    "candidateBetType": "0",
                    "tokenType": "1",
                    "oracleListHash": "0"
                }
            }
        });
        let s = stake_from_value(&v, "0xdeadbeef").unwrap().expect("present");
        assert_eq!(s.amount, vec!["1", "2"]);
        assert_eq!(s.debt_amount, vec!["0", "0"]);
        assert_eq!(s.coupons_amount, vec!["0", "0"]);
    }

    #[test]
    fn stake_from_value_finds_decimal_keyed_entry() {
        // 0xdeadbeef = 3735928559 in decimal — tvm_abi may emit either.
        let v = json!({
            "_stakes": {
                "3735928559": {
                    "amount": ["10"],
                    "debtAmount": ["0"],
                    "couponsAmount": ["0"],
                    "candidateAmount": "0",
                    "candidateOutcome": "0",
                    "candidateBetType": "0",
                    "tokenType": "1",
                    "oracleListHash": "0"
                }
            }
        });
        let s = stake_from_value(&v, "0xdeadbeef").unwrap().expect("present");
        assert_eq!(s.amount, vec!["10"]);
    }
}
