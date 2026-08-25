//! Network constants, TVM client context, Dex client construction.

use std::sync::Arc;

use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_sdk::Dex;

/// Pure core: normalizes endpoint value; takes parameter so tests avoid env mutation.
fn endpoint_from(v: Option<&str>) -> String {
    match v {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "https://shellnet.ackinacki.org".to_string(),
    }
}

/// The e2e network endpoint — a full URL including the scheme (mirroring the
/// contract in `services/api/tests/common/e2e_setup.rs`). Consumers do not append the
/// scheme themselves. A bare host drives tvm_client to the REST `/v2/account` over
/// plain HTTP, which times out. Reads the `E2E_NETWORK_ENDPOINT` environment
/// variable; when unset or empty, returns the Shellnet default.
pub fn network_endpoint() -> String {
    endpoint_from(std::env::var("E2E_NETWORK_ENDPOINT").ok().as_deref())
}

pub const TOKEN_TYPE_NACKL: u32 = dodex_sdk::proof::TokenType::Nackl as u32;
pub const VAULT_DEPOSIT: u64 = 100_000_000_000; // 100 NACKL (Nominal::N100)
pub const ECC_SHELL_DEPOSIT: u64 = 100_000_000_000; // 100 ECC shell (Nominal::N100)
pub const PMP_DEPOSIT: u64 = 1_000_000_000_000; // 1000 NACKL (Nominal::N1000) — enough for initial stakes + regular stake
pub const DEPLOYER_SEED_AMOUNT: u128 = 100_000_000_000; // 100 NACKL per outcome
pub const STAKE_AMOUNT: u128 = 200_000_000;
#[allow(dead_code)]
pub const STAKE_OUTCOME: u32 = 0;
pub const ORACLE_FEE: u128 = 100;
/// 1 SHELL, the fixed network fee a `PrivateNote` attaches to `deployPMP` on
/// top of the oracle fees (`NETWORK_FEE_AMOUNT` in
/// `contracts/dex/modifiers/modifiers.sol`, spent at
/// `PrivateNote.sol`'s `deployPMP`). Mirrored here because a conservation
/// scenario has to state where the physical ECC went, and the value existed
/// only on the Solidity side.
pub const NETWORK_FEE_AMOUNT: u128 = 1_000_000_000;
// Must clear the PMP `MIN_RESULT_GAP` (60s) gate with slack: `setTimings` runs
// seconds after this client-side `now`, so an exact 60 races the gate (ERR
// 129). The derived stake window is 10% of the period, so the value also has to
// leave room for the staking step.
#[allow(dead_code)]
pub const STAKE_PERIOD: u64 = 180;
pub const STAKE_PERIOD_LONG: u64 = 300; // 5 min — stake window = 30 sec for multi-step tests
pub const CURRENCY_ID_SHELL: u32 = 2;
pub const CURRENCY_ID_NACKL: u32 = 1;
pub const GIVER_ADDRESS: &str =
    "0:1111111111111111111111111111111111111111111111111111111111111111";

pub fn create_context() -> Arc<ClientContext> {
    let mut config = ClientConfig::default();
    config.network.endpoints = Some(vec![network_endpoint()]);
    Arc::new(ClientContext::new(config).expect("create context"))
}

pub fn create_dex() -> Dex {
    Dex::new(dodex_sdk::DexConfig { endpoints: vec![network_endpoint()], ..Default::default() })
        .expect("create Dex")
}

#[cfg(test)]
mod tests {
    use super::endpoint_from;

    #[test]
    fn endpoint_defaults_to_shellnet_with_scheme() {
        assert_eq!(endpoint_from(None), "https://shellnet.ackinacki.org");
    }

    #[test]
    fn endpoint_env_passthrough_verbatim() {
        assert_eq!(endpoint_from(Some("http://127.0.0.1")), "http://127.0.0.1");
    }

    #[test]
    fn endpoint_empty_env_falls_back() {
        assert_eq!(endpoint_from(Some("")), "https://shellnet.ackinacki.org");
    }
}
