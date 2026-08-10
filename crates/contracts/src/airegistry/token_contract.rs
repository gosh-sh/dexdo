use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u16;
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

const ABI: &str = include_str!("../../../../contracts/airegistry/TokenContract.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the AI Registry `TokenContract` contract — the per-deal
/// streaming escrow that holds the buyer's SHELL deposit and settles ticks
/// tick-by-tick (spec §2-§4, probe model §3.1.2).
pub struct TokenContract {
    base: ContractBase,
}

impl ModuleAccessor for TokenContract {
    const MODULE: KitModule = KitModule::External("airegistry.token_contract");
}

impl HasContractBase for TokenContract {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for TokenContract {}

impl AsyncGuarded<Account> for TokenContract {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for TokenContract {
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
/// Parameters for `TokenContract.fundFromOrderBook` (callback from the order
/// book on a match; sender must be the order book).
pub struct ParamsOfFundFromOrderBook {
    /// SHELL the match actually moved into the deal.
    pub paid: u128,
    pub buyer_note: String,
    /// `uint256`, decimal or hex string.
    pub buyer_pubkey: String,
    /// Shape of the matched order, carried over from the taker's flags — this is
    /// how a deal learns it is a subscription rather than a one-off.
    pub deal_flags: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `TokenContract.open` (seller posts the encrypted endpoint
/// and freezes the probe tick).
pub struct ParamsOfOpen {
    /// `bytes` as a hex string.
    pub endpoint_cipher: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `TokenContract.withdrawShell`.
pub struct ParamsOfWithdrawShell {
    pub amount: u128,
}

// ─── Result structs ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `TokenContract.getOffer`.
///
/// The whole sell-offer chain (note → `postFromNote` → book) is `bounce:false`,
/// so this latch is the only readable evidence of where an offer got to:
/// `offer_posted` is set the moment the TC forwards to the book and cleared
/// again by `onSellClosed` when the book refuses to rest it.
pub struct ResultOfGetOffer {
    pub offer_posted: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `TokenContract.getState`.
pub struct ResultOfGetState {
    pub funded: bool,
    pub opened: bool,
    pub probe_accepted: bool,
    pub disputed: bool,
    #[serde(deserialize_with = "deserialize_u128")]
    pub deposit: u128,
    /// Ticks frozen at probe acceptance, the reference the deal is priced from.
    #[serde(deserialize_with = "deserialize_u128")]
    pub probe_tick: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub finalized_owed: u128,
    /// Cumulative tokens the deal has actually credited — the `trusted` figure
    /// carried by `TicksClaimed`.
    #[serde(deserialize_with = "deserialize_u128")]
    pub tokens_final: u128,
    /// Claimed but not yet trusted — the `claimed` figure on `TicksClaimed`.
    #[serde(deserialize_with = "deserialize_u128")]
    pub tokens_pending: u128,
    #[serde(deserialize_with = "deserialize_u64")]
    pub probe_time: u64,
    /// Claims are rate-limited: `MIN_CLAIM_INTERVAL` (60 s) must elapse since
    /// this, or `claimTokens` reverts with `ERR_SETTLE_WINDOW_OPEN`.
    #[serde(deserialize_with = "deserialize_u64")]
    pub last_claim_time: u64,
    #[serde(deserialize_with = "deserialize_u64")]
    pub dispute_time: u64,
    /// When the order-book match handed this deal its escrow. The no-show
    /// cleanup window (`MATCH_OPEN_TIMEOUT`) is measured from here, so a
    /// caller can tell how long it still has to wait rather than guess.
    #[serde(deserialize_with = "deserialize_u64")]
    pub funded_time: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `TokenContract.getSellerBond` — the seller's mirror bond, the only
/// seller collateral held against the deal (spec §4.2).
pub struct ResultOfGetSellerBond {
    pub bond_funded: bool,
    #[serde(deserialize_with = "deserialize_u128")]
    pub bond_held: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub bond_required: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `TokenContract.getConfig` (protocol-wide constants, spec §9.1).
pub struct ResultOfGetConfig {
    #[serde(deserialize_with = "deserialize_u16")]
    pub platform_fee_bps: u16,
    /// Minimum gap between two `claimTokens` calls, seconds. A claim inside it
    /// reverts with `ERR_SETTLE_WINDOW_OPEN`.
    #[serde(deserialize_with = "deserialize_u64")]
    pub min_claim_interval: u64,
    #[serde(deserialize_with = "deserialize_u64")]
    pub min_seconds_per_tick: u64,
    #[serde(deserialize_with = "deserialize_u64")]
    pub dispute_window: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `TokenContract.getFees`.
pub struct ResultOfGetFees {
    #[serde(deserialize_with = "deserialize_u128")]
    pub fee_accrued: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub ticks_finalized: u128,
    pub ever_disputed: bool,
    #[serde(deserialize_with = "deserialize_u16")]
    pub rebate_max_bps: u16,
    #[serde(deserialize_with = "deserialize_u16")]
    pub rebate_slope_bps: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `TokenContract.getDeal`.
pub struct ResultOfGetDeal {
    #[serde(deserialize_with = "deserialize_u128")]
    pub tick_size: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub price_per_tick: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub max_ticks: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Result of `TokenContract.getParties`.
pub struct ResultOfGetParties {
    pub buyer: String,
    pub seller_note: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `TokenContract.getSeller`.
pub struct ResultOfGetSeller {
    #[serde(rename = "sellerPubkey")]
    pub seller_pubkey: String,
    #[serde(rename = "rootModelAddress")]
    pub root_model_address: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub nonce: u64,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `TokenContract.getBuyerPubkey`.
pub struct ResultOfGetBuyerPubkey {
    #[serde(rename = "value0")]
    pub buyer_pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `TokenContract.getEndpointCipher`.
pub struct ResultOfGetEndpointCipher {
    /// `bytes` as a hex string.
    #[serde(rename = "value0")]
    pub endpoint_cipher: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `TokenContract.getModelName`.
pub struct ResultOfGetModelName {
    #[serde(rename = "value0")]
    pub model_name: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `TokenContract.getShellBalance` — physical ECC[2] SHELL held.
pub struct ResultOfGetShellBalance {
    #[serde(rename = "value0", deserialize_with = "deserialize_u128")]
    pub value: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `TokenContract.getVersion` — `(version, contractName)`.
pub struct ResultOfGetVersion {
    #[serde(rename = "value0")]
    pub version: String,
    #[serde(rename = "value1")]
    pub name: String,
}

impl TokenContract {
    /// Create a wrapper for a deployed `TokenContract`.
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    // ─── Funding ──────────────────────────────────────────────────────

    /// # Fund from a matched order book buy (sender must be the order book)
    ///
    /// Original contract method: `fundFromOrderBook`
    pub async fn fund_from_order_book(
        &self,
        params: ParamsOfFundFromOrderBook,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "fundFromOrderBook".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    // ─── Streaming lifecycle ──────────────────────────────────────────

    /// # Seller opens the stream (freezes the probe tick, spec §3.1.2)
    ///
    /// Original contract method: `open`
    pub async fn open(
        &self,
        params: ParamsOfOpen,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "open".to_string(), header: None, input: Some(json!(params)) };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Buyer stops the stream cleanly (spec §4.1)
    ///
    /// Original contract method: `stop`
    pub async fn stop(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet { function_name: "stop".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Buyer disputes the current ticks (spec §4.2)
    ///
    /// Original contract method: `dispute`
    pub async fn dispute(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet { function_name: "dispute".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Buyer releases a dispute it raised
    ///
    /// Original contract method: `releaseDispute`
    pub async fn release_dispute(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "releaseDispute".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Seller resolves a dispute after the dispute window (50/50 / burn)
    ///
    /// Original contract method: `resolveDisputeTimeout`
    pub async fn resolve_dispute_timeout(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "resolveDisputeTimeout".to_string(),
            header: None,
            input: None,
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Recover funds from a funded-but-unopened deal (seller no-show, §2.1)
    ///
    /// Original contract method: `cleanupUnopened`
    pub async fn cleanup_unopened(&self, signer: Signer) -> KitResult<ResultOfSendMessage> {
        let call_set =
            CallSet { function_name: "cleanupUnopened".to_string(), header: None, input: None };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Seller withdraws finalized SHELL
    ///
    /// Original contract method: `withdrawShell`
    pub async fn withdraw_shell(
        &self,
        params: ParamsOfWithdrawShell,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "withdrawShell".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    // ─── Getters ──────────────────────────────────────────────────────

    /// Original contract method: `getState`.
    pub async fn get_state(&self) -> KitResult<ResultOfGetState> {
        self.call_get_method::<ResultOfGetState>("getState").await
    }

    /// Original contract method: `getSellerBond`.
    pub async fn get_seller_bond(&self) -> KitResult<ResultOfGetSellerBond> {
        self.call_get_method::<ResultOfGetSellerBond>("getSellerBond").await
    }

    /// Original contract method: `getOffer`.
    pub async fn get_offer(&self) -> KitResult<ResultOfGetOffer> {
        self.call_get_method::<ResultOfGetOffer>("getOffer").await
    }

    /// Original contract method: `getConfig`.
    pub async fn get_config(&self) -> KitResult<ResultOfGetConfig> {
        self.call_get_method::<ResultOfGetConfig>("getConfig").await
    }

    /// Original contract method: `getFees`.
    pub async fn get_fees(&self) -> KitResult<ResultOfGetFees> {
        self.call_get_method::<ResultOfGetFees>("getFees").await
    }

    /// Original contract method: `getDeal`.
    pub async fn get_deal(&self) -> KitResult<ResultOfGetDeal> {
        self.call_get_method::<ResultOfGetDeal>("getDeal").await
    }

    /// Original contract method: `getParties`.
    pub async fn get_parties(&self) -> KitResult<ResultOfGetParties> {
        self.call_get_method::<ResultOfGetParties>("getParties").await
    }

    /// Original contract method: `getSeller`.
    pub async fn get_seller(&self) -> KitResult<ResultOfGetSeller> {
        self.call_get_method::<ResultOfGetSeller>("getSeller").await
    }

    /// Original contract method: `getBuyerPubkey`.
    pub async fn get_buyer_pubkey(&self) -> KitResult<ResultOfGetBuyerPubkey> {
        self.call_get_method::<ResultOfGetBuyerPubkey>("getBuyerPubkey").await
    }

    /// Original contract method: `getEndpointCipher`.
    pub async fn get_endpoint_cipher(&self) -> KitResult<ResultOfGetEndpointCipher> {
        self.call_get_method::<ResultOfGetEndpointCipher>("getEndpointCipher").await
    }

    /// Original contract method: `getModelName`.
    pub async fn get_model_name(&self) -> KitResult<ResultOfGetModelName> {
        self.call_get_method::<ResultOfGetModelName>("getModelName").await
    }

    /// Original contract method: `getShellBalance`.
    pub async fn get_shell_balance(&self) -> KitResult<ResultOfGetShellBalance> {
        self.call_get_method::<ResultOfGetShellBalance>("getShellBalance").await
    }

    /// Original contract method: `getVersion`.
    pub async fn get_version(&self) -> KitResult<ResultOfGetVersion> {
        self.call_get_method::<ResultOfGetVersion>("getVersion").await
    }
}
