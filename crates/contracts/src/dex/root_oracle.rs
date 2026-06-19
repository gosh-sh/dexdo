use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::contracts::traits::AutoContract;
use ackinacki_kit::contracts::traits::ContractBase;
use ackinacki_kit::contracts::traits::GetMethodAccessor;
use ackinacki_kit::contracts::traits::HasContractBase;
use ackinacki_kit::contracts::traits::ModuleAccessor;
use ackinacki_kit::contracts::traits::SendMessage;
use ackinacki_kit::contracts::KitResult;
use ackinacki_kit::shared::traits::guarded::AsyncGuarded;
use ackinacki_kit::shared::traits::guarded::AsyncGuardedMut;
use ackinacki_kit::tvm_client::abi::Abi;
use ackinacki_kit::tvm_client::abi::CallSet;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use ackinacki_kit::tvm_client::ClientContext;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use tokio::sync::OwnedMutexGuard;

const ABI: &str = include_str!("../../../../contracts/dex/RootOracle.abi.json");

/// Reference migrated wrapper using `ContractBase + HasContractBase + AutoContract`.
#[derive(Debug, Clone)]
pub struct RootOracle {
    base: ContractBase,
}

impl ModuleAccessor for RootOracle {
    const MODULE: KitModule = KitModule::External("dex.root_oracle");
}

impl HasContractBase for RootOracle {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for RootOracle {}

impl AsyncGuarded<Account> for RootOracle {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for RootOracle {
    async fn async_guarded_mut<F, Fut, T, E>(&self, action: F) -> Result<T, E>
    where
        F: FnOnce(OwnedMutexGuard<Account>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let guard = self.account().clone().lock_owned().await;
        action(guard).await
    }
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for `RootOracle.deployOracle`.
pub struct ParamsOfDeployOracle {
    #[serde(rename(serialize = "oraclePubkey"))]
    pub oracle_pubkey: String,
    #[serde(rename(serialize = "oracleName"))]
    pub oracle_name: String,
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for `RootOracle.getOracleAddress`.
pub struct ParamsOfGetOracleAddress {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootOracle.getOracleAddress`.
pub struct ResultOfGetOracleAddress {
    #[serde(rename = "oracleAddress")]
    pub oracle_address: String,
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for owner-only `RootOracle.updateCode`.
pub struct ParamsOfUpdateCode {
    #[serde(rename(serialize = "newcode"))]
    pub new_code: String,
    pub cell: String,
}

impl RootOracle {
    /// Premine RootOracle address from `dex/modifiers/modifiers.sol`.
    pub const DEFAULT_ADDRESS: &'static str =
        "0:1515151515151515151515151515151515151515151515151515151515151515";

    /// Allows passing the root address explicitly (useful for shellnet/testnet
    /// or local networks where RootOracle may live at a non-premine address).
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// # Deploy oracle
    ///
    /// Original contract method: `deployOracle`
    ///
    /// Open method, can be called by any external sender
    pub async fn deploy_oracle(
        &self,
        params: ParamsOfDeployOracle,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deployOracle".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Get deterministic oracle address
    ///
    /// Original contract method: `getOracleAddress`
    pub async fn get_oracle_address(
        &self,
        params: ParamsOfGetOracleAddress,
    ) -> KitResult<ResultOfGetOracleAddress> {
        self.call_get_method_with::<ResultOfGetOracleAddress, ParamsOfGetOracleAddress>(
            "getOracleAddress",
            params,
        )
        .await
    }

    /// # Update root code
    ///
    /// Original contract method: `updateCode`
    ///
    /// Should be signed with root owner keys
    pub async fn update_code(
        &self,
        params: ParamsOfUpdateCode,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "updateCode".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }
}
