// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.

use ackinacki_kit::contracts::account::ParamsOfNewContract;
use ackinacki_kit::contracts::dapp::SystemDapp;

// DEX contracts live under the Dex dApp (`SystemDapp::Dex`, id …0004). A gateway
// < 1.0.0 ignores the field; a gateway >= 1.0.0 routes the account lookup by it.
// Centralised here so every DEX contract handle carries the same dApp.
pub fn dex_contract_params(address: impl Into<String>) -> ParamsOfNewContract {
    ParamsOfNewContract::new(address, SystemDapp::Dex)
}

// A GraphQL gateway >= 1.0.0 keys account lookups on (account_id, dapp_id)
// instead of the raw address. `account_id` is the address with its `0:`
// workchain prefix stripped.
pub(crate) fn account_id_of(address: &str) -> &str {
    address.strip_prefix("0:").unwrap_or(address)
}

// The DEX dApp ID carried by the >= 1.0.0 account query — the same Dex dApp the
// contract handles use; swap alongside `dex_contract_params`.
pub(crate) fn dex_dapp_id() -> &'static str {
    SystemDapp::Dex.dapp_id()
}

// Account-identifying GraphQL variables for a gateway account lookup,
// keyed on the gateway version. A gateway >= 1.0.0 keys on
// `(accountId, dappId)` with the `0:` prefix stripped; older gateways key
// on the raw `address`. Centralised so the SDK queries don't each
// hand-roll the key/strip choice — a wrong key or an unstripped `0:`
// silently returns empty pages instead of erroring. Callers add their own
// pagination/filter vars and MUST pair the map with the matching query
// (the `*_V3` form when `dapp_id_api`).
pub(crate) fn account_query_vars(
    dapp_id_api: bool,
    address: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut vars = serde_json::Map::new();
    if dapp_id_api {
        vars.insert("accountId".to_string(), account_id_of(address).into());
        vars.insert("dappId".to_string(), dex_dapp_id().into());
    } else {
        vars.insert("address".to_string(), address.into());
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_of_strips_only_leading_workchain_prefix() {
        assert_eq!(account_id_of("0:abc123"), "abc123");
        assert_eq!(account_id_of("abc123"), "abc123");
        assert_eq!(account_id_of("0:ab:cd"), "ab:cd");
    }

    #[test]
    fn account_query_vars_v3_keys_on_account_id_and_dapp_id() {
        let vars = account_query_vars(true, "0:deadbeef");
        assert_eq!(vars.get("accountId").and_then(|v| v.as_str()), Some("deadbeef"));
        assert_eq!(vars.get("dappId").and_then(|v| v.as_str()), Some(dex_dapp_id()));
        assert!(!vars.contains_key("address"), "V3 map must not carry `address`");
    }

    #[test]
    fn account_query_vars_legacy_keys_on_raw_address() {
        let vars = account_query_vars(false, "0:deadbeef");
        assert_eq!(vars.get("address").and_then(|v| v.as_str()), Some("0:deadbeef"));
        assert!(!vars.contains_key("accountId"), "legacy map must not carry `accountId`");
        assert!(!vars.contains_key("dappId"), "legacy map must not carry `dappId`");
    }
}
