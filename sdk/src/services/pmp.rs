use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::pmp::Pmp;
use ackinacki_kit::contracts::dex::pmp::ResultOfGetDetails;
use ackinacki_kit::contracts::dex::pmp::ResultOfGetOrderBookAddress;
use ackinacki_kit::contracts::dex::pmp::ResultOfGetShutdownState;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;

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

pub(crate) async fn get_order_book_address(
    pmp: &Pmp,
) -> AppResult<ResultOfGetOrderBookAddress> {
    pmp.get_order_book_address().await.map_err(Into::into)
}

pub(crate) async fn get_shutdown_state(pmp: &Pmp) -> AppResult<ResultOfGetShutdownState> {
    pmp.get_shutdown_state().await.map_err(Into::into)
}
