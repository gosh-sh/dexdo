// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.

use ackinacki_kit::contracts::account::ParamsOfNewContract;
use ackinacki_kit::contracts::dapp::SystemDapp;

// DEX contracts live under the System dApp (all-zero id). A gateway < 1.0.0
// ignores the field; a gateway >= 1.0.0 routes the account lookup by it.
// Centralised here so every DEX contract handle carries the same dApp.
pub fn dex_contract_params(address: impl Into<String>) -> ParamsOfNewContract {
    ParamsOfNewContract::new(address, SystemDapp::System)
}

// A contract deployed by an external message (not by a parent contract) is the
// root of its own dApp: its `dapp_id` equals its bare account id. Lookups and
// messages on `>= 1.0.0` servers are dApp-scoped, so such a contract must be
// addressed under that id, not the System dApp. The AI Registry `TokenContract`
// is deployed this way.
pub fn self_rooted_contract_params(address: impl Into<String>) -> ParamsOfNewContract {
    let address = address.into();
    let dapp_id = address.trim_start_matches("0:").to_string();
    ParamsOfNewContract::new(address, dapp_id)
}
