pub mod dto;

use ackinacki_kit::contracts::dex::oracle::ParamsOfGetEventListAddress;
use ackinacki_kit::contracts::dex::oracle::ParamsOfWithdrawFees;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfAddEvent;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfConfirmOrCancelEvent;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfDeleteEvent;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelAllOrders;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
use ackinacki_kit::contracts::dex::private_note::ParamsOfChangeOwner;
use ackinacki_kit::contracts::dex::private_note::ParamsOfDeployPmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfGenerateCoupon;
use ackinacki_kit::contracts::dex::private_note::ParamsOfInitTransfer;
use ackinacki_kit::contracts::dex::private_note::ParamsOfMergeFullSet;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSetStake;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSplitFullSet;
use ackinacki_kit::contracts::dex::private_note::ParamsOfStakeKey;
use ackinacki_kit::contracts::dex::private_note::ParamsOfWithdrawTokens;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfDeployOracle;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGenerateVoucher;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use ackinacki_kit::tvm_client::abi::Signer;
use dto::oracle::OracleEvents;
use dto::order_book::OrderBookDetails;
use dto::order_book::OrderBookShutdownState;
use dto::order_book::OrderInfo;
use dto::order_book::OwnedOrders;
use dto::pmp::PmpDetails;
use dto::pmp::PmpShutdownState;
use dto::private_note::AggregatedBalance;
use dto::private_note::PrivateNoteDetails;
use dto::private_note::PrivateNoteStakes;
use dto::private_note::ResultOfBlockchainWrite;

use crate::client::DexClient;
use crate::client::DexConfig;
use crate::dex_contract_params;
use crate::errors::AppResult;

pub struct Dex {
    inner: DexClient,
}

impl Dex {
    /// Creates a DEX client from the supplied `DexConfig`.
    pub fn new(config: DexConfig) -> AppResult<Self> {
        let inner = DexClient::new(config)?;
        Ok(Self { inner })
    }

    // ── PrivateNote ──────────────────────────────────────────────

    /// Step 0: Generate a voucher by sending ECC from multifactor wallet to
    /// RootPN. This is the entry point for creating a PrivateNote in
    /// production. `is_fee=false` for deposit tokens (NACKL/SHELL/USDC),
    /// `is_fee=true` for shell gas (converts ECC[2] → type 300 internally).
    pub async fn generate_voucher(
        &self,
        params: ParamsOfGenerateVoucher,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().generate_voucher(params, signer).await.map(Into::into)
    }

    pub async fn deploy_private_note(
        &self,
        params: ParamsOfDeployPrivateNote,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().deploy_private_note(params, signer).await.map(Into::into)
    }

    pub async fn send_ecc_shell(
        &self,
        params: ParamsOfSendEccShellToPrivateNote,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().send_ecc_shell(params, signer).await.map(Into::into)
    }

    pub async fn get_private_note_address(
        &self,
        params: ParamsOfGetPrivateNoteAddress,
    ) -> AppResult<String> {
        self.inner.private_note().get_private_note_address(params).await
    }

    pub async fn deploy_pmp(
        &self,
        pn_address: &str,
        params: ParamsOfDeployPmp,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().deploy_pmp(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn set_stake(
        &self,
        pn_address: &str,
        params: ParamsOfSetStake,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().set_stake(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn claim(
        &self,
        pn_address: &str,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().claim(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn cancel_stake(
        &self,
        pn_address: &str,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().cancel_stake(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn delete_stake(
        &self,
        pn_address: &str,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().delete_stake(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn change_owner(
        &self,
        pn_address: &str,
        params: ParamsOfChangeOwner,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().change_owner(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn init_transfer(
        &self,
        pn_address: &str,
        params: ParamsOfInitTransfer,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().init_transfer(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn withdraw_tokens(
        &self,
        pn_address: &str,
        params: ParamsOfWithdrawTokens,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().withdraw_tokens(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn generate_coupon(
        &self,
        pn_address: &str,
        params: ParamsOfGenerateCoupon,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().generate_coupon(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn discard_coupon(
        &self,
        pn_address: &str,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().discard_coupon(pn_address, signer).await.map(Into::into)
    }

    pub async fn place_order(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceOrder,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().place_order(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn place_batch(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceBatch,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().place_batch(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn cancel_order(
        &self,
        pn_address: &str,
        params: ParamsOfCancelOrder,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().cancel_order(pn_address, params, signer).await.map(Into::into)
    }

    pub async fn cancel_order_by_client(
        &self,
        pn_address: &str,
        params: ParamsOfCancelOrderByClient,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner
            .private_note()
            .cancel_order_by_client(pn_address, params, signer)
            .await
            .map(Into::into)
    }

    pub async fn cancel_all_orders(
        &self,
        pn_address: &str,
        params: ParamsOfCancelAllOrders,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner
            .private_note()
            .cancel_all_orders(pn_address, params, signer)
            .await
            .map(Into::into)
    }

    /// Split collateral on PMP into a vector of outcome tokens (one per
    /// outcome). Triggers `_ensureFrozen()` on PMP if called after `stakeEnd`,
    /// which spawns the OrderBook on first invocation.
    pub async fn split_full_set(
        &self,
        pn_address: &str,
        params: ParamsOfSplitFullSet,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().split_full_set(pn_address, params, signer).await.map(Into::into)
    }

    /// Merge a vector of outcome tokens back into PMP collateral. Must be
    /// called before `submitResolve` to recover collateral; afterwards the
    /// outcome tokens settle through the resolved-outcome path.
    pub async fn merge_full_set(
        &self,
        pn_address: &str,
        params: ParamsOfMergeFullSet,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.private_note().merge_full_set(pn_address, params, signer).await.map(Into::into)
    }

    /// Compute PMP address deterministically from event_id + oracle names +
    /// token_type. Used for PMP address verification (§12.2 of
    /// dex_connect_protocol).
    pub async fn get_pmp_address(
        &self,
        event_id: String,
        names: Vec<String>,
        token_type: u32,
    ) -> AppResult<String> {
        let root_pn = ackinacki_kit::contracts::dex::root_pn::RootPn::new(
            self.inner.private_note().ctx_client(),
            dex_contract_params(ackinacki_kit::contracts::dex::root_pn::RootPn::DEFAULT_ADDRESS),
        );
        let result = root_pn
            .get_pmp_address(ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPmpAddress {
                event_id,
                names,
                token_type,
            })
            .await
            .map_err(|e| crate::errors::AppError::from(e).with_context("get_pmp_address"))?;
        Ok(result.pmp_address)
    }

    pub async fn get_private_note_details(
        &self,
        pn_address: &str,
    ) -> AppResult<PrivateNoteDetails> {
        self.inner.private_note().get_details(pn_address).await.map(Into::into)
    }

    pub async fn get_stakes(&self, pn_address: &str) -> AppResult<PrivateNoteStakes> {
        self.inner.private_note().get_stakes(pn_address).await.map(Into::into)
    }

    /// Paginated history of PN events (stakes, claims, transfers, etc.).
    /// Pass multiple addresses to merge history across notes.
    pub async fn get_notes_history(
        &self,
        pn_addresses: &[String],
        limit: u32,
        cursor: Option<String>,
    ) -> AppResult<crate::services::history::NotesHistoryPage> {
        self.inner.private_note().get_notes_history(pn_addresses, limit, cursor).await
    }

    /// Scan RootPN deploy events, find PNs matching derived public keys.
    /// `derived_pubkeys_dec` — decimal pubkey strings derived from seed phrase.
    pub async fn discover_my_notes(
        &self,
        derived_pubkeys_dec: &std::collections::HashSet<String>,
    ) -> AppResult<Vec<dto::private_note::DiscoveredNote>> {
        let notes = self.inner.private_note().discover_notes(derived_pubkeys_dec).await?;
        Ok(notes.into_iter().map(Into::into).collect())
    }

    /// Resolve PN addresses from deposit_identifier_hashes and return
    /// aggregated balance.
    pub async fn get_aggregated_balance(
        &self,
        deposit_identifier_hashes: &[String],
    ) -> AppResult<AggregatedBalance> {
        let pn_mod = self.inner.private_note();
        let mut notes = Vec::with_capacity(deposit_identifier_hashes.len());

        for dih in deposit_identifier_hashes {
            let address = pn_mod
                .get_private_note_address(
                    ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress {
                        deposit_identifier_hash: dih.clone(),
                    },
                )
                .await?;

            match pn_mod.get_details(&address).await {
                Ok(details) => notes.push((address, details.into())),
                Err(e) => {
                    eprintln!("skip PN {address} (dih={dih}): {e}");
                }
            }
        }

        Ok(AggregatedBalance::from_details(notes))
    }

    // ── PMP ──────────────────────────────────────────────────────

    pub async fn submit_set_timings(
        &self,
        pmp_address: &str,
        params: ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.pmp().submit_set_timings(pmp_address, params, signer).await.map(Into::into)
    }

    pub async fn submit_resolve(
        &self,
        pmp_address: &str,
        params: ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.pmp().submit_resolve(pmp_address, params, signer).await.map(Into::into)
    }

    pub async fn submit_cancel_event(
        &self,
        pmp_address: &str,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.pmp().submit_cancel_event(pmp_address, signer).await.map(Into::into)
    }

    pub async fn get_pmp_details(&self, pmp_address: &str) -> AppResult<PmpDetails> {
        self.inner.pmp().get_details(pmp_address).await.map(Into::into)
    }

    /// Resolve the OrderBook address for a PMP. The OrderBook is spawned by
    /// PMP on first `splitFullSet`/`mergeFullSet`/resolve after `stakeEnd`
    /// (`_ensureFrozen()`); before that, the returned address may not yet
    /// be Active on chain.
    pub async fn get_order_book_address(&self, pmp_address: &str) -> AppResult<String> {
        self.inner.pmp().get_order_book_address(pmp_address).await
    }

    /// PMP-side shutdown state. After `submitResolve`, PMP triggers OB
    /// shutdown; the OB drains its queue and reports back via
    /// `onOrderBookShutdownComplete`, which flips `order_book_done` to
    /// true. `claim()` is gated on this flag.
    pub async fn get_pmp_shutdown_state(
        &self,
        pmp_address: &str,
    ) -> AppResult<PmpShutdownState> {
        self.inner.pmp().get_shutdown_state(pmp_address).await.map(Into::into)
    }

    // ── OrderBook ────────────────────────────────────────────────

    pub async fn get_order_book_details(
        &self,
        ob_address: &str,
    ) -> AppResult<OrderBookDetails> {
        self.inner.order_book().get_details(ob_address).await.map(Into::into)
    }

    pub async fn get_order_book_queue_size(&self, ob_address: &str) -> AppResult<u8> {
        self.inner.order_book().get_queue_size(ob_address).await.map(|r| r.size)
    }

    pub async fn get_order(&self, ob_address: &str, order_id: u128) -> AppResult<OrderInfo> {
        self.inner
            .order_book()
            .get_order(
                ob_address,
                ackinacki_kit::contracts::dex::order_book::ParamsOfGetOrder { order_id },
            )
            .await
            .map(Into::into)
    }

    pub async fn get_orders_by_owner(
        &self,
        ob_address: &str,
        deposit_identifier_hash: String,
    ) -> AppResult<OwnedOrders> {
        let raw = self
            .inner
            .order_book()
            .get_orders_by_owner(
                ob_address,
                ackinacki_kit::contracts::dex::order_book::ParamsOfGetOrdersByOwner {
                    deposit_hash: deposit_identifier_hash,
                },
            )
            .await?;
        OwnedOrders::try_from(raw)
    }

    /// Resolve the chain-assigned `order_id` for a given `client_order_id`
    /// under `deposit_identifier_hash`. The on-chain `getOrderIdByClient`
    /// getter was removed in the new OrderBook contract; we now derive it
    /// client-side by scanning `getOrdersByOwner` (which exposes parallel
    /// `order_ids` + `client_order_ids` arrays).
    ///
    /// Returns `Ok(None)` if no live order with that `client_order_id`
    /// exists (cancelled, fully filled, or never placed).
    pub async fn get_order_id_by_client(
        &self,
        ob_address: &str,
        deposit_identifier_hash: String,
        client_order_id: u128,
    ) -> AppResult<Option<u128>> {
        let owned = self
            .get_orders_by_owner(ob_address, deposit_identifier_hash)
            .await?;
        Ok(owned
            .orders
            .into_iter()
            .find(|o| o.client_order_id == client_order_id)
            .map(|o| o.order_id))
    }

    pub async fn get_order_book_shutdown_state(
        &self,
        ob_address: &str,
    ) -> AppResult<OrderBookShutdownState> {
        self.inner.order_book().get_shutdown_state(ob_address).await.map(Into::into)
    }

    /// Wait for an `OrderPlaced` ext-out event from `ob_address` whose
    /// payload's `client_order_id` matches. `min_created_at` filters out
    /// stale historical events (the chain retains all OrderPlaced events
    /// forever — without this filter, a coid reused after cancel/fill
    /// races against events from previous test runs).
    ///
    /// Pass `0` to disable the filter (e.g. one-shot scripts that
    /// guarantee fresh coids).
    pub async fn wait_for_order_placed(
        &self,
        ob_address: &str,
        client_order_id: u128,
        min_created_at: u64,
        timeout: std::time::Duration,
    ) -> AppResult<ackinacki_kit::contracts::dex::order_book_events::OrderPlacedData> {
        crate::services::order_book_event::wait_for_order_placed_by_client_id(
            self.inner.private_note().ctx_client(),
            ob_address,
            client_order_id,
            min_created_at,
            timeout,
        )
        .await
        .map_err(Into::into)
    }

    /// Wait for an `OrderCancelled` event for the given `client_order_id`.
    /// See [`Dex::wait_for_order_placed`] for why `min_created_at` exists.
    pub async fn wait_for_order_cancelled(
        &self,
        ob_address: &str,
        client_order_id: u128,
        min_created_at: u64,
        timeout: std::time::Duration,
    ) -> AppResult<ackinacki_kit::contracts::dex::order_book_events::OrderCancelledData> {
        crate::services::order_book_event::wait_for_order_cancelled_by_client_id(
            self.inner.private_note().ctx_client(),
            ob_address,
            client_order_id,
            min_created_at,
            timeout,
        )
        .await
        .map_err(Into::into)
    }

    /// Wait for the FIRST `OrderFilled` event for the given chain-assigned
    /// `order_id`. A single order can yield multiple fills (partial); use
    /// `services::order_book_event::fetch_extout_events_for_kind` directly
    /// to collect them all.
    pub async fn wait_for_order_filled(
        &self,
        ob_address: &str,
        order_id: u128,
        min_created_at: u64,
        timeout: std::time::Duration,
    ) -> AppResult<ackinacki_kit::contracts::dex::order_book_events::OrderFilledData> {
        crate::services::order_book_event::wait_for_order_filled_by_order_id(
            self.inner.private_note().ctx_client(),
            ob_address,
            order_id,
            min_created_at,
            timeout,
        )
        .await
        .map_err(Into::into)
    }

    // ── Oracle ───────────────────────────────────────────────────

    pub async fn deploy_oracle(
        &self,
        params: ParamsOfDeployOracle,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.oracle().deploy_oracle(params, signer).await.map(Into::into)
    }

    pub async fn get_oracle_address(&self, name: String) -> AppResult<String> {
        self.inner.oracle().get_oracle_address(name).await
    }

    pub async fn deploy_event_list(
        &self,
        oracle_address: &str,
        params: ackinacki_kit::contracts::dex::oracle::ParamsOfDeployEventList,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.oracle().deploy_event_list(oracle_address, params, signer).await.map(Into::into)
    }

    pub async fn get_event_list_address(
        &self,
        oracle_address: &str,
        params: ParamsOfGetEventListAddress,
    ) -> AppResult<String> {
        self.inner.oracle().get_event_list_address(oracle_address, params).await
    }

    pub async fn withdraw_fees(
        &self,
        oracle_address: &str,
        params: ParamsOfWithdrawFees,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.oracle().withdraw_fees(oracle_address, params, signer).await.map(Into::into)
    }

    pub async fn add_event(
        &self,
        event_list_address: &str,
        params: ParamsOfAddEvent,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.oracle().add_event(event_list_address, params, signer).await.map(Into::into)
    }

    pub async fn delete_event(
        &self,
        event_list_address: &str,
        params: ParamsOfDeleteEvent,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.oracle().delete_event(event_list_address, params, signer).await.map(Into::into)
    }

    pub async fn confirm_event(
        &self,
        event_list_address: &str,
        params: ParamsOfConfirmOrCancelEvent,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.oracle().confirm_event(event_list_address, params, signer).await.map(Into::into)
    }

    pub async fn cancel_event(
        &self,
        event_list_address: &str,
        params: ParamsOfConfirmOrCancelEvent,
        signer: Signer,
    ) -> AppResult<ResultOfBlockchainWrite> {
        self.inner.oracle().cancel_event(event_list_address, params, signer).await.map(Into::into)
    }

    pub async fn get_events(&self, event_list_address: &str) -> AppResult<OracleEvents> {
        self.inner.oracle().get_events(event_list_address).await.map(Into::into)
    }

    // ── Market discovery ──────────────────────────────────────────

    /// Returns all oracles deployed via RootOracle.
    pub async fn discover_oracles(&self) -> AppResult<Vec<dto::market::OracleInfo>> {
        self.inner
            .market()
            .discover_oracles()
            .await
            .map(|v| v.into_iter().map(Into::into).collect())
    }

    /// Returns all markets (PMPs) for a given token_type.
    pub async fn discover_markets(
        &self,
        token_type: u32,
    ) -> AppResult<Vec<dto::market::MarketInfo>> {
        self.inner
            .market()
            .discover_markets(token_type)
            .await
            .map(|v| v.into_iter().map(Into::into).collect())
    }

    /// Returns only active markets (approved, not cancelled, not resolved).
    pub async fn discover_active_markets(
        &self,
        token_type: u32,
    ) -> AppResult<Vec<dto::market::MarketInfo>> {
        self.inner
            .market()
            .discover_active_markets(token_type)
            .await
            .map(|v| v.into_iter().map(Into::into).collect())
    }

    /// Returns parsed, typed events from an OracleEventList.
    pub async fn get_parsed_events(
        &self,
        event_list_address: &str,
    ) -> AppResult<Vec<dto::oracle::EventInfo>> {
        let raw = self.get_events(event_list_address).await?;
        Ok(raw.parse())
    }
}
