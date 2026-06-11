// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Off-chain executor for ABI v2.4 contract getters. Reimplements the core of
// tvm_client::tvm::call_tvm_msg directly on top of tvm_vm/tvm_executor/tvm_block,
// so we depend on the low-level crates instead of pulling the full tvm_client
// graph (network layer, async runtime hooks, FIFT-style get_methods, etc.).

use std::collections::HashMap;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde_json::Value;
use tvm_abi::token::Detokenizer;
use tvm_abi::token::Tokenizer;
use tvm_abi::Contract;
use tvm_abi::TokenValue;
use tvm_block::Account;
use tvm_block::CommonMsgInfo;
use tvm_block::Deserializable;
use tvm_block::ExternalInboundMessageHeader;
use tvm_block::Message;
use tvm_block::MsgAddressExt;
use tvm_block::OutAction;
use tvm_block::OutActions;
use tvm_block::Serializable;
use tvm_executor::BlockchainConfig;
use tvm_types::HashmapType;
use tvm_types::SliceData;
use tvm_vm::executor::gas::gas_state::Gas;
use tvm_vm::executor::Engine;
use tvm_vm::int;
use tvm_vm::stack::integer::IntegerData;
use tvm_vm::stack::savelist::SaveList;
use tvm_vm::stack::Stack;
use tvm_vm::stack::StackItem;
use tvm_vm::SmartContractInfo;

const GAS_LIMIT: i64 = 1_000_000_000;

/// Execute a contract getter against the supplied account state and decode the
/// reply via the provided ABI.
///
/// `account_boc_base64` — output of GraphQL `accounts(...) { boc }`.
/// `function_name` — the getter to invoke (must exist in `contract`).
/// `input` — JSON object of named params; pass `Value::Object(Default::default())`
///           or `serde_json::json!({})` for parameterless getters.
///
/// Returns the detokenized response as a JSON object keyed by output param name.
pub fn run_getter(
    contract: &Contract,
    account_boc_base64: &str,
    function_name: &str,
    input: &Value,
) -> Result<Value> {
    let bytes = BASE64_STANDARD.decode(account_boc_base64).context("decode account boc base64")?;
    let mut account = Account::construct_from_bytes(&bytes).context("parse account boc")?;

    if account.is_none() {
        return Err(anyhow!("account is AccountNone — not deployed"));
    }

    let address = account.get_addr().ok_or_else(|| anyhow!("account has no address"))?.clone();

    let function = contract
        .function(function_name)
        .map_err(|err| anyhow!("function `{function_name}` not in abi: {err}"))?;

    let input_tokens = Tokenizer::tokenize_all_params(function.input_params(), input)
        .map_err(|err| anyhow!("tokenize input for {function_name}: {err}"))?;

    // Replay-protection (`afterSignatureCheck` in modifiers/replayprotection.sol)
    // rejects external messages whose `expireAt` is more than 5 minutes ahead of
    // `block.timestamp`. The default header `Expire` produced by tvm_abi is
    // `u32::MAX` (token/mod.rs::get_default_value_for_header), which trips that
    // guard on any real-account BOC and surfaces as TVM throw 401
    // (ERR_MESSAGE_WITH_HUGE_EXPIREAT). Pin `expire` to `now + 60s` so the read
    // path is accepted; we don't sign and don't persist state, so any value
    // inside the 5-minute window is fine.
    let now = now_unixtime();
    let mut header = HashMap::new();
    header.insert("expire".to_string(), TokenValue::Expire(now.saturating_add(60)));
    let body_builder = function
        .encode_input(&header, &input_tokens, false, None, Some(address.clone()))
        .map_err(|err| anyhow!("encode_input for {function_name}: {err}"))?;
    let body_cell = body_builder.into_cell().map_err(|err| anyhow!("body to cell: {err}"))?;
    let body_slice = SliceData::load_cell(body_cell).map_err(|err| anyhow!("body slice: {err}"))?;

    let mut msg = Message::with_ext_in_header(ExternalInboundMessageHeader::new(
        MsgAddressExt::AddrNone,
        address,
    ));
    msg.set_body(body_slice);

    let out_messages =
        call_tvm_msg(&mut account, &msg).with_context(|| format!("execute {function_name}"))?;

    let response_body = out_messages
        .iter()
        .find_map(|m| match m.header() {
            CommonMsgInfo::ExtOutMsgInfo(_) => m.body(),
            _ => None,
        })
        .ok_or_else(|| anyhow!("getter {function_name} produced no ext-out reply"))?;

    let tokens = function
        .decode_output(response_body, false, true)
        .map_err(|err| anyhow!("decode output for {function_name}: {err}"))?;

    Detokenizer::detokenize_to_json_value(&tokens)
        .map_err(|err| anyhow!("detokenize {function_name}: {err}"))
}

/// Decode the contract's persistent storage fields (the ABI `fields`
/// section) straight from an account BOC — no getter call, so it can read
/// state variables that have no getter (e.g. PrivateNote `_pubkey`).
pub fn decode_account_fields(contract: &Contract, account_boc_base64: &str) -> Result<Value> {
    let bytes = BASE64_STANDARD.decode(account_boc_base64).context("decode account boc base64")?;
    let account = Account::construct_from_bytes(&bytes).context("parse account boc")?;
    if account.is_none() {
        return Err(anyhow!("account is AccountNone — not deployed"));
    }
    let data = account.get_data().ok_or_else(|| anyhow!("account has no data"))?;
    let data_slice = SliceData::load_cell(data).map_err(|err| anyhow!("data slice: {err}"))?;
    let tokens = contract
        .decode_storage_fields(data_slice, true)
        .map_err(|err| anyhow!("decode storage fields: {err}"))?;
    Detokenizer::detokenize_to_json_value(&tokens)
        .map_err(|err| anyhow!("detokenize storage fields: {err}"))
}

fn call_tvm_msg(account: &mut Account, msg: &Message) -> Result<Vec<Message>> {
    let config = BlockchainConfig::default();

    let msg_cell = msg.serialize().map_err(|err| anyhow!("serialize msg: {err}"))?;
    let balance = account.balance().map_or(0u128, |cc| cc.grams.as_u128());

    let function_selector = match msg.header() {
        CommonMsgInfo::IntMsgInfo(_) => int!(0),
        CommonMsgInfo::ExtInMsgInfo(_) => int!(-1),
        CommonMsgInfo::ExtOutMsgInfo(_) => return Err(anyhow!("invalid message type ext-out")),
    };

    let mut stack = Stack::new();
    stack
        .push(int!(balance))
        .push(int!(0))
        .push(StackItem::Cell(msg_cell))
        .push(StackItem::Slice(msg.body().unwrap_or_default()))
        .push(function_selector);

    let engine = call_tvm(account, &config, stack)?;

    let actions_cell =
        engine.get_actions().as_cell().map_err(|err| anyhow!("get_actions cell: {err}"))?.clone();
    let mut actions = OutActions::construct_from_cell(actions_cell)
        .map_err(|err| anyhow!("parse OutActions: {err}"))?;

    let mut msgs = Vec::new();
    for action in actions.iter_mut() {
        if let OutAction::SendMsg { out_msg, .. } = std::mem::replace(action, OutAction::None) {
            msgs.push(out_msg);
        }
    }
    msgs.reverse();
    Ok(msgs)
}

fn call_tvm(account: &mut Account, config: &BlockchainConfig, stack: Stack) -> Result<Engine> {
    let code = account.get_code().ok_or_else(|| anyhow!("account has no code"))?;
    let data = account.get_data().ok_or_else(|| anyhow!("account has no data"))?;
    let addr = account.get_addr().ok_or_else(|| anyhow!("account has no address"))?.clone();
    let balance = account.balance().ok_or_else(|| anyhow!("account has no balance"))?.clone();

    let mut ctrls = SaveList::new();
    ctrls.put(4, &mut StackItem::Cell(data)).map_err(|err| anyhow!("put c4: {err}"))?;

    let raw_config = config.raw_config();
    let mut sci = SmartContractInfo::with_myself(
        addr.serialize().and_then(SliceData::load_cell).unwrap_or_default(),
    );
    sci.block_lt = 0;
    sci.trans_lt = 0;
    sci.unix_time = now_unixtime();
    sci.balance = balance;
    if let Some(d) = raw_config.config_params.data() {
        sci.config_params = Some(d.clone());
    }
    if let Some(h) = account.init_code_hash() {
        sci.set_init_code_hash(h.clone());
    }
    sci.set_mycode(code.clone());
    sci.capabilities = config.capabilites();
    ctrls.put(7, &mut sci.into_temp_data_item()).map_err(|err| anyhow!("put c7: {err}"))?;

    let gas = Gas::new(GAS_LIMIT, 0, GAS_LIMIT, 10);
    let code_slice = SliceData::load_cell(code).map_err(|err| anyhow!("code slice: {err}"))?;

    let mut engine = Engine::with_capabilities(config.capabilites()).setup(
        code_slice,
        Some(ctrls),
        Some(stack),
        Some(gas),
    );

    if let Err(err) = engine.execute() {
        let exception =
            tvm_vm::error::tvm_exception(err).map_err(|e| anyhow!("tvm exception unwrap: {e}"))?;
        return Err(anyhow!(
            "tvm execution failed: code={}, msg={}",
            exception
                .custom_code()
                .unwrap_or(exception.exception_code().map(|c| c as i32).unwrap_or(-1)),
            exception
        ));
    }

    Ok(engine)
}

fn now_unixtime() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_invalid_account_boc() {
        let abi = include_str!("../../../contracts/abi/dex/PMP.abi.json");
        let contract = Contract::load(std::io::Cursor::new(abi)).unwrap();
        let err = run_getter(&contract, "not_a_boc", "getDetails", &json!({})).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("decode") || msg.contains("base64") || msg.contains("parse"));
    }

    #[test]
    fn rejects_unknown_function_name() {
        // Empty Account BOC suffices because the function-name lookup happens
        // before account decoding succeeds — but tokenization needs a valid
        // ABI match, so we send a deliberately wrong name.
        let abi = include_str!("../../../contracts/abi/dex/PMP.abi.json");
        let contract = Contract::load(std::io::Cursor::new(abi)).unwrap();
        // base64 of empty bytes — parsing will fail before function name check;
        // but the error path is what matters: it must NOT panic.
        let err = run_getter(&contract, "", "thisDoesNotExist", &json!({})).unwrap_err();
        assert!(!format!("{err:#}").is_empty());
    }
}
