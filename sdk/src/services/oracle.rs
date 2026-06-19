use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use dodex_contracts::dex::oracle::Oracle;
use dodex_contracts::dex::oracle::ParamsOfDeployEventList;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle::ParamsOfWithdrawFees;
use dodex_contracts::dex::oracle::ResultOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::OracleEventList;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::oracle_event_list::ParamsOfConfirmOrCancelEvent;
use dodex_contracts::dex::oracle_event_list::ParamsOfDeleteEvent;
use dodex_contracts::dex::oracle_event_list::ResultOfGetEvents;
use dodex_contracts::dex::root_oracle::ParamsOfDeployOracle;
use dodex_contracts::dex::root_oracle::ParamsOfGetOracleAddress;
use dodex_contracts::dex::root_oracle::ResultOfGetOracleAddress;
use dodex_contracts::dex::root_oracle::RootOracle;

use crate::errors::AppResult;

// --- RootOracle ---

pub(crate) async fn deploy_oracle(
    root: &RootOracle,
    params: ParamsOfDeployOracle,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    root.deploy_oracle(params, signer).await.map_err(Into::into)
}

pub(crate) async fn get_oracle_address(
    root: &RootOracle,
    params: ParamsOfGetOracleAddress,
) -> AppResult<ResultOfGetOracleAddress> {
    root.get_oracle_address(params).await.map_err(Into::into)
}

// --- Oracle ---

pub(crate) async fn deploy_event_list(
    oracle: &Oracle,
    params: ParamsOfDeployEventList,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    oracle.deploy_event_list(params, signer).await.map_err(Into::into)
}

pub(crate) async fn get_event_list_address(
    oracle: &Oracle,
    params: ParamsOfGetEventListAddress,
) -> AppResult<ResultOfGetEventListAddress> {
    oracle.get_event_list_address(params).await.map_err(Into::into)
}

pub(crate) async fn withdraw_fees(
    oracle: &Oracle,
    params: ParamsOfWithdrawFees,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    oracle.withdraw_fees(params, signer).await.map_err(Into::into)
}

// --- OracleEventList ---

pub(crate) async fn add_event(
    event_list: &OracleEventList,
    params: ParamsOfAddEvent,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    event_list.add_event(params, signer).await.map_err(Into::into)
}

pub(crate) async fn delete_event(
    event_list: &OracleEventList,
    params: ParamsOfDeleteEvent,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    event_list.delete_event(params, signer).await.map_err(Into::into)
}

pub(crate) async fn confirm_event(
    event_list: &OracleEventList,
    params: ParamsOfConfirmOrCancelEvent,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    event_list.confirm_event(params, signer).await.map_err(Into::into)
}

pub(crate) async fn cancel_event(
    event_list: &OracleEventList,
    params: ParamsOfConfirmOrCancelEvent,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    event_list.cancel_event(params, signer).await.map_err(Into::into)
}

pub(crate) async fn get_events(event_list: &OracleEventList) -> AppResult<ResultOfGetEvents> {
    event_list.get_events().await.map_err(Into::into)
}
