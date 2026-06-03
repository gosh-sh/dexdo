use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::pmp::Pmp;
use ackinacki_kit::contracts::dex::pmp::ResultOfGetDetails;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;

use crate::client::DexContext;
use crate::dex_contract_params;
use crate::errors::AppResult;
use crate::services;

pub(crate) struct PmpModule<'a> {
    ctx: &'a DexContext,
}

impl<'a> PmpModule<'a> {
    pub fn new(ctx: &'a DexContext) -> Self {
        Self { ctx }
    }

    fn pmp(&self, address: &str) -> Pmp {
        Pmp::new(self.ctx.tvm_client.clone(), dex_contract_params(address))
    }

    pub async fn submit_set_timings(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitSetTimings,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::pmp::submit_set_timings(&self.pmp(pmp_address), params, signer).await
    }

    pub async fn submit_resolve(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitResolve,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::pmp::submit_resolve(&self.pmp(pmp_address), params, signer).await
    }

    pub async fn submit_cancel_event(
        &self,
        pmp_address: &str,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::pmp::submit_cancel_event(&self.pmp(pmp_address), signer).await
    }

    pub async fn get_details(&self, pmp_address: &str) -> AppResult<ResultOfGetDetails> {
        self.ctx.acquire().await;
        services::pmp::get_details(&self.pmp(pmp_address)).await
    }

    pub async fn get_order_book_address(&self, pmp_address: &str) -> AppResult<String> {
        self.ctx.acquire().await;
        let result = services::pmp::get_order_book_address(&self.pmp(pmp_address)).await?;
        Ok(result.order_book_address)
    }

    pub async fn get_shutdown_state(
        &self,
        pmp_address: &str,
    ) -> AppResult<ackinacki_kit::contracts::dex::pmp::ResultOfGetShutdownState> {
        self.ctx.acquire().await;
        services::pmp::get_shutdown_state(&self.pmp(pmp_address)).await
    }
}
