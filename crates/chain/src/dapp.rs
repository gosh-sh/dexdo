// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.

use ackinacki_kit::contracts::account::ParamsOfNewContract;
use ackinacki_kit::contracts::dapp::SystemDapp;

// DEX contracts live under the System dApp (all-zero id). A gateway < 1.0.0
// ignores the field; a gateway >= 1.0.0 routes the account lookup by it.
// Centralised here so every DEX contract handle carries the same dApp.
pub fn dex_contract_params(address: impl Into<String>) -> ParamsOfNewContract {
    ParamsOfNewContract::new(address, SystemDapp::System)
}
