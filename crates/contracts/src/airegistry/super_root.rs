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

const ABI: &str = include_str!("../../../../contracts/airegistry/SuperRoot.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the AI Registry `SuperRoot` contract — the per-network root
/// factory that registers `RootModel` children at deterministic addresses
/// derived from an owner pubkey.
pub struct SuperRoot {
    base: ContractBase,
}

impl ModuleAccessor for SuperRoot {
    const MODULE: KitModule = KitModule::External("airegistry.super_root");
}

impl HasContractBase for SuperRoot {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for SuperRoot {}

impl AsyncGuarded<Account> for SuperRoot {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for SuperRoot {
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
/// Parameters for `SuperRoot.setPubkey`.
pub struct ParamsOfSetPubkey {
    /// `uint256`, decimal or hex string.
    pub pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `SuperRoot.deployRootModel`.
pub struct ParamsOfDeployRootModel {
    /// `uint256`, decimal or hex string.
    pub owner_pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `SuperRoot.getRootModelAddress`.
pub struct ParamsOfGetRootModelAddress {
    /// `uint256`, decimal or hex string.
    pub owner_pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of the address-derivation getter (`getRootModelAddress`).
pub struct ResultOfGetAddress {
    #[serde(rename = "value0")]
    pub address: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `SuperRoot.getOwnerPubkey`.
pub struct ResultOfGetOwnerPubkey {
    #[serde(rename = "value0")]
    pub owner_pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `SuperRoot.getVersion` — `(version, contractName)`.
pub struct ResultOfGetVersion {
    #[serde(rename = "value0")]
    pub version: String,
    #[serde(rename = "value1")]
    pub name: String,
}

impl SuperRoot {
    /// Create a wrapper for a deployed `SuperRoot`.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// # Rotate the owner pubkey
    ///
    /// Original contract method: `setPubkey`
    ///
    /// Should be signed with the current owner key.
    pub async fn set_pubkey(
        &self,
        params: ParamsOfSetPubkey,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "setPubkey".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Deploy a RootModel for an owner pubkey
    ///
    /// Original contract method: `deployRootModel`
    ///
    /// THE SUPER ROOT PERFORMS THE DEPLOY; this is not a registration call. It
    /// was one — a RootModel used to be deployed by its owner as an external
    /// message and then announce itself here, and this wrapper was named for
    /// that announcement. The direction is now the other way round: an internal
    /// `new` from the super root, so the child lands in the super root's dapp,
    /// where its `ensureBalance()` can actually draw on a configured dapp.
    ///
    /// A consequence worth knowing before reaching for an external deploy
    /// instead: `RootModel`'s constructor requires `msg.sender` to be the super
    /// root, so there is no longer any other way to create one. Deploying onto
    /// an already-occupied address leaves the existing code untouched and only
    /// donates the value, so a squatted address cannot be reclaimed.
    pub async fn deploy_root_model(
        &self,
        params: ParamsOfDeployRootModel,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deployRootModel".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Get deterministic RootModel address for an owner pubkey
    ///
    /// Original contract method: `getRootModelAddress`
    pub async fn get_root_model_address(
        &self,
        params: ParamsOfGetRootModelAddress,
    ) -> KitResult<ResultOfGetAddress> {
        self.call_get_method_with::<ResultOfGetAddress, ParamsOfGetRootModelAddress>(
            "getRootModelAddress",
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
