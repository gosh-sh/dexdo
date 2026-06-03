use ackinacki_kit::contracts::dex::oracle::Oracle;
use ackinacki_kit::contracts::dex::oracle::ParamsOfDeployEventList;
use ackinacki_kit::contracts::dex::oracle::ParamsOfGetEventListAddress;
use ackinacki_kit::contracts::dex::oracle::ParamsOfWithdrawFees;
use ackinacki_kit::contracts::dex::oracle_event_list::OracleEventList;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfAddEvent;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfConfirmOrCancelEvent;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfDeleteEvent;
use ackinacki_kit::contracts::dex::oracle_event_list::ResultOfGetEvents;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfDeployOracle;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfGetOracleAddress;
use ackinacki_kit::contracts::dex::root_oracle::RootOracle;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;

use crate::client::DexContext;
use crate::dex_contract_params;
use crate::errors::AppResult;
use crate::services;

pub(crate) struct OracleModule<'a> {
    ctx: &'a DexContext,
}

impl<'a> OracleModule<'a> {
    pub fn new(ctx: &'a DexContext) -> Self {
        Self { ctx }
    }

    fn root_oracle(&self) -> RootOracle {
        RootOracle::new(
            self.ctx.tvm_client.clone(),
            dex_contract_params(RootOracle::DEFAULT_ADDRESS),
        )
    }

    fn oracle(&self, address: &str) -> Oracle {
        Oracle::new(self.ctx.tvm_client.clone(), dex_contract_params(address))
    }

    fn event_list(&self, address: &str) -> OracleEventList {
        OracleEventList::new(self.ctx.tvm_client.clone(), dex_contract_params(address))
    }

    // --- RootOracle ---

    pub async fn deploy_oracle(
        &self,
        params: ParamsOfDeployOracle,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::oracle::deploy_oracle(&self.root_oracle(), params, signer).await
    }

    pub async fn get_oracle_address(&self, name: String) -> AppResult<String> {
        self.ctx.acquire().await;
        let result = services::oracle::get_oracle_address(
            &self.root_oracle(),
            ParamsOfGetOracleAddress { name },
        )
        .await?;
        Ok(result.oracle_address)
    }

    // --- Oracle ---

    pub async fn deploy_event_list(
        &self,
        oracle_address: &str,
        params: ParamsOfDeployEventList,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::oracle::deploy_event_list(&self.oracle(oracle_address), params, signer).await
    }

    pub async fn get_event_list_address(
        &self,
        oracle_address: &str,
        params: ParamsOfGetEventListAddress,
    ) -> AppResult<String> {
        self.ctx.acquire().await;
        let result =
            services::oracle::get_event_list_address(&self.oracle(oracle_address), params).await?;
        Ok(result.address)
    }

    pub async fn withdraw_fees(
        &self,
        oracle_address: &str,
        params: ParamsOfWithdrawFees,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::oracle::withdraw_fees(&self.oracle(oracle_address), params, signer).await
    }

    // --- EventList ---

    pub async fn add_event(
        &self,
        event_list_address: &str,
        params: ParamsOfAddEvent,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::oracle::add_event(&self.event_list(event_list_address), params, signer).await
    }

    pub async fn delete_event(
        &self,
        event_list_address: &str,
        params: ParamsOfDeleteEvent,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::oracle::delete_event(&self.event_list(event_list_address), params, signer).await
    }

    pub async fn confirm_event(
        &self,
        event_list_address: &str,
        params: ParamsOfConfirmOrCancelEvent,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::oracle::confirm_event(&self.event_list(event_list_address), params, signer).await
    }

    pub async fn cancel_event(
        &self,
        event_list_address: &str,
        params: ParamsOfConfirmOrCancelEvent,
        signer: Signer,
    ) -> AppResult<ResultOfSendMessage> {
        self.ctx.acquire().await;
        services::oracle::cancel_event(&self.event_list(event_list_address), params, signer).await
    }

    pub async fn get_events(&self, event_list_address: &str) -> AppResult<ResultOfGetEvents> {
        self.ctx.acquire().await;
        services::oracle::get_events(&self.event_list(event_list_address)).await
    }
}
