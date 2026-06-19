use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u128_map;
use ackinacki_kit::contracts::deserialize::deserialize_u32;
use ackinacki_kit::contracts::deserialize::deserialize_u64;
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

use crate::dex::order_book::OrderBookOrder;

const ABI: &str = include_str!("../../../../contracts/dex/PrivateNote.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the DEX `PrivateNote` contract.
pub struct PrivateNote {
    base: ContractBase,
}

impl ModuleAccessor for PrivateNote {
    const MODULE: KitModule = KitModule::External("dex.private_note");
}

impl HasContractBase for PrivateNote {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for PrivateNote {}

impl AsyncGuarded<Account> for PrivateNote {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for PrivateNote {
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
/// Parameters for `PrivateNote.changeOwner`.
pub struct ParamsOfChangeOwner {
    pub new_pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.deployPMP`.
pub struct ParamsOfDeployPmp {
    pub event_id: String,
    pub oracle_fee: Vec<u128>,
    pub token_type: u32,
    pub names: Vec<String>,
    pub index: Vec<u128>,
    pub initial_stakes: Vec<u128>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Shared PMP key (`event_id`, `oracle_list_hash`, `token_type`) used by
/// multiple `PrivateNote` methods (`deleteStake`, `cancelStake`, `claim`).
pub struct ParamsOfStakeKey {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.mergeFullSet`.
pub struct ParamsOfMergeFullSet {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub amount: Vec<u128>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onInitialStakesAccepted`.
pub struct ParamsOfOnInitialStakesAccepted {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub amounts: Vec<u128>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onInitialStakesFailed`.
pub struct ParamsOfOnInitialStakesFailed {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub refund_total: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onPmpCleanRefund`.
pub struct ParamsOfOnPmpCleanRefund {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub refund_amounts: Vec<u128>,
    pub refund_total: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onStakeCancelled`.
pub struct ParamsOfOnStakeCancelled {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub value: u128,
    pub coupon_value: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.splitFullSet`.
pub struct ParamsOfSplitFullSet {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub collateral: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onSplitAccepted`.
pub struct ParamsOfOnSplitAccepted {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub amounts: Vec<u128>,
    pub collateral_used: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onMergeAccepted`.
pub struct ParamsOfOnMergeAccepted {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub collateral: u128,
    pub amounts: Vec<u128>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onStakeAccepted`.
pub struct ParamsOfOnStakeAccepted {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub outcome_count: u128,
    pub bet_type: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onClaimAccepted`.
pub struct ParamsOfOnClaimAccepted {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub outcome: Option<u32>,
    pub payout_clean: u128,
    pub payout_debt: u128,
    pub payout_coupon: u128,
    pub debt_paid: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.acceptFee`.
pub struct ParamsOfAcceptFee {
    pub fee: u128,
    pub token_type: u32,
    pub event_id: String,
    pub oracle_list_hash: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.setStake`.
pub struct ParamsOfSetStake {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub outcome: u32,
    pub amount: u128,
    pub use_coupon: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.generateCoupon`.
pub struct ParamsOfGenerateCoupon {
    pub token_type: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.initTransfer`.
pub struct ParamsOfInitTransfer {
    pub dest_deposit_hash: String,
    pub token_type: u32,
    pub amount: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.offerTransfer`.
pub struct ParamsOfOfferTransfer {
    pub token_type: u32,
    pub amount: u128,
    pub sender_deposit_hash: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.withdrawTokens`.
pub struct ParamsOfWithdrawTokens {
    pub dest_wallet_addr: String,
    /// `uint256` dApp id, decimal or hex string. Drives no PrivateNote logic;
    /// forwarded to `RootPN.withdrawTokens` and surfaced in `TokensWithdrawn`.
    #[serde(rename = "dapp_id")]
    pub dapp_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.revertWithdraw`.
///
/// `amounts` maps `tokenType → value` for the balances being reverted back
/// into the note (callback from `RootPN` when a withdraw fails).
pub struct ParamsOfRevertWithdraw {
    pub amounts: HashMap<u32, u128>,
}

// ─── OrderBook proxy methods (PrivateNote → OrderBook) ────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.placeOrder`.
pub struct ParamsOfPlaceOrder {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub outcome_id: u32,
    pub is_buy: bool,
    /// `uint256` decimal or hex string.
    pub price: String,
    pub amount: u128,
    pub flags: u8,
    pub min_amount: u128,
    pub epoch_id: u64,
    pub client_order_id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.placeBatch`.
///
/// Atomic batch: cancels `cancel_ids` and places `orders` in a single
/// `OrderBook.executeBatch` dispatch. Either side may be empty (but not
/// both).
pub struct ParamsOfPlaceBatch {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub orders: Vec<OrderBookOrder>,
    pub cancel_ids: Vec<u128>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.cancelOrder`.
pub struct ParamsOfCancelOrder {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub order_id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.cancelOrderByClient`.
pub struct ParamsOfCancelOrderByClient {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub client_order_id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.cancelAllOrders`.
pub struct ParamsOfCancelAllOrders {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
}

// ─── OrderBook → PrivateNote callbacks ────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onOrderPlaced` (callback from OrderBook).
pub struct ParamsOfOnOrderPlaced {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub order_id: u128,
    pub fee_reserve: u128,
    pub lock: u128,
    pub client_order_id: u128,
    pub outcome_id: u32,
    pub is_buy: bool,
    pub flags: u8,
    /// `uint256` decimal or hex string.
    pub price: String,
    pub amount: u128,
    pub op_nonce: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onOrderRejected` (callback from OrderBook).
pub struct ParamsOfOnOrderRejected {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub outcome_id: u32,
    pub is_buy: bool,
    pub flags: u8,
    /// `uint256` decimal or hex string.
    pub price: String,
    pub amount: u128,
    pub num_outcomes: u32,
    pub client_order_id: u128,
    pub op_nonce: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onOrderCancelled` (callback from OrderBook).
pub struct ParamsOfOnOrderCancelled {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub order_id: u128,
    pub outcome_id: u32,
    pub is_buy: bool,
    pub amount: u128,
    pub client_order_id: u128,
    pub op_nonce: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onOrderFilled` (callback from OrderBook).
pub struct ParamsOfOnOrderFilled {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub outcome_id: u32,
    pub filled_amount: u128,
    /// `uint256` decimal or hex string.
    pub clearing_price: String,
    pub is_buy: bool,
    pub refund_amount: u128,
    pub fee_amount: u128,
    pub is_rebate: bool,
    pub order_id: u128,
    pub is_final: bool,
    pub num_outcomes: u32,
    pub client_order_id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.onBatchComplete` (callback from OrderBook).
pub struct ParamsOfOnBatchComplete {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub op_nonce: u64,
}

// ─── Inference market (spec §2-§8): note as participant ───────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for the stream-lock callbacks `streamLock`, `streamUnlock`,
/// `streamDisputeLock`, `streamDisputeUnlock`.
///
/// `deal` is the streaming-deal (TokenContract) address; the contract requires
/// `msg.sender == deal`, so these are normally only invoked by the deal itself.
pub struct ParamsOfStreamLock {
    pub deal: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.deployInferenceOrderBook` and
/// `PrivateNote.getInferenceOrderBookAddress`.
pub struct ParamsOfInferenceOrderBook {
    /// Canonical `InferenceOrderBook` code as a base64 BOC string (`cell`).
    pub inference_order_book_code: String,
    /// `uint256` model hash, decimal/hex string.
    pub model_hash: String,
    pub tick_size: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.postSellOffer`.
pub struct ParamsOfPostSellOffer {
    pub order_book: String,
    pub price_per_tick: u128,
    pub max_ticks: u128,
    pub token_contract: String,
    pub flags: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.placeInferenceBuy`.
pub struct ParamsOfPlaceInferenceBuy {
    pub order_book: String,
    pub max_price_per_tick: u128,
    pub ticks: u128,
    pub escrow: u128,
    pub flags: u8,
    /// Time-in-force deadline (`0` = good-till-cancel).
    pub deadline: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.placeInferenceSubscription`.
pub struct ParamsOfPlaceInferenceSubscription {
    pub order_book: String,
    pub max_price_per_tick: u128,
    pub ticks: u128,
    pub escrow: u128,
    pub auto_renew: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.cancelInferenceOrder`.
pub struct ParamsOfCancelInferenceOrder {
    pub order_book: String,
    pub order_id: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `PrivateNote.cancelAllInferenceOrders`.
pub struct ParamsOfCancelAllInferenceOrders {
    pub order_book: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for the streaming-deal driver methods `streamStop` and
/// `streamDispute` (buyer note → deal `TokenContract`).
pub struct ParamsOfStreamDeal {
    pub token_contract: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote.getStreamLocks`.
pub struct ResultOfGetStreamLocks {
    #[serde(rename = "streamCount", deserialize_with = "deserialize_u32")]
    pub stream_count: u32,
    #[serde(rename = "disputeCount", deserialize_with = "deserialize_u32")]
    pub dispute_count: u32,
    #[serde(rename = "lastChange", deserialize_with = "deserialize_u64")]
    pub last_change: u64,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote.getInferenceOrderBookAddress`.
pub struct ResultOfGetInferenceOrderBookAddress {
    #[serde(rename = "value0")]
    pub address: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote.getPMPCode`.
pub struct ResultOfGetPmpCode {
    #[serde(rename = "pmpCode")]
    pub pmp_code: String,
    #[serde(rename = "pmpCodeHash")]
    pub pmp_code_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote.getDetails`.
pub struct ResultOfGetDetails {
    #[serde(rename = "depositIdentifierHash")]
    pub deposit_identifier_hash: String,
    #[serde(rename = "ephemeralPubkey")]
    pub ephemeral_pubkey: String,
    #[serde(deserialize_with = "deserialize_u128_map")]
    pub balance: HashMap<String, u128>,
    #[serde(rename = "pmpCodeHash")]
    pub pmp_code_hash: String,
    #[serde(rename = "privateNoteCodeHash")]
    pub private_note_code_hash: String,
    #[serde(rename = "busyAddress")]
    pub busy_address: Option<String>,
    #[serde(rename = "couponsValue", deserialize_with = "deserialize_u128")]
    pub coupons_value: u128,
    #[serde(rename = "hasWithdrawn")]
    pub has_withdrawn: bool,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote._depositIdentifierHash`.
pub struct ResultOfGetDepositIdentifierHash {
    #[serde(rename = "_depositIdentifierHash")]
    pub deposit_identifier_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote._pendingPlaceBuyLock`.
pub struct ResultOfGetPendingPlaceBuyLock {
    #[serde(rename = "_pendingPlaceBuyLock", deserialize_with = "deserialize_u128")]
    pub pending_place_buy_lock: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote._pendingPlaceBuyTokenType`.
pub struct ResultOfGetPendingPlaceBuyTokenType {
    #[serde(rename = "_pendingPlaceBuyTokenType", deserialize_with = "deserialize_u32")]
    pub pending_place_buy_token_type: u32,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `PrivateNote._stakes`.
///
/// Stake entries are intentionally kept as raw JSON to keep the wrapper stable
/// across DEX stake tuple schema changes.
pub struct ResultOfGetStakes {
    #[serde(rename = "_stakes")]
    pub stakes: HashMap<String, serde_json::Value>,
}

impl PrivateNote {
    /// Create a wrapper for a deployed `PrivateNote`.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// # Change ephemeral owner key
    ///
    /// Original contract method: `changeOwner`
    ///
    /// Should be signed with current ephemeral owner keys
    pub async fn change_owner(
        &self,
        params: ParamsOfChangeOwner,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "changeOwner".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Deploy PMP
    ///
    /// Original contract method: `deployPMP`
    ///
    /// Should be signed with PrivateNote owner keys
    pub async fn deploy_pmp(
        &self,
        params: ParamsOfDeployPmp,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deployPMP".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for accepted initial full-set stakes
    ///
    /// Original contract method: `onInitialStakesAccepted`
    pub async fn on_initial_stakes_accepted(
        &self,
        params: ParamsOfOnInitialStakesAccepted,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onInitialStakesAccepted".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for failed initial full-set stakes
    ///
    /// Original contract method: `onInitialStakesFailed`
    pub async fn on_initial_stakes_failed(
        &self,
        params: ParamsOfOnInitialStakesFailed,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onInitialStakesFailed".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for PMP-side clean refund
    ///
    /// Original contract method: `onPmpCleanRefund`
    pub async fn on_pmp_clean_refund(
        &self,
        params: ParamsOfOnPmpCleanRefund,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onPmpCleanRefund".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Abandon stake (forfeit any claim against the PMP)
    ///
    /// Original contract method: `deleteStake`
    ///
    /// Should be signed with PrivateNote owner keys
    ///
    /// Before deleting the local stake record, the PrivateNote notifies the
    /// PMP via `forfeitStake(...)`; the record is deleted when the PMP acks
    /// back through `onForfeitAccepted`.
    pub async fn delete_stake(
        &self,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deleteStake".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback acknowledging forfeit
    ///
    /// Original contract method: `onForfeitAccepted`
    ///
    /// PMP→PrivateNote callback acknowledging `forfeitStake`: deletes the
    /// local stake record and clears the busy lock.
    pub async fn on_forfeit_accepted(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "onForfeitAccepted".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Cancel stake on PMP
    ///
    /// Original contract method: `cancelStake`
    ///
    /// Should be signed with PrivateNote owner keys
    pub async fn cancel_stake(
        &self,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelStake".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for canceled stake
    ///
    /// Original contract method: `onStakeCancelled`
    pub async fn on_stake_cancelled(
        &self,
        params: ParamsOfOnStakeCancelled,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onStakeCancelled".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Split full set on PMP
    ///
    /// Original contract method: `splitFullSet`
    pub async fn split_full_set(
        &self,
        params: ParamsOfSplitFullSet,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "splitFullSet".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for accepted split
    ///
    /// Original contract method: `onSplitAccepted`
    pub async fn on_split_accepted(
        &self,
        params: ParamsOfOnSplitAccepted,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onSplitAccepted".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Merge full set on PMP
    ///
    /// Original contract method: `mergeFullSet`
    pub async fn merge_full_set(
        &self,
        params: ParamsOfMergeFullSet,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "mergeFullSet".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for accepted merge
    ///
    /// Original contract method: `onMergeAccepted`
    pub async fn on_merge_accepted(
        &self,
        params: ParamsOfOnMergeAccepted,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onMergeAccepted".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Place a single-outcome stake
    ///
    /// Original contract method: `setStake`
    ///
    /// Should be signed with PrivateNote owner keys
    pub async fn set_stake(
        &self,
        params: ParamsOfSetStake,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "setStake".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for accepted stake
    ///
    /// Original contract method: `onStakeAccepted`
    pub async fn on_stake_accepted(
        &self,
        params: ParamsOfOnStakeAccepted,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onStakeAccepted".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Claim PMP payout
    ///
    /// Original contract method: `claim`
    ///
    /// Should be signed with PrivateNote owner keys
    pub async fn claim(
        &self,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "claim".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback for accepted claim
    ///
    /// Original contract method: `onClaimAccepted`
    pub async fn on_claim_accepted(
        &self,
        params: ParamsOfOnClaimAccepted,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onClaimAccepted".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Accept creator fee from PMP
    ///
    /// Original contract method: `acceptFee`
    pub async fn accept_fee(
        &self,
        params: ParamsOfAcceptFee,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "acceptFee".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Generate coupon
    ///
    /// Original contract method: `generateCoupon`
    ///
    /// Should be signed with PrivateNote owner keys
    pub async fn generate_coupon(
        &self,
        params: ParamsOfGenerateCoupon,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "generateCoupon".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Initiate transfer to another PrivateNote
    ///
    /// Original contract method: `initTransfer`
    pub async fn init_transfer(
        &self,
        params: ParamsOfInitTransfer,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "initTransfer".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Receive transfer offer callback
    ///
    /// Original contract method: `offerTransfer`
    pub async fn offer_transfer(
        &self,
        params: ParamsOfOfferTransfer,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "offerTransfer".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Confirm accepted transfer callback
    ///
    /// Original contract method: `onTransferAccepted`
    pub async fn on_transfer_accepted(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "onTransferAccepted".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Clear busy transfer state
    ///
    /// Original contract method: `clearTransferBusy`
    pub async fn clear_transfer_busy(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "clearTransferBusy".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Discard unused coupon
    ///
    /// Original contract method: `discardCoupon`
    pub async fn discard_coupon(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "discardCoupon".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Withdraw tokens via RootPN vault
    ///
    /// Original contract method: `withdrawTokens`
    ///
    /// Should be signed with PrivateNote owner keys
    pub async fn withdraw_tokens(
        &self,
        params: ParamsOfWithdrawTokens,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "withdrawTokens".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Revert token withdraw callback
    ///
    /// Original contract method: `revertWithdraw`
    pub async fn revert_withdraw(
        &self,
        params: ParamsOfRevertWithdraw,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "revertWithdraw".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    // ─── OrderBook proxy methods ──────────────────────────────────────

    /// Original contract method: `placeOrder`. Submits a single order to
    /// the PMP's `OrderBook`.
    pub async fn place_order(
        &self,
        params: ParamsOfPlaceOrder,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "placeOrder".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `placeBatch`. Atomic batch: cancels
    /// `cancel_ids` and places `orders` in a single OrderBook dispatch.
    pub async fn place_batch(
        &self,
        params: ParamsOfPlaceBatch,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "placeBatch".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `cancelOrder`.
    pub async fn cancel_order(
        &self,
        params: ParamsOfCancelOrder,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelOrder".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `cancelOrderByClient`.
    pub async fn cancel_order_by_client(
        &self,
        params: ParamsOfCancelOrderByClient,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelOrderByClient".to_string(),
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

    // ─── OrderBook → PrivateNote callback bindings ────────────────────
    // These methods are normally only invoked by the OrderBook itself.
    // Exposed here so consumers can build admin tools / event-replay /
    // tests that need to call them directly.

    /// Original contract method: `onOrderPlaced` (callback from OrderBook).
    pub async fn on_order_placed(
        &self,
        params: ParamsOfOnOrderPlaced,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onOrderPlaced".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `onOrderRejected` (callback from OrderBook).
    pub async fn on_order_rejected(
        &self,
        params: ParamsOfOnOrderRejected,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onOrderRejected".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `onOrderCancelled` (callback from OrderBook).
    pub async fn on_order_cancelled(
        &self,
        params: ParamsOfOnOrderCancelled,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onOrderCancelled".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `onOrderFilled` (callback from OrderBook).
    pub async fn on_order_filled(
        &self,
        params: ParamsOfOnOrderFilled,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onOrderFilled".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `onBatchComplete` (callback from OrderBook).
    pub async fn on_batch_complete(
        &self,
        params: ParamsOfOnBatchComplete,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "onBatchComplete".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    // ─── Inference market: stream locks (spec §4.3) ───────────────────
    // Stream/dispute locks are set by a streaming-deal (TokenContract); while
    // any lock is held the note cannot withdraw / split / merge. The contract
    // gates these on `msg.sender == deal`, so they are normally only invoked by
    // the deal — exposed here for admin tools and tests.

    /// Original contract method: `streamLock` (callback from a streaming deal).
    pub async fn stream_lock(
        &self,
        params: ParamsOfStreamLock,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "streamLock".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `streamUnlock` (callback from a streaming deal).
    pub async fn stream_unlock(
        &self,
        params: ParamsOfStreamLock,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "streamUnlock".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `streamDisputeLock` (callback from a streaming
    /// deal).
    pub async fn stream_dispute_lock(
        &self,
        params: ParamsOfStreamLock,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "streamDisputeLock".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// Original contract method: `streamDisputeUnlock` (callback from a
    /// streaming deal).
    pub async fn stream_dispute_unlock(
        &self,
        params: ParamsOfStreamLock,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "streamDisputeUnlock".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Owner escape hatch: clear stale stream/dispute locks
    ///
    /// Original contract method: `forceClearStreamLocks`
    ///
    /// Should be signed with PrivateNote owner keys. Only succeeds once the
    /// locks have been stale longer than the contract's `STREAM_LOCK_MAX`.
    pub async fn force_clear_stream_locks(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "forceClearStreamLocks".to_string(),
            header: None,
            input: None,
        };
        self.send_message(Some(call_set), None, signer).await
    }

    // ─── Inference market: order book + streaming deals (spec §2-§8) ──
    // Inference settles in SHELL held physically by the note, so the note
    // itself is the market participant. All of these are owner-signed.

    /// # Deploy an InferenceOrderBook from this note
    ///
    /// Original contract method: `deployInferenceOrderBook`
    ///
    /// Should be signed with PrivateNote owner keys. Permissionless at the
    /// deterministic `(model, tick)` address.
    pub async fn deploy_inference_order_book(
        &self,
        params: ParamsOfInferenceOrderBook,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deployInferenceOrderBook".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Post a SELL offer to an InferenceOrderBook
    ///
    /// Original contract method: `postSellOffer`
    ///
    /// Should be signed with PrivateNote owner keys.
    pub async fn post_sell_offer(
        &self,
        params: ParamsOfPostSellOffer,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "postSellOffer".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Place a BUY order with SHELL escrow
    ///
    /// Original contract method: `placeInferenceBuy`
    ///
    /// Should be signed with PrivateNote owner keys.
    pub async fn place_inference_buy(
        &self,
        params: ParamsOfPlaceInferenceBuy,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "placeInferenceBuy".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Place a subscription (semantic order)
    ///
    /// Original contract method: `placeInferenceSubscription`
    ///
    /// Should be signed with PrivateNote owner keys.
    pub async fn place_inference_subscription(
        &self,
        params: ParamsOfPlaceInferenceSubscription,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "placeInferenceSubscription".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Cancel one resting inference order owned by this note
    ///
    /// Original contract method: `cancelInferenceOrder`
    ///
    /// Should be signed with PrivateNote owner keys.
    pub async fn cancel_inference_order(
        &self,
        params: ParamsOfCancelInferenceOrder,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelInferenceOrder".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Cancel all resting inference orders owned by this note
    ///
    /// Original contract method: `cancelAllInferenceOrders`
    ///
    /// Should be signed with PrivateNote owner keys.
    pub async fn cancel_all_inference_orders(
        &self,
        params: ParamsOfCancelAllInferenceOrders,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "cancelAllInferenceOrders".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Buyer note stops the stream (amicable exit, spec §4.1)
    ///
    /// Original contract method: `streamStop`
    ///
    /// Should be signed with PrivateNote owner keys.
    pub async fn stream_stop(
        &self,
        params: ParamsOfStreamDeal,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "streamStop".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Buyer note disputes the current ticks (spec §4.2)
    ///
    /// Original contract method: `streamDispute`
    ///
    /// Should be signed with PrivateNote owner keys.
    pub async fn stream_dispute(
        &self,
        params: ParamsOfStreamDeal,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "streamDispute".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    // ─── Getters ──────────────────────────────────────────────────────

    /// # Get salted PMP code and hash
    ///
    /// Original contract method: `getPMPCode`
    pub async fn get_pmp_code(&self) -> KitResult<ResultOfGetPmpCode> {
        self.call_get_method::<ResultOfGetPmpCode>("getPMPCode").await
    }

    /// # Get PrivateNote details
    ///
    /// Original contract method: `getDetails`
    pub async fn get_details(&self) -> KitResult<ResultOfGetDetails> {
        self.call_get_method::<ResultOfGetDetails>("getDetails").await
    }

    /// # Get deposit identifier hash (public static field getter)
    ///
    /// Original contract method: `_depositIdentifierHash`
    pub async fn get_deposit_identifier_hash(&self) -> KitResult<ResultOfGetDepositIdentifierHash> {
        self.call_get_method::<ResultOfGetDepositIdentifierHash>("_depositIdentifierHash").await
    }

    /// # Get raw `_stakes` mapping
    ///
    /// Original contract method: `_stakes`
    ///
    /// Returns raw JSON entries because stake tuple schema is large and evolves
    /// frequently in DEX contract iterations.
    pub async fn get_stakes(&self) -> KitResult<ResultOfGetStakes> {
        self.call_get_method::<ResultOfGetStakes>("_stakes").await
    }

    /// # Get pending place-buy lock amount
    ///
    /// Original contract method: `_pendingPlaceBuyLock`
    pub async fn get_pending_place_buy_lock(&self) -> KitResult<ResultOfGetPendingPlaceBuyLock> {
        self.call_get_method::<ResultOfGetPendingPlaceBuyLock>("_pendingPlaceBuyLock").await
    }

    /// # Get pending place-buy token type
    ///
    /// Original contract method: `_pendingPlaceBuyTokenType`
    pub async fn get_pending_place_buy_token_type(
        &self,
    ) -> KitResult<ResultOfGetPendingPlaceBuyTokenType> {
        self.call_get_method::<ResultOfGetPendingPlaceBuyTokenType>("_pendingPlaceBuyTokenType")
            .await
    }

    /// # Get inference-market stream/dispute lock state
    ///
    /// Original contract method: `getStreamLocks`
    pub async fn get_stream_locks(&self) -> KitResult<ResultOfGetStreamLocks> {
        self.call_get_method::<ResultOfGetStreamLocks>("getStreamLocks").await
    }

    /// # Get deterministic InferenceOrderBook address for `(model, tick)`
    ///
    /// Original contract method: `getInferenceOrderBookAddress`
    pub async fn get_inference_order_book_address(
        &self,
        params: ParamsOfInferenceOrderBook,
    ) -> KitResult<ResultOfGetInferenceOrderBookAddress> {
        self.call_get_method_with::<ResultOfGetInferenceOrderBookAddress, ParamsOfInferenceOrderBook>(
            "getInferenceOrderBookAddress",
            params,
        )
        .await
    }
}
