use std::collections::HashMap;
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

const ABI: &str = include_str!("../../../../contracts/dex/OracleEventList.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for an `OracleEventList` shard contract.
pub struct OracleEventList {
    base: ContractBase,
}

impl ModuleAccessor for OracleEventList {
    const MODULE: KitModule = KitModule::External("dex.oracle_event_list");
}

impl HasContractBase for OracleEventList {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for OracleEventList {}

impl AsyncGuarded<Account> for OracleEventList {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for OracleEventList {
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
/// Parameters for `OracleEventList.addEvent`.
pub struct ParamsOfAddEvent {
    pub event_name: String,
    pub oracle_fee: u128,
    pub deadline: u64,
    pub describe: String,
    pub outcome_names: HashMap<u32, String>,
    pub trust_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OracleEventList.deleteEvent`.
pub struct ParamsOfDeleteEvent {
    pub event_id: String,
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for `OracleEventList.setDescription`.
pub struct ParamsOfSetDescription {
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OracleEventList.confirmEvent` and `OracleEventList.cancelEvent`.
pub struct ParamsOfConfirmOrCancelEvent {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OracleEventList.addRangeEvent`.
///
/// Adds a numeric *range* event: `bounds` are strictly increasing upper bounds
/// (`uint256[]`, decimal/hex strings) so `n` bounds yield `n + 1` outcomes; the
/// matching dense `0..n` string labels are passed in `outcome_names`. The event
/// resolves on-chain from `ob` (an `InferenceOrderBook`) weekly median.
pub struct ParamsOfAddRangeEvent {
    pub event_name: String,
    pub oracle_fee: u128,
    pub deadline: u64,
    pub describe: String,
    /// `uint256[]` strictly increasing upper bounds, decimal/hex strings.
    pub bounds: Vec<String>,
    pub outcome_names: HashMap<u32, String>,
    /// Address of the `InferenceOrderBook` providing the reference price.
    pub ob: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OracleEventList.resolveRange` (callable by anyone after the
/// event deadline).
pub struct ParamsOfResolveRange {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OracleEventList.onWeeklyMedian` (callback from the bound
/// `InferenceOrderBook`).
pub struct ParamsOfOnWeeklyMedian {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    /// `uint256` reference price, decimal/hex string.
    pub price: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OracleEventList.getRangeData`.
pub struct ParamsOfGetRangeData {
    pub event_id: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `OracleEventList.getRangeData`.
pub struct ResultOfGetRangeData {
    /// `uint256[]` strictly increasing upper bounds, returned as decimal/hex
    /// strings.
    pub bounds: Vec<String>,
    /// Address of the bound `InferenceOrderBook`.
    pub ob: String,
    pub exists: bool,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `OracleEventList._events` getter.
///
/// Entries are left as raw JSON because the event tuple schema can evolve.
pub struct ResultOfGetEvents {
    #[serde(rename = "_events")]
    pub events: HashMap<String, serde_json::Value>,
}

impl OracleEventList {
    /// Create a wrapper for a deployed `OracleEventList` shard.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// # Update human-readable list description
    ///
    /// Original contract method: `setDescription`
    ///
    /// Should be signed with oracle owner keys
    pub async fn set_description(
        &self,
        params: ParamsOfSetDescription,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "setDescription".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Add oracle-serviced event
    ///
    /// Original contract method: `addEvent`
    ///
    /// Should be signed with oracle owner keys
    pub async fn add_event(
        &self,
        params: ParamsOfAddEvent,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "addEvent".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Delete event from list
    ///
    /// Original contract method: `deleteEvent`
    ///
    /// Should be signed with oracle owner keys
    pub async fn delete_event(
        &self,
        params: ParamsOfDeleteEvent,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deleteEvent".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Confirm event and deploy PMP
    ///
    /// Original contract method: `confirmEvent`
    ///
    /// Should be signed with oracle owner keys
    pub async fn confirm_event(
        &self,
        params: ParamsOfConfirmOrCancelEvent,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "confirmEvent".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Cancel event in PMP
    ///
    /// Original contract method: `cancelEvent`
    ///
    /// Should be signed with oracle owner keys
    pub async fn cancel_event(
        &self,
        params: ParamsOfConfirmOrCancelEvent,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelEvent".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Add numeric range event
    ///
    /// Original contract method: `addRangeEvent`
    ///
    /// Should be signed with oracle owner keys. The event resolves on-chain from
    /// the bound `InferenceOrderBook` weekly median (`resolveRange` →
    /// `onWeeklyMedian`).
    pub async fn add_range_event(
        &self,
        params: ParamsOfAddRangeEvent,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "addRangeEvent".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Resolve a range event from the bound order book median
    ///
    /// Original contract method: `resolveRange`
    ///
    /// Callable by anyone after the event deadline; pulls the order book weekly
    /// median asynchronously (resolved in `onWeeklyMedian`).
    pub async fn resolve_range(
        &self,
        params: ParamsOfResolveRange,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "resolveRange".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Order book median callback
    ///
    /// Original contract method: `onWeeklyMedian` (callback from the bound
    /// `InferenceOrderBook`).
    ///
    /// Normally only invoked by the bound order book; exposed for admin tools
    /// and tests.
    pub async fn on_weekly_median(
        &self,
        params: ParamsOfOnWeeklyMedian,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onWeeklyMedian".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Read range-event data (bounds + bound order book)
    ///
    /// Original contract method: `getRangeData`
    pub async fn get_range_data(
        &self,
        params: ParamsOfGetRangeData,
    ) -> KitResult<ResultOfGetRangeData> {
        self.call_get_method_with::<ResultOfGetRangeData, ParamsOfGetRangeData>(
            "getRangeData",
            params,
        )
        .await
    }

    /// # Read full `_events` mapping
    ///
    /// Original contract method: `_events`
    ///
    /// Returns raw JSON values for map entries to keep the wrapper stable while
    /// DEX event tuple schema is still evolving.
    pub async fn get_events(&self) -> KitResult<ResultOfGetEvents> {
        self.call_get_method::<ResultOfGetEvents>("_events").await
    }
}
