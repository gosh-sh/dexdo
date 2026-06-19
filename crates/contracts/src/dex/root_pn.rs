use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::contracts::account::Account;
use ackinacki_kit::contracts::deserialize::deserialize_u128;
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

const ABI: &str = include_str!("../../../../contracts/dex/RootPN.abi.json");

#[derive(Debug, Clone)]
/// Wrapper for the DEX `RootPN` contract.
pub struct RootPn {
    base: ContractBase,
}

impl ModuleAccessor for RootPn {
    const MODULE: KitModule = KitModule::External("dex.root_pn");
}

impl HasContractBase for RootPn {
    fn base(&self) -> &ContractBase {
        &self.base
    }
}

impl AutoContract for RootPn {}

impl AsyncGuarded<Account> for RootPn {
    async fn async_guarded<F, T>(&self, action: F) -> T
    where
        F: FnOnce(&Account) -> T,
    {
        let guard = self.account().lock().await;
        action(&guard)
    }
}

impl AsyncGuardedMut<Account> for RootPn {
    async fn async_guarded_mut<F, Fut, T, E>(&self, action: F) -> Result<T, E>
    where
        F: FnOnce(OwnedMutexGuard<Account>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let guard = self.account().clone().lock_owned().await;
        action(guard).await
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `RootPN.sendEccShellToPrivateNote`.
pub struct ParamsOfSendEccShellToPrivateNote {
    pub proof: String,
    pub nullifier_hash: String,
    pub deposit_identifier_hash: String,
    pub final_layer_historical_hash_root: String,
    pub voucher_nominal_fr: String,
    pub token_type_fr: String,
    pub value: u64,
    pub layer_number: u8,
    pub recipient_ephemeral_pubkey: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `RootPN.deployPrivateNote`.
pub struct ParamsOfDeployPrivateNote {
    pub zkproof: String,
    pub deposit_identifier_hash: String,
    pub final_layer_historical_hash_root: String,
    pub voucher_nominal_fr: String,
    pub token_type_fr: String,
    pub ephemeral_pubkey: String,
    pub value: u64,
    pub token_type: u32,
    pub layer_number: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `RootPN.getPrivateNoteAddress`.
pub struct ParamsOfGetPrivateNoteAddress {
    pub deposit_identifier_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootPN.getPrivateNoteAddress`.
pub struct ResultOfGetPrivateNoteAddress {
    #[serde(rename = "privateNoteAddress")]
    pub private_note_address: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootPN.getPrivateNoteCode`.
pub struct ResultOfGetPrivateNoteCode {
    #[serde(rename = "privateNoteCode")]
    pub private_note_code: String,
    #[serde(rename = "privateNoteHash")]
    pub private_note_hash: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `RootPN.getPMPAddress`.
pub struct ParamsOfGetPmpAddress {
    pub event_id: String,
    pub names: Vec<String>,
    pub token_type: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `RootPN.privateNoteDeployed`.
pub struct ParamsOfPrivateNoteDeployed {
    pub deposit_identifier_hash: String,
    pub token_type: u32,
    pub deployed_value: u128,
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for `RootPN.generateVoucher`.
pub struct ParamsOfGenerateVoucher {
    #[serde(rename(serialize = "skUCommit"))]
    pub sk_u_commit: String,
    #[serde(rename(serialize = "isFee"))]
    pub is_fee: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `RootPN.withdrawTokens`.
///
/// `amounts` maps `tokenType → value` for every balance withdrawn in this
/// single call. `dapp_id` drives no logic here — it is only surfaced in the
/// emitted `TokensWithdrawn` event.
pub struct ParamsOfWithdrawTokens {
    pub amounts: HashMap<u32, u128>,
    pub wallet_addr: String,
    pub initial_data_hash: String,
    /// `uint256`, decimal or hex string.
    #[serde(rename = "dapp_id")]
    pub dapp_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for owner-only `RootPN.collectProtocolFee`.
pub struct ParamsOfCollectProtocolFee {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub amount: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for owner-only `RootPN.withdrawProtocolFees`.
pub struct ParamsOfWithdrawProtocolFees {
    pub to: String,
    /// `uint256`, decimal or hex string.
    #[serde(rename = "dapp_id")]
    pub dapp_id: String,
    pub token_type: u32,
    pub amount: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// Parameters for `RootPN.getProtocolFee`.
pub struct ParamsOfGetProtocolFee {
    pub token_type: u32,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootPN.getProtocolFee`.
pub struct ResultOfGetProtocolFee {
    #[serde(rename = "value0", deserialize_with = "deserialize_u128")]
    pub value: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootPN.getPMPAddress`.
pub struct ResultOfGetPmpAddress {
    #[serde(rename = "pmpAddress")]
    pub pmp_address: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Result of `RootPN.getDetails`.
pub struct ResultOfGetDetails {
    #[serde(rename = "pmpCodeHash")]
    pub pmp_code_hash: String,
    #[serde(rename = "privateNoteCodeHash")]
    pub private_note_code_hash: String,
    #[serde(rename = "ownerPubkey")]
    pub owner_pubkey: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub balance: u128,
}

#[derive(Debug, Clone, Serialize)]
/// Parameters for owner-only `RootPN.updateCode`.
pub struct ParamsOfUpdateCode {
    #[serde(rename(serialize = "newcode"))]
    pub new_code: String,
    pub cell: String,
}

impl RootPn {
    /// Premine RootPN address from `dex/modifiers/modifiers.sol`.
    pub const DEFAULT_ADDRESS: &'static str =
        "0:1010101010101010101010101010101010101010101010101010101010101010";

    /// Allows passing the root address explicitly (useful for shellnet/testnet
    /// or local networks where RootPN may live at a non-premine address).
    pub fn new(
        context: Arc<ClientContext>,
        params: impl Into<ackinacki_kit::contracts::account::ParamsOfNewContract>,
    ) -> Self {
        let params = params.into();
        Self { base: ContractBase::new(context, params, Abi::Json(ABI.to_string())) }
    }

    /// # Send ECC shell to a deterministic PrivateNote via ZK proof
    ///
    /// Original contract method: `sendEccShellToPrivateNote`
    ///
    /// Open method, can be called by any external sender (valid ZK proof required)
    pub async fn send_ecc_shell_to_private_note(
        &self,
        params: ParamsOfSendEccShellToPrivateNote,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "sendEccShellToPrivateNote".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Deploy PrivateNote
    ///
    /// Original contract method: `deployPrivateNote`
    ///
    /// Open method, can be called by any external sender (valid ZK proof required)
    pub async fn deploy_private_note(
        &self,
        params: ParamsOfDeployPrivateNote,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "deployPrivateNote".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Process callback when PrivateNote deployment is acknowledged
    ///
    /// Original contract method: `privateNoteDeployed`
    pub async fn private_note_deployed(
        &self,
        params: ParamsOfPrivateNoteDeployed,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "privateNoteDeployed".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Get salted PrivateNote code and hash
    ///
    /// Original contract method: `getPrivateNoteCode`
    pub async fn get_private_note_code(&self) -> KitResult<ResultOfGetPrivateNoteCode> {
        self.call_get_method::<ResultOfGetPrivateNoteCode>("getPrivateNoteCode").await
    }

    /// # Get deterministic PrivateNote address
    ///
    /// Original contract method: `getPrivateNoteAddress`
    pub async fn get_private_note_address(
        &self,
        params: ParamsOfGetPrivateNoteAddress,
    ) -> KitResult<ResultOfGetPrivateNoteAddress> {
        self.call_get_method_with::<ResultOfGetPrivateNoteAddress, ParamsOfGetPrivateNoteAddress>(
            "getPrivateNoteAddress",
            params,
        )
        .await
    }

    /// # Get deterministic PMP address
    ///
    /// Original contract method: `getPMPAddress`
    pub async fn get_pmp_address(
        &self,
        params: ParamsOfGetPmpAddress,
    ) -> KitResult<ResultOfGetPmpAddress> {
        self.call_get_method_with::<ResultOfGetPmpAddress, ParamsOfGetPmpAddress>(
            "getPMPAddress",
            params,
        )
        .await
    }

    /// # Get root details
    ///
    /// Original contract method: `getDetails`
    pub async fn get_details(&self) -> KitResult<ResultOfGetDetails> {
        self.call_get_method::<ResultOfGetDetails>("getDetails").await
    }

    /// # Generate voucher in RootPN vault
    ///
    /// Original contract method: `generateVoucher`
    pub async fn generate_voucher(
        &self,
        params: ParamsOfGenerateVoucher,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "generateVoucher".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Withdraw tokens from RootPN vault
    ///
    /// Original contract method: `withdrawTokens`
    pub async fn withdraw_tokens(
        &self,
        params: ParamsOfWithdrawTokens,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "withdrawTokens".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Collect protocol fee into the RootPN vault
    ///
    /// Original contract method: `collectProtocolFee`
    ///
    /// Owner-only; accrues the fee under `_protocolFees[tokenType]`.
    pub async fn collect_protocol_fee(
        &self,
        params: ParamsOfCollectProtocolFee,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "collectProtocolFee".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Withdraw accrued protocol fees
    ///
    /// Original contract method: `withdrawProtocolFees`
    ///
    /// Owner-only; pays out `amount` of accrued `tokenType` fees to `to`.
    pub async fn withdraw_protocol_fees(
        &self,
        params: ParamsOfWithdrawProtocolFees,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "withdrawProtocolFees".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }

    /// # Get accrued protocol fee for a token type
    ///
    /// Original contract method: `getProtocolFee`
    pub async fn get_protocol_fee(
        &self,
        params: ParamsOfGetProtocolFee,
    ) -> KitResult<ResultOfGetProtocolFee> {
        self.call_get_method_with::<ResultOfGetProtocolFee, ParamsOfGetProtocolFee>(
            "getProtocolFee",
            params,
        )
        .await
    }

    /// # Update root code
    ///
    /// Original contract method: `updateCode`
    ///
    /// Should be signed with root owner keys
    pub async fn update_code(
        &self,
        params: ParamsOfUpdateCode,
        signer: Signer,
    ) -> KitResult<ResultOfSendMessage> {
        let call_set = CallSet {
            function_name: "updateCode".to_string(),
            header: None,
            input: Some(json!(params)),
        };
        self.send_message(Some(call_set), None, signer).await
    }
}
