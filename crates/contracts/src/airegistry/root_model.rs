use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::contracts::traits::AutoContract;
use ackinacki_kit::contracts::traits::ContractBase;
use ackinacki_kit::contracts::traits::GetMethodAccessor;
use ackinacki_kit::contracts::traits::HasContractBase;
use ackinacki_kit::contracts::traits::ModuleAccessor;
use ackinacki_kit::contracts::KitResult;
use ackinacki_kit::shared::traits::guarded::AsyncGuarded;
use ackinacki_kit::shared::traits::guarded::AsyncGuardedMut;
use ackinacki_kit::tvm_client::abi::Abi;
use ackinacki_kit::tvm_client::ClientContext;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::OwnedMutexGuard;

const ABI: &str = include_str!("../../../../contracts/airegistry/RootModel.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the AI Registry `RootModel` contract — a per-owner model
/// registry that derives the deterministic `(sellerPubkey, nonce)` address every
/// `TokenContract` of that owner lives at.
///
/// It no longer creates them. `registerTokenContract` has no wrapper because
/// nothing off chain can call it: since 4.0.36 it is a `pure` self-announcement
/// that recomputes the canonical address and requires `msg.sender` to equal it,
/// which only the already-deployed deal satisfies. Deals are deployed by the
/// seller's note — see `dex::private_note::PrivateNote::deploy_deal`.
pub struct RootModel {
    base: ContractBase,
}

impl ModuleAccessor for RootModel {
    const MODULE: KitModule = KitModule::External("airegistry.root_model");
}

impl HasContractBase for RootModel {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for RootModel {}

impl AsyncGuarded<Account> for RootModel {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for RootModel {
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
#[serde(rename_all = "camelCase")]
/// Parameters for `RootModel.getTokenContractAddress`.
pub struct ParamsOfGetTokenContractAddress {
    /// `uint256`, decimal or hex string.
    pub seller_pubkey: String,
    pub nonce: u64,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootModel.getTokenContractAddress`.
pub struct ResultOfGetTokenContractAddress {
    #[serde(rename = "value0")]
    pub address: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootModel.getOwnerPubkey`.
pub struct ResultOfGetOwnerPubkey {
    #[serde(rename = "value0")]
    pub owner_pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootModel.getVersion` — `(version, contractName)`.
pub struct ResultOfGetVersion {
    #[serde(rename = "value0")]
    pub version: String,
    #[serde(rename = "value1")]
    pub name: String,
}

impl RootModel {
    /// Create a wrapper for a deployed `RootModel`.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// # Get deterministic TokenContract address for `(sellerPubkey, nonce)`
    ///
    /// Original contract method: `getTokenContractAddress`
    pub async fn get_token_contract_address(
        &self,
        params: ParamsOfGetTokenContractAddress,
    ) -> KitResult<ResultOfGetTokenContractAddress> {
        self.call_get_method_with::<ResultOfGetTokenContractAddress, ParamsOfGetTokenContractAddress>(
            "getTokenContractAddress",
            params,
        )
        .await
    }

    /// # Get the owner pubkey
    ///
    /// Original contract method: `getOwnerPubkey`
    pub async fn get_owner_pubkey(&self) -> KitResult<ResultOfGetOwnerPubkey> {
        self.call_get_method::<ResultOfGetOwnerPubkey>("getOwnerPubkey").await
    }

    /// # Get version + contract name
    ///
    /// Original contract method: `getVersion`
    pub async fn get_version(&self) -> KitResult<ResultOfGetVersion> {
        self.call_get_method::<ResultOfGetVersion>("getVersion").await
    }
}
