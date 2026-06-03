// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.

use ackinacki_kit::contracts::account::ParamsOfNewContract;
use ackinacki_kit::contracts::dapp::SystemDapp;

// DEX contracts live under the System dApp (all-zero id). A gateway < 1.0.0
// ignores the field; a gateway >= 1.0.0 routes the account lookup by it.
// Centralised here so every DEX contract handle carries the same dApp.
pub fn dex_contract_params(address: impl Into<String>) -> ParamsOfNewContract {
    ParamsOfNewContract::new(address, SystemDapp::System)
}

// A GraphQL gateway >= 1.0.0 keys account lookups on (account_id, dapp_id)
// instead of the raw address. `account_id` is the address with its `0:`
// workchain prefix stripped.
pub(crate) fn account_id_of(address: &str) -> &str {
    address.strip_prefix("0:").unwrap_or(address)
}

// The DEX dApp ID carried by the >= 1.0.0 account query — same System
// placeholder the contract handles use; swap alongside `dex_contract_params`.
pub(crate) fn dex_dapp_id() -> &'static str {
    SystemDapp::System.dapp_id()
}
