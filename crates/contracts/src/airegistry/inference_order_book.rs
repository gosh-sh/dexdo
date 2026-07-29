use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u16;
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

const ABI: &str = include_str!("../../../../contracts/airegistry/InferenceOrderBook.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the AI Registry `InferenceOrderBook` contract — the per-model
/// CLOB that matches SELL offers (backed by a `TokenContract`) against BUY
/// orders / subscriptions paid in SHELL escrow (spec §2 + §8).
pub struct InferenceOrderBook {
    base: ContractBase,
}

impl ModuleAccessor for InferenceOrderBook {
    const MODULE: KitModule = KitModule::External("airegistry.inference_order_book");
}

impl HasContractBase for InferenceOrderBook {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for InferenceOrderBook {}

impl AsyncGuarded<Account> for InferenceOrderBook {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for InferenceOrderBook {
    async fn async_guarded_mut<F, Fut, T, E>(&self, action: F) -> Result<T, E>
    where
        F: FnOnce(OwnedMutexGuard<Account>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let guard = self.account().clone().lock_owned().await;
        action(guard).await
    }
}

// ─── Method param structs ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `InferenceOrderBook.placeSellOffer`.
pub struct ParamsOfPlaceSellOffer {
    pub price_per_tick: u128,
    pub max_ticks: u128,
    pub flags: u8,
    /// `uint256`, decimal or hex string — the seller note's owner pubkey. The
    /// book recomputes the canonical TokenContract address from this pubkey and
    /// `nonce`, then requires the caller to be it, so the deal address is never
    /// taken from the message.
    pub seller_pubkey: String,
    /// Deal nonce — must match the nonce the calling TokenContract was derived
    /// from.
    pub nonce: u64,
    /// The seller's note, recorded as the offer's owner so a fill can settle
    /// back to it.
    pub owner_note: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `InferenceOrderBook.placeBuyOrder`.
pub struct ParamsOfPlaceBuyOrder {
    pub max_price_per_tick: u128,
    pub ticks: u128,
    pub flags: u8,
    /// Time-in-force deadline (`0` = good-till-cancel).
    pub deadline: u64,
    /// `uint256`, decimal or hex string.
    pub buyer_pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `InferenceOrderBook.placeSubscription`.
pub struct ParamsOfPlaceSubscription {
    pub max_price_per_tick: u128,
    pub ticks: u128,
    /// Same flag mask a limit buy takes (`IOC`/`FOK`/`MARKET`/`POST_ONLY`); a
    /// subscription rests as a standing bid, so 0 is the ordinary value.
    pub flags: u8,
    pub auto_renew: bool,
    /// `uint256`, decimal or hex string.
    pub buyer_pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for the order-id-keyed methods `cancelOrder`, `pokeSubscription`
/// and the `getOrder` / `getSubscription` getters.
pub struct ParamsOfOrderId {
    pub order_id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `InferenceOrderBook.getOrder` (the getter keys on `id`, not
/// `orderId`).
pub struct ParamsOfGetOrder {
    pub id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `InferenceOrderBook.requestWeeklyMedian`.
pub struct ParamsOfRequestWeeklyMedian {
    /// `uint256`, decimal or hex string.
    pub event_id: String,
    /// `uint256`, decimal or hex string.
    pub oracle_list_hash: String,
    pub token_type: u32,
}

// ─── Result structs ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
/// Result of `InferenceOrderBook.getWeeklyMedianPrice`.
pub struct ResultOfGetWeeklyMedianPrice {
    /// `uint256` represented as returned by ABI.
    #[serde(rename = "value0")]
    pub price: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `InferenceOrderBook.getOrder`.
pub struct ResultOfGetOrder {
    pub note: String,
    pub token_contract: String,
    /// `uint256` represented as returned by ABI.
    pub price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub escrow: u128,
    #[serde(deserialize_with = "deserialize_u64")]
    pub deadline: u64,
    #[serde(deserialize_with = "deserialize_u8")]
    pub flags: u8,
    pub is_buy: bool,
    #[serde(deserialize_with = "deserialize_u64")]
    pub ts: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `InferenceOrderBook.getBestBidAsk`.
pub struct ResultOfGetBestBidAsk {
    pub has_bid: bool,
    /// `uint256` represented as returned by ABI.
    pub bid: String,
    pub has_ask: bool,
    /// `uint256` represented as returned by ABI.
    pub ask: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `InferenceOrderBook.getStats`.
pub struct ResultOfGetStats {
    #[serde(deserialize_with = "deserialize_u128")]
    pub next_order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_count: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub executed_notional: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub executed_ticks: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `InferenceOrderBook.getQueueSize`.
pub struct ResultOfGetQueueSize {
    #[serde(rename = "value0", deserialize_with = "deserialize_u8")]
    pub size: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `InferenceOrderBook.getSubscription`.
pub struct ResultOfGetSubscription {
    pub exists: bool,
    #[serde(deserialize_with = "deserialize_u64")]
    pub period_start: u64,
    #[serde(deserialize_with = "deserialize_u8")]
    pub cur_cycle: u8,
    #[serde(deserialize_with = "deserialize_u128")]
    pub cycle_budget: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub cycle_spent: u128,
    pub auto_renew: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `InferenceOrderBook.getParams`.
pub struct ResultOfGetParams {
    /// `uint256` represented as returned by ABI.
    pub model_hash: String,
    #[serde(deserialize_with = "deserialize_u16")]
    pub platform_fee_bps: u16,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `InferenceOrderBook.getVersion` — `(version, contractName)`.
pub struct ResultOfGetVersion {
    #[serde(rename = "value0")]
    pub version: String,
    #[serde(rename = "value1")]
    pub name: String,
}

impl InferenceOrderBook {
    /// Create a wrapper for a deployed `InferenceOrderBook`.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    // ─── Matching ─────────────────────────────────────────────────────

    /// Original contract method: `processHead`. Drains the matching queue
    /// across continuation transactions (`> MAX_MATCHES_PER_CALL`).
    pub async fn process_head(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "processHead".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `placeSellOffer`. On-chain sender is normally
    /// the seller note; the offer is backed by a deployed `TokenContract`.
    pub async fn place_sell_offer(
        &self,
        params: ParamsOfPlaceSellOffer,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "placeSellOffer".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `placeBuyOrder`. On-chain sender is normally
    /// the buyer note, which forwards the SHELL escrow.
    pub async fn place_buy_order(
        &self,
        params: ParamsOfPlaceBuyOrder,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "placeBuyOrder".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `placeSubscription` (weekly semantic order,
    /// spec §8).
    pub async fn place_subscription(
        &self,
        params: ParamsOfPlaceSubscription,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "placeSubscription".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `pokeSubscription`. Rolls a subscription onto
    /// its next cycle / forfeits the unspent budget of the closing cycle.
    pub async fn poke_subscription(
        &self,
        params: ParamsOfOrderId,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "pokeSubscription".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `cancelOrder`. Cancels one resting order and
    /// refunds its remaining escrow.
    pub async fn cancel_order(
        &self,
        params: ParamsOfOrderId,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelOrder".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `cancelAllOrders`. Cancels every resting order
    /// owned by the caller.
    pub async fn cancel_all_orders(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "cancelAllOrders".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `requestWeeklyMedian`. Asks the matching
    /// engine to refresh the reference price for the model.
    pub async fn request_weekly_median(
        &self,
        params: ParamsOfRequestWeeklyMedian,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "requestWeeklyMedian".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    // ─── Getters ──────────────────────────────────────────────────────

    /// Original contract method: `getWeeklyMedianPrice`.
    pub async fn get_weekly_median_price(&self) -> KitResult<ResultOfGetWeeklyMedianPrice> {
        self.call_get_method::<ResultOfGetWeeklyMedianPrice>("getWeeklyMedianPrice").await
    }

    /// Original contract method: `getOrder`.
    pub async fn get_order(&self, params: ParamsOfGetOrder) -> KitResult<ResultOfGetOrder> {
        self.call_get_method_with::<ResultOfGetOrder, ParamsOfGetOrder>("getOrder", params).await
    }

    /// Original contract method: `getBestBidAsk`.
    pub async fn get_best_bid_ask(&self) -> KitResult<ResultOfGetBestBidAsk> {
        self.call_get_method::<ResultOfGetBestBidAsk>("getBestBidAsk").await
    }

    /// Original contract method: `getStats`.
    pub async fn get_stats(&self) -> KitResult<ResultOfGetStats> {
        self.call_get_method::<ResultOfGetStats>("getStats").await
    }

    /// Original contract method: `getQueueSize`.
    pub async fn get_queue_size(&self) -> KitResult<ResultOfGetQueueSize> {
        self.call_get_method::<ResultOfGetQueueSize>("getQueueSize").await
    }

    /// Original contract method: `getSubscription`.
    pub async fn get_subscription(
        &self,
        params: ParamsOfOrderId,
    ) -> KitResult<ResultOfGetSubscription> {
        self.call_get_method_with::<ResultOfGetSubscription, ParamsOfOrderId>(
            "getSubscription",
            params,
        )
        .await
    }

    /// Original contract method: `getParams`.
    pub async fn get_params(&self) -> KitResult<ResultOfGetParams> {
        self.call_get_method::<ResultOfGetParams>("getParams").await
    }

    /// Original contract method: `getVersion`.
    pub async fn get_version(&self) -> KitResult<ResultOfGetVersion> {
        self.call_get_method::<ResultOfGetVersion>("getVersion").await
    }
}
