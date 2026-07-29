// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Methods compiled only with the `test-helpers` feature — deploy +
// setup entry points plus the read-only PN getters used by e2e tests
// to verify on-chain state. Used by the api crate's e2e integration
// tests, which spawn an ephemeral PMP + OrderBook per run before
// exercising the trader write-path, and poll PN getters to assert
// on-chain effects.
//
// The prod api/infrastructure build leaves `test-helpers` off so
// `Dex` exposes only the trader-path methods in `client.rs`.

use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use dodex_contracts::airegistry::inference_order_book::InferenceOrderBook;
use dodex_contracts::airegistry::inference_order_book::ParamsOfGetOrder as IobParamsOfGetOrder;
use dodex_contracts::airegistry::inference_order_book::ParamsOfOrderId as IobParamsOfOrderId;
use dodex_contracts::airegistry::inference_order_book::ResultOfGetBestBidAsk;
use dodex_contracts::airegistry::inference_order_book::ResultOfGetOrder as IobOrder;
use dodex_contracts::airegistry::inference_order_book::ResultOfGetStats as IobStats;
use dodex_contracts::airegistry::inference_order_book::ResultOfGetSubscription as IobSubscription;
use dodex_contracts::airegistry::token_contract::ParamsOfOpen;
use dodex_contracts::airegistry::token_contract::ParamsOfWithdrawShell;
use dodex_contracts::airegistry::token_contract::ResultOfGetFees as TcFees;
use dodex_contracts::airegistry::token_contract::ResultOfGetOffer as TcOffer;
use dodex_contracts::airegistry::token_contract::ResultOfGetParties as TcParties;
use dodex_contracts::airegistry::token_contract::ResultOfGetSellerBond as TcSellerBond;
use dodex_contracts::airegistry::token_contract::ResultOfGetState as TcState;
use dodex_contracts::airegistry::token_contract::TokenContract;
use dodex_contracts::dex::oracle::Oracle;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::OracleEventList;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::oracle_event_list::ResultOfGetEvents;
use dodex_contracts::dex::pmp::ParamsOfSubmitResolve;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::pmp::ResultOfGetDetails as PmpKitDetails;
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfCancelInferenceOrder;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceSubscription;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use dodex_contracts::dex::private_note::ParamsOfPostSellerBond;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfStreamDeal;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_contracts::dex::private_note::ResultOfGetDetails as PnDetails;
use dodex_contracts::dex::private_note::ResultOfGetStakes as PnStakesRaw;
use dodex_contracts::dex::root_oracle::ParamsOfDeployOracle;
use dodex_contracts::dex::root_oracle::ParamsOfGetOracleAddress;
use dodex_contracts::dex::root_oracle::RootOracle;
use dodex_contracts::dex::root_pn::ParamsOfGetPmpAddress;
use dodex_contracts::dex::root_pn::RootPn;

use super::client::Dex;
use super::dapp::dex_contract_params;
use super::dapp::self_rooted_contract_params;
use super::error::ChainResult;

/// Shape returned by `Pmp.getDetails`. Aliased so call sites don't
/// have to reach for the kit's three-deep contract path.
pub type PmpDetails = PmpKitDetails;

/// Shape returned by `OracleEventList.getEvents`. `.events` is a
/// `HashMap<String, serde_json::Value>` keyed by event id.
pub type OracleEvents = ResultOfGetEvents;

/// Account-level state of an `InferenceOrderBook`, independent of whether its
/// getters run. Returned by `Dex::inference_book_account`.
#[derive(Debug, Clone)]
pub struct IobAccount {
    /// `AccountStatus` rendered for a message: `Active`, `Uninit`, `Frozen`,
    /// `NonExist`.
    pub acc_type: String,
    /// Hash of the account's code cell. `None` before the account holds state.
    pub code_hash: Option<String>,
    /// Native balance in nanovmshell.
    pub balance: Option<String>,
}

impl Dex {
    // ── PrivateNote (deployer-side) ──────────────────────────────────

    pub async fn deploy_pmp(
        &self,
        pn_address: &str,
        params: ParamsOfDeployPmp,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .deploy_pmp(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn set_stake(
        &self,
        pn_address: &str,
        params: ParamsOfSetStake,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .set_stake(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Read-only `PrivateNote.getDetails` accessor. Surfaces
    /// `_balance[tokenType]`, `_busy.busyAddress`, and a handful of
    /// other public PN fields without touching the read-model. The
    /// production API reads PN state via the GraphQL-backed
    /// `pn_state_reader`; this direct accessor exists so e2e tests can
    /// verify on-chain balance moves after `splitFullSet` and poll the
    /// `_busy` window between submission and the `onSplitAccepted`
    /// callback without standing up the GraphQL gateway.
    pub async fn get_private_note_details(&self, pn_address: &str) -> ChainResult<PnDetails> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .get_details()
            .await
            .map_err(Into::into)
    }

    /// Read-only `PrivateNote._stakes` accessor. Returns the raw
    /// `HashMap<String, Value>` keyed by `tvm.hash(eventId,
    /// oracleListHash, tokenType)` (0x-prefixed lowercase hex). Each
    /// value carries the StakeInfo tuple — `amount`, `debtAmount`,
    /// `couponsAmount` plus bookkeeping fields. Sibling of
    /// `get_private_note_details`, scoped to the same e2e niche.
    pub async fn get_private_note_stakes(&self, pn_address: &str) -> ChainResult<PnStakesRaw> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .get_stakes()
            .await
            .map_err(Into::into)
    }

    // ── PMP (oracle-signed ops + getters) ────────────────────────────

    pub async fn submit_set_timings(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitSetTimings,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
            .submit_set_timings(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn submit_resolve(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitResolve,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
            .submit_resolve(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_pmp_details(&self, pmp_address: &str) -> ChainResult<PmpDetails> {
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
            .get_details()
            .await
            .map_err(Into::into)
    }

    pub async fn get_order_book_address(&self, pmp_address: &str) -> ChainResult<String> {
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
            .get_order_book_address()
            .await
            .map(|r| r.order_book_address)
            .map_err(Into::into)
    }

    // ── RootPN (address derivation) ──────────────────────────────────

    pub async fn get_pmp_address(
        &self,
        event_id: String,
        names: Vec<String>,
        token_type: u32,
    ) -> ChainResult<String> {
        RootPn::new(self.ctx.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS))
            .get_pmp_address(ParamsOfGetPmpAddress { event_id, names, token_type })
            .await
            .map(|r| r.pmp_address)
            .map_err(Into::into)
    }

    // ── RootOracle ───────────────────────────────────────────────────

    pub async fn deploy_oracle(
        &self,
        params: ParamsOfDeployOracle,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        RootOracle::new(self.ctx.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS))
            .deploy_oracle(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_oracle_address(&self, name: String) -> ChainResult<String> {
        RootOracle::new(self.ctx.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS))
            .get_oracle_address(ParamsOfGetOracleAddress { name })
            .await
            .map(|r| r.oracle_address)
            .map_err(Into::into)
    }

    // ── Oracle + OracleEventList ─────────────────────────────────────

    pub async fn get_event_list_address(
        &self,
        oracle_address: &str,
        params: ParamsOfGetEventListAddress,
    ) -> ChainResult<String> {
        Oracle::new(self.ctx.clone(), dex_contract_params(oracle_address))
            .get_event_list_address(params)
            .await
            .map(|r| r.address)
            .map_err(Into::into)
    }

    pub async fn add_event(
        &self,
        event_list_address: &str,
        params: ParamsOfAddEvent,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        OracleEventList::new(self.ctx.clone(), dex_contract_params(event_list_address))
            .add_event(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_events(&self, event_list_address: &str) -> ChainResult<OracleEvents> {
        OracleEventList::new(self.ctx.clone(), dex_contract_params(event_list_address))
            .get_events()
            .await
            .map_err(Into::into)
    }
}

// ── AI Registry: inference market (spec §2-§8) ───────────────────────────
//
// The note (`PrivateNote`) is the on-chain participant — it deploys the
// per-model `InferenceOrderBook`, posts SELL offers backed by a
// `TokenContract`, and places BUY orders with SHELL escrow. Settlement runs
// on the `TokenContract` (open → advance → stop/dispute/reclaim). These all
// stay behind `test-helpers`: there are no inference request handlers yet,
// so they would otherwise be dead weight in the prod binary.
impl Dex {
    // ── PrivateNote (inference participant write-path) ────────────────

    pub async fn deploy_inference_order_book(
        &self,
        pn_address: &str,
        params: ParamsOfDeployInferenceOrderBook,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .deploy_inference_order_book(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_inference_order_book_address(
        &self,
        pn_address: &str,
        params: ParamsOfInferenceOrderBook,
    ) -> ChainResult<String> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .get_inference_order_book_address(params)
            .await
            .map(|r| r.address)
            .map_err(Into::into)
    }

    pub async fn post_sell_offer(
        &self,
        pn_address: &str,
        params: ParamsOfPostSellOffer,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .post_sell_offer(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Post the seller mirror bond from the note. The TC accepts the bond from
    /// its `_sellerNote` and nothing else, so this is the only funding path.
    pub async fn post_seller_bond(
        &self,
        pn_address: &str,
        params: ParamsOfPostSellerBond,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .post_seller_bond(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn place_inference_buy(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceInferenceBuy,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .place_inference_buy(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn cancel_inference_order(
        &self,
        pn_address: &str,
        params: ParamsOfCancelInferenceOrder,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .cancel_inference_order(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn cancel_all_inference_orders(
        &self,
        pn_address: &str,
        params: ParamsOfCancelAllInferenceOrders,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .cancel_all_inference_orders(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn place_inference_subscription(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceInferenceSubscription,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .place_inference_subscription(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Buyer note stops the stream cleanly (`PrivateNote.streamStop`).
    pub async fn stream_stop(
        &self,
        pn_address: &str,
        params: ParamsOfStreamDeal,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .stream_stop(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Buyer note disputes the current ticks (`PrivateNote.streamDispute`).
    pub async fn stream_dispute(
        &self,
        pn_address: &str,
        params: ParamsOfStreamDeal,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .stream_dispute(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Buyer note reclaims a probe tick on seller no-show
    /// (`PrivateNote.streamReclaim`).
    pub async fn stream_reclaim(
        &self,
        pn_address: &str,
        params: ParamsOfStreamDeal,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .stream_reclaim(params, signer)
            .await
            .map_err(Into::into)
    }

    // ── InferenceOrderBook (read-only assertions) ─────────────────────

    pub async fn inference_get_order(
        &self,
        order_book_address: &str,
        order_id: u128,
    ) -> ChainResult<IobOrder> {
        InferenceOrderBook::new(self.ctx.clone(), dex_contract_params(order_book_address))
            .get_order(IobParamsOfGetOrder { id: order_id })
            .await
            .map_err(Into::into)
    }

    pub async fn inference_get_stats(&self, order_book_address: &str) -> ChainResult<IobStats> {
        InferenceOrderBook::new(self.ctx.clone(), dex_contract_params(order_book_address))
            .get_stats()
            .await
            .map_err(Into::into)
    }

    /// Raw account state of a book, for telling apart the ways a book can fail
    /// to answer its getters. `getStats` reports the same "cannot run" error
    /// whether the account is missing, uninitialised, or carries a code cell
    /// that has no runnable code — only the account itself separates them.
    pub async fn inference_book_account(
        &self,
        order_book_address: &str,
    ) -> ChainResult<IobAccount> {
        let ob = InferenceOrderBook::new(self.ctx.clone(), dex_contract_params(order_book_address));
        ob.fetch_account().await?;
        let account = ob.account().lock().await;
        Ok(IobAccount {
            acc_type: format!("{:?}", account.acc_type),
            code_hash: account.code_hash.clone(),
            balance: account.balance.as_ref().map(ToString::to_string),
        })
    }

    pub async fn inference_get_queue_size(&self, order_book_address: &str) -> ChainResult<u8> {
        InferenceOrderBook::new(self.ctx.clone(), dex_contract_params(order_book_address))
            .get_queue_size()
            .await
            .map(|r| r.size)
            .map_err(Into::into)
    }

    pub async fn inference_get_best_bid_ask(
        &self,
        order_book_address: &str,
    ) -> ChainResult<ResultOfGetBestBidAsk> {
        InferenceOrderBook::new(self.ctx.clone(), dex_contract_params(order_book_address))
            .get_best_bid_ask()
            .await
            .map_err(Into::into)
    }

    pub async fn inference_get_subscription(
        &self,
        order_book_address: &str,
        order_id: u128,
    ) -> ChainResult<IobSubscription> {
        InferenceOrderBook::new(self.ctx.clone(), dex_contract_params(order_book_address))
            .get_subscription(IobParamsOfOrderId { order_id })
            .await
            .map_err(Into::into)
    }

    pub async fn inference_get_weekly_median_price(
        &self,
        order_book_address: &str,
    ) -> ChainResult<String> {
        InferenceOrderBook::new(self.ctx.clone(), dex_contract_params(order_book_address))
            .get_weekly_median_price()
            .await
            .map(|r| r.price)
            .map_err(Into::into)
    }

    // ── TokenContract (streaming-deal settlement) ─────────────────────

    /// Seller opens the stream and freezes the probe tick
    /// (`TokenContract.open`).
    pub async fn token_contract_open(
        &self,
        token_contract_address: &str,
        params: ParamsOfOpen,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .open(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Seller advances one tick (`TokenContract.advance`).
    pub async fn token_contract_advance(
        &self,
        token_contract_address: &str,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .advance(signer)
            .await
            .map_err(Into::into)
    }

    /// Seller posts the mirror bond (`TokenContract.fundSellerBond`).
    pub async fn token_contract_fund_seller_bond(
        &self,
        token_contract_address: &str,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .fund_seller_bond(signer)
            .await
            .map_err(Into::into)
    }

    /// Seller resolves a dispute after the dispute window
    /// (`TokenContract.resolveDisputeTimeout`).
    pub async fn token_contract_resolve_dispute_timeout(
        &self,
        token_contract_address: &str,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .resolve_dispute_timeout(signer)
            .await
            .map_err(Into::into)
    }

    /// Seller withdraws finalized SHELL (`TokenContract.withdrawShell`).
    pub async fn token_contract_withdraw_shell(
        &self,
        token_contract_address: &str,
        params: ParamsOfWithdrawShell,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .withdraw_shell(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn token_contract_get_state(
        &self,
        token_contract_address: &str,
    ) -> ChainResult<TcState> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .get_state()
            .await
            .map_err(Into::into)
    }

    pub async fn token_contract_get_seller_bond(
        &self,
        token_contract_address: &str,
    ) -> ChainResult<TcSellerBond> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .get_seller_bond()
            .await
            .map_err(Into::into)
    }

    /// `ticksFinalized` here is the only bond-free measure of "did the seller get
    /// paid for a tick": `finalizedOwed` also carries the returned seller bond and
    /// the close rebate, both of which dwarf a single tick.
    pub async fn token_contract_get_fees(
        &self,
        token_contract_address: &str,
    ) -> ChainResult<TcFees> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .get_fees()
            .await
            .map_err(Into::into)
    }

    pub async fn token_contract_get_offer(
        &self,
        token_contract_address: &str,
    ) -> ChainResult<TcOffer> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .get_offer()
            .await
            .map_err(Into::into)
    }

    pub async fn token_contract_get_parties(
        &self,
        token_contract_address: &str,
    ) -> ChainResult<TcParties> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .get_parties()
            .await
            .map_err(Into::into)
    }

    pub async fn token_contract_get_shell_balance(
        &self,
        token_contract_address: &str,
    ) -> ChainResult<u128> {
        TokenContract::new(self.ctx.clone(), self_rooted_contract_params(token_contract_address))
            .get_shell_balance()
            .await
            .map(|r| r.value)
            .map_err(Into::into)
    }
}
