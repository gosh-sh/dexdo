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
pub struct GraphqlPnStateReader {
    graphql: Arc<GraphqlClient>,
    abi: Arc<Contract>,
}

impl GraphqlPnStateReader {
    pub fn new(graphql: Arc<GraphqlClient>) -> anyhow::Result<Self> {
        let abi = Contract::load(Cursor::new(PN_ABI)).context("load PrivateNote ABI")?;
        Ok(Self { graphql, abi: Arc::new(abi) })
    }

    async fn fetch_boc(&self, pn_address: &str) -> anyhow::Result<String> {
        self.graphql
            .fetch_account_boc(pn_address)
            .await
            .with_context(|| format!("fetch BOC for {pn_address}"))?
            // Distinct typed variant so the use case can preserve it
            // through its map_err chain and the API surface as 404
            // instead of conflating with gateway/parse failures (503).
            .ok_or_else(|| {
                anyhow::Error::from(dodex_domain::DomainError::AccountNotDeployed)
                    .context(format!("PN not deployed at {pn_address}"))
            })
    }
}

#[async_trait]
impl PnStateReader for GraphqlPnStateReader {
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
///
/// Exposed so HTTP integration tests can push raw `getDetails`-shaped
/// payloads through the same boundary validation the production reader
/// applies — otherwise a relaxation of `read_uint_map` would slip
/// through the test suite green.
pub fn details_from_value(v: &Value) -> anyhow::Result<PnDetails> {
    let balance = read_uint_map(v, "balance")?;
    let locked = read_uint_map(v, "lockedInOrders")?;
    Ok(PnDetails { balance, locked_in_orders: locked })
}

/// Locate `stake_hash` inside the detokenized `_stakes` map and parse
/// it into `PnStake`. Returns `Ok(None)` when the key is absent.
///
/// Exposed alongside [`details_from_value`] for the same reason — see
/// that doc.
pub fn stake_from_value(v: &Value, stake_hash: &str) -> anyhow::Result<Option<PnStake>> {
    let map = v
        .get("_stakes")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("_stakes reply has no `_stakes` object"))?;
    // tvm_abi serializes uint256 map keys as `0x` + 64-char zero-padded
    // lowercase hex. `stake_hash` from infrastructure::tvm_hash always
    // produces that exact shape, so a direct lookup is sufficient — no
    // per-version fallback.
    let Some(entry) = map.get(stake_hash) else { return Ok(None) };
    let amount = read_uint_array(entry, "amount")?;
    let debt = read_uint_array(entry, "debtAmount")?;
    let coupons = read_uint_array(entry, "couponsAmount")?;
    Ok(Some(PnStake { amount, debt_amount: debt, coupons_amount: coupons }))
}

fn read_uint_map(v: &Value, key: &str) -> anyhow::Result<Vec<(u32, String)>> {
    let obj = v
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing `{key}` in getDetails reply"))?;
    let mut out = Vec::with_capacity(obj.len());
    for (k, val) in obj {
        // Chain ABI emits `uint32` keys; parse straight into u32 so a
        // negative or out-of-range value fails closed at the boundary
        // rather than poisoning the rest of the read path.
        let token_type: u32 = k.parse().with_context(|| format!("parse `{key}` key: {k}"))?;
        let amount = val.as_str().ok_or_else(|| anyhow!("`{key}[{k}]` is not a string"))?;
        // Same posture as `read_uint_array`: chain ABI emits `uint128`
        // amounts as a non-empty decimal literal ("0" at minimum).
        // Validate at the boundary so a corrupt value cannot survive
        // into `scale_decimal`'s BigUint::from_str two layers later.
        if amount.is_empty() {
            return Err(anyhow!("`{key}[{k}]` is empty"));
        }
        if amount.bytes().any(|b| !b.is_ascii_digit()) {
            return Err(anyhow!("`{key}[{k}]` is not a non-negative integer literal: {amount}"));
        }
        out.push((token_type, amount.to_string()));
    }
    Ok(out)
}

fn read_uint_array(v: &Value, key: &str) -> anyhow::Result<Vec<String>> {
    let arr = v
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing array `{key}` in stake reply"))?;
    arr.iter()
        .enumerate()
        .map(|(i, x)| {
            let s = x.as_str().ok_or_else(|| anyhow!("`{key}[{i}]` is not a string"))?;
            // Chain ABI emits uint256 as a non-empty decimal literal ("0" at
            // minimum). An empty string would otherwise survive into
            // add_decimal_strs and coerce silently to 0, hiding read-model
            // corruption.
            if s.is_empty() {
                return Err(anyhow!("`{key}[{i}]` is empty"));
            }
            Ok(s.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
        let lock: std::collections::HashMap<_, _> = d.locked_in_orders.into_iter().collect();
        assert_eq!(lock.get(&1), Some(&"1500000000".to_string()));
        assert_eq!(lock.get(&3), None);
    }

    #[test]
    fn details_from_value_rejects_empty_balance_amount() {
        let v = json!({
            "balance": { "1": "" },
            "lockedInOrders": {}
        });
        let err = details_from_value(&v).unwrap_err().to_string();
        assert!(err.contains("balance[1]"), "got: {err}");
    }

    #[test]
    fn details_from_value_rejects_non_digit_balance_amount() {
        let v = json!({
            "balance": { "2": "10abc" },
            "lockedInOrders": {}
        });
        let err = details_from_value(&v).unwrap_err().to_string();
        assert!(err.contains("balance[2]"), "got: {err}");
        assert!(err.contains("non-negative integer"), "got: {err}");
    }

    #[test]
    fn stake_from_value_rejects_empty_amount_element() {
        use num_bigint::BigUint;

        use crate::tvm_hash::stake_hash;

        let key = stake_hash(&BigUint::from(0x42u32), &BigUint::from(0x24u32), 1).unwrap();
        let v = serde_json::json!({
            "_stakes": {
                key.clone(): {
                    "amount": ["10", ""],
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
        let err = stake_from_value(&v, &key).unwrap_err().to_string();
        assert!(err.contains("amount[1]"), "got: {err}");
    }

    #[test]
    fn stake_from_value_returns_none_for_missing_key() {
        let v = json!({ "_stakes": {} });
        assert!(stake_from_value(&v, "0xdeadbeef").unwrap().is_none());
    }

    #[test]
    fn stake_from_value_finds_entry_keyed_by_real_stake_hash() {
        use num_bigint::BigUint;

        use crate::tvm_hash::stake_hash;

        // Compute the same 0x+64-hex shape tvm_abi produces for uint256 map
        // keys via the production hasher. This pins the test JSON to whatever
        // tvm_abi actually emits.
        let key = stake_hash(&BigUint::from(0x42u32), &BigUint::from(0x24u32), 1).unwrap();
        let v = serde_json::json!({
            "_stakes": {
                key.clone(): {
                    "amount": ["10", "5"],
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
        let s = stake_from_value(&v, &key).unwrap().expect("present");
        assert_eq!(s.amount, vec!["10", "5"]);
        assert_eq!(s.debt_amount, vec!["0", "0"]);
        assert_eq!(s.coupons_amount, vec!["0", "0"]);
    }
}
