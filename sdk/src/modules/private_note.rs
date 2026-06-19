use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use dodex_contracts::dex::private_note::ParamsOfCancelAllOrders;
use dodex_contracts::dex::private_note::ParamsOfCancelOrder;
use dodex_contracts::dex::private_note::ParamsOfCancelOrderByClient;
use dodex_contracts::dex::private_note::ParamsOfChangeOwner;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::private_note::ParamsOfGenerateCoupon;
use dodex_contracts::dex::private_note::ParamsOfInitTransfer;
use dodex_contracts::dex::private_note::ParamsOfMergeFullSet;
use dodex_contracts::dex::private_note::ParamsOfPlaceBatch;
use dodex_contracts::dex::private_note::ParamsOfPlaceOrder;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfSplitFullSet;
use dodex_contracts::dex::private_note::ParamsOfStakeKey;
use dodex_contracts::dex::private_note::ParamsOfWithdrawTokens;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_contracts::dex::private_note::ResultOfGetDetails;
use dodex_contracts::dex::private_note::ResultOfGetStakes;
use dodex_contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfGenerateVoucher;
use dodex_contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use dodex_contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use dodex_contracts::dex::root_pn::RootPn;

use crate::client::DexContext;
use crate::dex_contract_params;
use crate::errors::AppResult;
use crate::services;

pub(crate) struct PrivateNoteModule<'a> {
    ctx: &'a DexContext,
}

impl<'a> PrivateNoteModule<'a> {
    pub fn new(ctx: &'a DexContext) -> Self {
        Self { ctx }
    }

    pub fn ctx_client(&self) -> std::sync::Arc<ackinacki_kit::tvm_client::ClientContext> {
        self.ctx.tvm_client.clone()
    }

    fn root_pn(&self) -> RootPn {
        RootPn::new(self.ctx.tvm_client.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS))
    }

    fn pn(&self, address: &str) -> PrivateNote {
        PrivateNote::new(self.ctx.tvm_client.clone(), dex_contract_params(address))
    }

    // --- Voucher (entry point from multifactor wallet) ---

    pub async fn generate_voucher(
        &self,
        params: ParamsOfGenerateVoucher,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::generate_voucher(&self.root_pn(), params, signer).await
    }

    // --- Deploy flow ---

    pub async fn deploy_private_note(
        &self,
        params: ParamsOfDeployPrivateNote,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::deploy_private_note(&self.root_pn(), params, signer).await
    }

    pub async fn send_ecc_shell(
        &self,
        params: ParamsOfSendEccShellToPrivateNote,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::send_ecc_shell(&self.root_pn(), params, signer).await
    }

    pub async fn get_private_note_address(
        &self,
        params: ParamsOfGetPrivateNoteAddress,
    ) -> AppResult<String> {
        self.ctx.acquire().await;
        let result =
            services::private_note::get_private_note_address(&self.root_pn(), params).await?;
        Ok(result.private_note_address)
    }

    // --- Stake operations ---

    pub async fn deploy_pmp(
        &self,
        pn_address: &str,
        params: ParamsOfDeployPmp,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::deploy_pmp(&self.pn(pn_address), params, signer).await
    }

    pub async fn set_stake(
        &self,
        pn_address: &str,
        params: ParamsOfSetStake,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::set_stake(&self.pn(pn_address), params, signer).await
    }

    pub async fn claim(
        &self,
        pn_address: &str,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::claim(&self.pn(pn_address), params, signer).await
    }

    pub async fn cancel_stake(
        &self,
        pn_address: &str,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::cancel_stake(&self.pn(pn_address), params, signer).await
    }

    pub async fn delete_stake(
        &self,
        pn_address: &str,
        params: ParamsOfStakeKey,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::delete_stake(&self.pn(pn_address), params, signer).await
    }

    // --- Account management ---

    pub async fn change_owner(
        &self,
        pn_address: &str,
        params: ParamsOfChangeOwner,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::change_owner(&self.pn(pn_address), params, signer).await
    }

    pub async fn init_transfer(
        &self,
        pn_address: &str,
        params: ParamsOfInitTransfer,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::init_transfer(&self.pn(pn_address), params, signer).await
    }

    pub async fn withdraw_tokens(
        &self,
        pn_address: &str,
        params: ParamsOfWithdrawTokens,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::withdraw_tokens(&self.pn(pn_address), params, signer).await
    }

    pub async fn generate_coupon(
        &self,
        pn_address: &str,
        params: ParamsOfGenerateCoupon,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::generate_coupon(&self.pn(pn_address), params, signer).await
    }

    pub async fn discard_coupon(
        &self,
        pn_address: &str,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::discard_coupon(&self.pn(pn_address), signer).await
    }

    // --- OrderBook proxy operations (PrivateNote → OrderBook) ---

    pub async fn place_order(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceOrder,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::place_order(&self.pn(pn_address), params, signer).await
    }

    pub async fn place_batch(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceBatch,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::place_batch(&self.pn(pn_address), params, signer).await
    }

    pub async fn cancel_order(
        &self,
        pn_address: &str,
        params: ParamsOfCancelOrder,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::cancel_order(&self.pn(pn_address), params, signer).await
    }

    pub async fn cancel_order_by_client(
        &self,
        pn_address: &str,
        params: ParamsOfCancelOrderByClient,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::cancel_order_by_client(&self.pn(pn_address), params, signer).await
    }

    pub async fn cancel_all_orders(
        &self,
        pn_address: &str,
        params: ParamsOfCancelAllOrders,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::cancel_all_orders(&self.pn(pn_address), params, signer).await
    }

    // --- Split / merge full set (PMP collateral ↔ outcome tokens) ---

    pub async fn split_full_set(
        &self,
        pn_address: &str,
        params: ParamsOfSplitFullSet,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::split_full_set(&self.pn(pn_address), params, signer).await
    }

    pub async fn merge_full_set(
        &self,
        pn_address: &str,
        params: ParamsOfMergeFullSet,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::private_note::merge_full_set(&self.pn(pn_address), params, signer).await
    }

    // --- Getters ---

    pub async fn get_details(&self, pn_address: &str) -> AppResult<ResultOfGetDetails> {
        self.ctx.acquire().await;
        services::private_note::get_details(&self.pn(pn_address)).await
    }

    pub async fn get_stakes(&self, pn_address: &str) -> AppResult<ResultOfGetStakes> {
        self.ctx.acquire().await;
        services::private_note::get_stakes(&self.pn(pn_address)).await
    }

    // --- History ---

    pub async fn get_notes_history(
        &self,
        pn_addresses: &[String],
        limit: u32,
        cursor: Option<String>,
    ) -> AppResult<services::history::NotesHistoryPage> {
        self.ctx.acquire().await;
        services::history::get_notes_history(
            self.ctx.tvm_client.clone(),
            pn_addresses,
            limit,
            cursor,
        )
        .await
    }

    // --- Discovery ---

    pub async fn discover_notes(
        &self,
        derived_pubkeys_dec: &std::collections::HashSet<String>,
    ) -> AppResult<Vec<services::discovery::DiscoveredNote>> {
        self.ctx.acquire().await;
        services::discovery::discover_private_notes(
            self.ctx.tvm_client.clone(),
            derived_pubkeys_dec,
        )
        .await
    }
}
