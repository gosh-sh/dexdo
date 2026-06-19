use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use dodex_contracts::dex::pmp::ParamsOfSubmitResolve;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::pmp::ResultOfGetDetails;
use dodex_contracts::dex::pmp::ResultOfGetOrderBookAddress;
use dodex_contracts::dex::pmp::ResultOfGetShutdownState;

use crate::errors::AppResult;

pub(crate) async fn submit_set_timings(
    pmp: &Pmp,
    params: ParamsOfSubmitSetTimings,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pmp.submit_set_timings(params, signer).await.map_err(Into::into)
}

pub(crate) async fn submit_resolve(
    pmp: &Pmp,
    params: ParamsOfSubmitResolve,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pmp.submit_resolve(params, signer).await.map_err(Into::into)
}

pub(crate) async fn submit_cancel_event(
    pmp: &Pmp,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pmp.submit_cancel_event(signer).await.map_err(Into::into)
}

pub(crate) async fn get_details(pmp: &Pmp) -> AppResult<ResultOfGetDetails> {
    pmp.get_details().await.map_err(Into::into)
}

pub(crate) async fn get_order_book_address(pmp: &Pmp) -> AppResult<ResultOfGetOrderBookAddress> {
    pmp.get_order_book_address().await.map_err(Into::into)
}

pub(crate) async fn get_shutdown_state(pmp: &Pmp) -> AppResult<ResultOfGetShutdownState> {
    pmp.get_shutdown_state().await.map_err(Into::into)
}
