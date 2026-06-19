use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u32;
use ackinacki_kit::contracts::deserialize::deserialize_u64;
use ackinacki_kit::contracts::deserialize::deserialize_u8;
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

const ABI: &str = include_str!("../../../../contracts/dex/OrderBook.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the DEX `OrderBook` contract.
pub struct OrderBook {
    base: ContractBase,
}

impl ModuleAccessor for OrderBook {
    const MODULE: KitModule = KitModule::External("dex.order_book");
}

impl HasContractBase for OrderBook {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for OrderBook {}

impl AsyncGuarded<Account> for OrderBook {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for OrderBook {
    async fn async_guarded_mut<F, Fut, T, E>(&self, action: F) -> Result<T, E>
    where
        F: FnOnce(OwnedMutexGuard<Account>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let guard = self.account().clone().lock_owned().await;
        action(guard).await
    }
}

// ─── Order tuple used by `executeBatch.orders[]` ───────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// One element of the `orders` array passed to `OrderBook.executeBatch`.
/// Mirrors the on-chain tuple layout exactly.
pub struct OrderBookOrder {
    pub outcome_id: u32,
    pub is_buy: bool,
    pub flags: u8,
    /// `uint256`, decimal or hex string.
    pub price: String,
    pub amount: u128,
    pub min_amount: u128,
    pub epoch_id: u64,
    pub client_order_id: u128,
}

// ─── Method param structs ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OrderBook.setResultStart`.
pub struct ParamsOfSetResultStart {
    pub result_start: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OrderBook.executeBatch`.
pub struct ParamsOfExecuteBatch {
    pub deposit_identifier_hash: String,
    pub orders: Vec<OrderBookOrder>,
    pub cancel_ids: Vec<u128>,
    pub op_nonce: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OrderBook.cancelAllOrders`.
pub struct ParamsOfCancelAllOrders {
    pub deposit_identifier_hash: String,
    pub op_nonce: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OrderBook.getOrder`.
pub struct ParamsOfGetOrder {
    pub order_id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `OrderBook.getOrdersByOwner`.
pub struct ParamsOfGetOrdersByOwner {
    pub deposit_hash: String,
}

// ─── Result structs ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `OrderBook.getDetails`.
pub struct ResultOfGetDetails {
    pub event_id: String,
    pub oracle_list_hash: String,
    #[serde(deserialize_with = "deserialize_u32")]
    pub token_type: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub next_order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_count: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub total_maker_rebates_paid: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub total_protocol_fees: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `OrderBook.getQueueSize`.
pub struct ResultOfGetQueueSize {
    #[serde(deserialize_with = "deserialize_u8")]
    pub size: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `OrderBook.getOrder`.
pub struct ResultOfGetOrder {
    pub deposit_identifier_hash: String,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    pub is_buy: bool,
    #[serde(deserialize_with = "deserialize_u8")]
    pub flags: u8,
    /// `uint256` represented as returned by ABI.
    pub price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub min_amount: u128,
    #[serde(deserialize_with = "deserialize_u64")]
    pub epoch_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `OrderBook.getOrdersByOwner`.
pub struct ResultOfGetOrdersByOwner {
    pub order_ids: Vec<String>,
    pub outcome_ids: Vec<String>,
    pub is_buys: Vec<bool>,
    /// `uint256[]` returned as decimal/hex strings.
    pub prices: Vec<String>,
    pub amounts: Vec<String>,
    pub epoch_ids: Vec<String>,
    pub client_order_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `OrderBook.getShutdownState`.
pub struct ResultOfGetShutdownState {
    pub shutting_down: bool,
    pub shutdown_pending: bool,
}

// ─── Method bindings ──────────────────────────────────────────────────────

impl OrderBook {
    /// Create a wrapper for a deployed `OrderBook`.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// Original contract method: `setResultStart`.
    pub async fn set_result_start(
        &self,
        params: ParamsOfSetResultStart,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "setResultStart".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `executeBatch`. Submits a batch of new
    /// orders + a list of order IDs to cancel, all bound to a single
    /// `depositIdentifierHash` (the calling PrivateNote).
    pub async fn execute_batch(
        &self,
        params: ParamsOfExecuteBatch,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "executeBatch".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `cancelAllOrders`.
    pub async fn cancel_all_orders(
        &self,
        params: ParamsOfCancelAllOrders,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelAllOrders".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `processHead`. Drains the matching queue
    /// without submitting new orders.
    pub async fn process_head(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "processHead".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `shutdown`. Owner-only.
    pub async fn shutdown(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet { function_name: "shutdown".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `getDetails`.
    pub async fn get_details(&self) -> KitResult<ResultOfGetDetails> {
        self.call_get_method::<ResultOfGetDetails>("getDetails").await
    }

    /// Original contract method: `getQueueSize`.
    pub async fn get_queue_size(&self) -> KitResult<ResultOfGetQueueSize> {
        self.call_get_method::<ResultOfGetQueueSize>("getQueueSize").await
    }

    /// Original contract method: `getOrder`.
    pub async fn get_order(&self, params: ParamsOfGetOrder) -> KitResult<ResultOfGetOrder> {
        self.call_get_method_with::<ResultOfGetOrder, ParamsOfGetOrder>("getOrder", params).await
    }

    /// Original contract method: `getOrdersByOwner`.
    pub async fn get_orders_by_owner(
        &self,
        params: ParamsOfGetOrdersByOwner,
    ) -> KitResult<ResultOfGetOrdersByOwner> {
        self.call_get_method_with::<ResultOfGetOrdersByOwner, ParamsOfGetOrdersByOwner>(
            "getOrdersByOwner",
            params,
        )
        .await
    }

    /// Original contract method: `getShutdownState`.
    pub async fn get_shutdown_state(&self) -> KitResult<ResultOfGetShutdownState> {
        self.call_get_method::<ResultOfGetShutdownState>("getShutdownState").await
    }
}
