use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelAllOrders;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelBatch;
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
use ackinacki_kit::contracts::dex::private_note::PrivateNote;
use ackinacki_kit::contracts::dex::private_note::ResultOfGetDetails;
use ackinacki_kit::contracts::dex::private_note::ResultOfGetStakes;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGenerateVoucher;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use ackinacki_kit::contracts::dex::root_pn::ResultOfGetPrivateNoteAddress;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;

use crate::errors::AppResult;

pub(crate) async fn generate_voucher(
    root_pn: &RootPn,
    params: ParamsOfGenerateVoucher,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    root_pn.generate_voucher(params, signer).await.map_err(Into::into)
}

pub(crate) async fn deploy_private_note(
    root_pn: &RootPn,
    params: ParamsOfDeployPrivateNote,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    root_pn.deploy_private_note(params, signer).await.map_err(Into::into)
}

pub(crate) async fn send_ecc_shell(
    root_pn: &RootPn,
    params: ParamsOfSendEccShellToPrivateNote,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    root_pn.send_ecc_shell_to_private_note(params, signer).await.map_err(Into::into)
}

pub(crate) async fn get_private_note_address(
    root_pn: &RootPn,
    params: ParamsOfGetPrivateNoteAddress,
) -> AppResult<ResultOfGetPrivateNoteAddress> {
    root_pn.get_private_note_address(params).await.map_err(Into::into)
}

pub(crate) async fn deploy_pmp(
    pn: &PrivateNote,
    params: ParamsOfDeployPmp,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.deploy_pmp(params, signer).await.map_err(Into::into)
}

pub(crate) async fn set_stake(
    pn: &PrivateNote,
    params: ParamsOfSetStake,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.set_stake(params, signer).await.map_err(Into::into)
}

pub(crate) async fn claim(
    pn: &PrivateNote,
    params: ParamsOfStakeKey,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.claim(params, signer).await.map_err(Into::into)
}

pub(crate) async fn cancel_stake(
    pn: &PrivateNote,
    params: ParamsOfStakeKey,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.cancel_stake(params, signer).await.map_err(Into::into)
}

pub(crate) async fn delete_stake(
    pn: &PrivateNote,
    params: ParamsOfStakeKey,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.delete_stake(params, signer).await.map_err(Into::into)
}

pub(crate) async fn change_owner(
    pn: &PrivateNote,
    params: ParamsOfChangeOwner,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.change_owner(params, signer).await.map_err(Into::into)
}

pub(crate) async fn init_transfer(
    pn: &PrivateNote,
    params: ParamsOfInitTransfer,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.init_transfer(params, signer).await.map_err(Into::into)
}

pub(crate) async fn withdraw_tokens(
    pn: &PrivateNote,
    params: ParamsOfWithdrawTokens,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.withdraw_tokens(params, signer).await.map_err(Into::into)
}

pub(crate) async fn generate_coupon(
    pn: &PrivateNote,
    params: ParamsOfGenerateCoupon,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.generate_coupon(params, signer).await.map_err(Into::into)
}

pub(crate) async fn discard_coupon(
    pn: &PrivateNote,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.discard_coupon(signer).await.map_err(Into::into)
}

pub(crate) async fn place_order(
    pn: &PrivateNote,
    params: ParamsOfPlaceOrder,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.place_order(params, signer).await.map_err(Into::into)
}

pub(crate) async fn place_batch(
    pn: &PrivateNote,
    params: ParamsOfPlaceBatch,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.place_batch(params, signer).await.map_err(Into::into)
}

pub(crate) async fn cancel_order(
    pn: &PrivateNote,
    params: ParamsOfCancelOrder,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.cancel_order(params, signer).await.map_err(Into::into)
}

pub(crate) async fn cancel_order_by_client(
    pn: &PrivateNote,
    params: ParamsOfCancelOrderByClient,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.cancel_order_by_client(params, signer).await.map_err(Into::into)
}

pub(crate) async fn cancel_batch(
    pn: &PrivateNote,
    params: ParamsOfCancelBatch,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.cancel_batch(params, signer).await.map_err(Into::into)
}

pub(crate) async fn cancel_all_orders(
    pn: &PrivateNote,
    params: ParamsOfCancelAllOrders,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.cancel_all_orders(params, signer).await.map_err(Into::into)
}

pub(crate) async fn split_full_set(
    pn: &PrivateNote,
    params: ParamsOfSplitFullSet,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.split_full_set(params, signer).await.map_err(Into::into)
}

pub(crate) async fn merge_full_set(
    pn: &PrivateNote,
    params: ParamsOfMergeFullSet,
    signer: Signer,
) -> AppResult<ResultOfSendMessage> {
    pn.merge_full_set(params, signer).await.map_err(Into::into)
}

pub(crate) async fn get_details(pn: &PrivateNote) -> AppResult<ResultOfGetDetails> {
    pn.get_details().await.map_err(Into::into)
}

pub(crate) async fn get_stakes(pn: &PrivateNote) -> AppResult<ResultOfGetStakes> {
    pn.get_stakes().await.map_err(Into::into)
}
