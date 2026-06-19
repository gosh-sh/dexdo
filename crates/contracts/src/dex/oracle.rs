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

const ABI: &str = include_str!("../../../../contracts/dex/Oracle.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the DEX `Oracle` contract.
pub struct Oracle {
    base: ContractBase,
}

impl ModuleAccessor for Oracle {
    const MODULE: KitModule = KitModule::External("dex.oracle");
}

impl HasContractBase for Oracle {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for Oracle {}

impl AsyncGuarded<Account> for Oracle {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for Oracle {
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
/// Parameters for `Oracle.deployEventList`.
pub struct ParamsOfDeployEventList {
    pub index: u128,
    /// Human-readable description of the list.
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for owner-only `Oracle.withdrawFees`.
pub struct ParamsOfWithdrawFees {
    pub to: String,
    pub amount: u128,
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for `Oracle.getEventListAddress`.
pub struct ParamsOfGetEventListAddress {
    pub index: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `Oracle.getEventListAddress`.
pub struct ResultOfGetEventListAddress {
    #[serde(rename = "value0")]
    pub address: String,
}

impl Oracle {
    /// Create a wrapper for an already deployed `Oracle` contract.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// # Deploy OracleEventList shard
    ///
    /// Original contract method: `deployEventList`
    ///
    /// Should be signed with oracle owner keys
    pub async fn deploy_event_list(
        &self,
        params: ParamsOfDeployEventList,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deployEventList".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Withdraw collected fees
    ///
    /// Original contract method: `withdrawFees`
    ///
    /// Should be signed with oracle owner keys
    pub async fn withdraw_fees(
        &self,
        params: ParamsOfWithdrawFees,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "withdrawFees".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Get OracleEventList address by index
    ///
    /// Original contract method: `getEventListAddress`
    pub async fn get_event_list_address(
        &self,
        params: ParamsOfGetEventListAddress,
    ) -> KitResult<ResultOfGetEventListAddress> {
        self.call_get_method_with::<ResultOfGetEventListAddress, ParamsOfGetEventListAddress>(
            "getEventListAddress",
            params,
        )
        .await
    }
}
