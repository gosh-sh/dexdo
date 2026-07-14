// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.

use ackinacki_kit::contracts::account::ParamsOfNewContract;

/// dApp hosting the DEX contracts, mirroring `ROOT_PN_DAPP_ID` and
/// `ORACLE_DAPP_ID` in `contracts/dex/modifiers/modifiers.sol`. The chain keeps
/// the two systems (RootPN / PrivateNote / PMP / OrderBook, and RootOracle /
/// Oracle / OracleEventList) as separate constants that currently hold the same
/// value; should they ever diverge, this helper has to split along with them.
///
/// `SystemDapp` in the kit has no variant for it — it only covers the all-zero
/// System dApp plus MobileVerifiers and AuthService — so the id is spelled out.
/// A gateway < 1.0.0 ignores the field; a gateway >= 1.0.0 routes the account
/// lookup by it, so an id that does not match the deployment makes every send
/// and every account read miss the shard and report the account as stateless.
pub const DEX_DAPP_ID: &str = "0000000000000000000000000000000000000000000000000000000000000004";

// Centralised here so every DEX contract handle carries the same dApp.
pub fn dex_contract_params(address: impl Into<String>) -> ParamsOfNewContract {
    ParamsOfNewContract::new(address, DEX_DAPP_ID)
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
