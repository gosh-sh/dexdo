//! Network constants, TVM client context, Dex client construction.

use std::sync::Arc;

use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_sdk::Dex;

pub const ENDPOINT: &str = "shellnet.ackinacki.org";
pub const TOKEN_TYPE_NACKL: u32 = dodex_sdk::proof::TokenType::Nackl as u32;
pub const VAULT_DEPOSIT: u64 = 100_000_000_000; // 100 NACKL (Nominal::N100)
pub const ECC_SHELL_DEPOSIT: u64 = 100_000_000_000; // 100 ECC shell (Nominal::N100)
pub const PMP_DEPOSIT: u64 = 1_000_000_000_000; // 1000 NACKL (Nominal::N1000) — enough for initial stakes + regular stake
pub const DEPLOYER_SEED_AMOUNT: u128 = 100_000_000_000; // 100 NACKL per outcome
pub const STAKE_AMOUNT: u128 = 200_000_000;
pub const STAKE_OUTCOME: u32 = 0;
pub const ORACLE_FEE: u128 = 100;
// Must clear the PMP `MIN_RESULT_GAP` (60s) gate with slack: `setTimings` runs
// seconds after this client-side `now`, so an exact 60 races the gate (ERR
// 129). The derived stake window is 10% of the period, so the value also has to
// leave room for the staking step.
pub const STAKE_PERIOD: u64 = 180;
pub const STAKE_PERIOD_LONG: u64 = 300; // 5 min — stake window = 30 sec for multi-step tests
pub const CURRENCY_ID_SHELL: u32 = 2;
pub const CURRENCY_ID_NACKL: u32 = 1;
pub const GIVER_ADDRESS: &str =
    "0:1111111111111111111111111111111111111111111111111111111111111111";

pub fn create_context() -> Arc<ClientContext> {
    let mut config = ClientConfig::default();
    config.network.endpoints = Some(vec![ENDPOINT.to_string()]);
    Arc::new(ClientContext::new(config).expect("create context"))
}

pub fn create_dex() -> Dex {
    Dex::new(dodex_sdk::DexConfig { endpoints: vec![ENDPOINT.to_string()], ..Default::default() })
        .expect("create Dex")
}
