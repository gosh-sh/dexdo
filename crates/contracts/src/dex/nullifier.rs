use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::contracts::traits::AutoContract;
use ackinacki_kit::contracts::traits::ContractBase;
use ackinacki_kit::contracts::traits::HasContractBase;
use ackinacki_kit::contracts::traits::ModuleAccessor;
use ackinacki_kit::shared::traits::guarded::AsyncGuarded;
use ackinacki_kit::shared::traits::guarded::AsyncGuardedMut;
use ackinacki_kit::tvm_client::abi::Abi;
use ackinacki_kit::tvm_client::ClientContext;
use tokio::sync::OwnedMutexGuard;

const ABI: &str = include_str!("../../../../contracts/dex/Nullifier.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the DEX `Nullifier` contract.
///
/// In practice this contract is deployed and controlled by `RootPN`; the only
/// public method exposed by ABI is `getVersion` (available via `VersionAccessor`).
pub struct Nullifier {
    base: ContractBase,
}

impl ModuleAccessor for Nullifier {
    const MODULE: KitModule = KitModule::External("dex.nullifier");
}

impl HasContractBase for Nullifier {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for Nullifier {}

impl AsyncGuarded<Account> for Nullifier {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for Nullifier {
    async fn async_guarded_mut<F, Fut, T, E>(&self, action: F) -> Result<T, E>
    where
        F: FnOnce(OwnedMutexGuard<Account>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let guard = self.account().clone().lock_owned().await;
        action(guard).await
    }
}

impl Nullifier {
    /// Create a wrapper for a deployed `Nullifier`.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }
}
