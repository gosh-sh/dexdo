//! ABI-grounded checks for the AI Registry wrappers.
//!
//! These run offline (no node) and guard the bug surface of hand-written
//! wrappers: that every `Params*` struct serializes to exactly the ABI input
//! names, that every `Result*` struct deserializes the ABI output shape, and
//! that each event enum's numeric id matches `modifiers/modifiers.sol`. They
//! are the unit-level counterpart of the `test_airegistry` registration /
//! order-book / streaming flows — the on-chain flows themselves live in the
//! node-gated e2e suite.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

const SUPER_ROOT_ABI: &str = include_str!("../../../../contracts/airegistry/SuperRoot.abi.json");
const ROOT_MODEL_ABI: &str = include_str!("../../../../contracts/airegistry/RootModel.abi.json");
const TOKEN_CONTRACT_ABI: &str =
    include_str!("../../../../contracts/airegistry/TokenContract.abi.json");
const ORACLE_EVENT_LIST_ABI: &str =
    include_str!("../../../../contracts/dex/OracleEventList.abi.json");
const INFERENCE_ORDER_BOOK_ABI: &str =
    include_str!("../../../../contracts/airegistry/InferenceOrderBook.abi.json");

const SAMPLE_ADDRESS: &str = "0:0000000000000000000000000000000000000000000000000000000000000001";

fn abi_function(abi: &str, func: &str) -> Value {
    let v: Value = serde_json::from_str(abi).expect("parse ABI");
    v["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .find(|f| f["name"].as_str() == Some(func))
        .unwrap_or_else(|| panic!("function `{func}` not in ABI"))
        .clone()
}

fn abi_input_names(abi: &str, func: &str) -> BTreeSet<String> {
    abi_function(abi, func)["inputs"]
        .as_array()
        .expect("inputs array")
        .iter()
        .map(|i| i["name"].as_str().expect("input name").to_string())
        .collect()
}

fn serialized_keys<T: Serialize>(params: &T) -> BTreeSet<String> {
    serde_json::to_value(params)
        .expect("serialize params")
        .as_object()
        .expect("params is an object")
        .keys()
        .cloned()
        .collect()
}

/// A value shaped the way `tvm_client` returns an output of `ty` — every
/// integer arrives as a decimal string, addresses/bytes/strings as strings,
/// bools as JSON bools.
fn sample_for_type(ty: &str) -> Value {
    if let Some(inner) = ty.strip_suffix("[]") {
        return Value::Array(vec![sample_for_type(inner)]);
    }
    match ty {
        "bool" => Value::Bool(true),
        "address" => Value::String(SAMPLE_ADDRESS.to_string()),
        "cell" => Value::String("te6cc".to_string()),
        _ if ty.starts_with("uint") || ty.starts_with("int") || ty.starts_with("varuint") => {
            Value::String("1".to_string())
        }
        // bytes / bytesN / string
        _ => Value::String("00".to_string()),
    }
}

/// Build an output object for `func` straight from the ABI declaration, so the
/// keys and types match exactly what the node would return.
fn sample_output_json(abi: &str, func: &str) -> Value {
    let mut obj = Map::new();
    for o in abi_function(abi, func)["outputs"].as_array().expect("outputs array") {
        let name = o["name"].as_str().expect("output name");
        let ty = o["type"].as_str().expect("output type");
        obj.insert(name.to_string(), sample_for_type(ty));
    }
    Value::Object(obj)
}

fn abi_event(abi: &str, event: &str) -> Value {
    let v: Value = serde_json::from_str(abi).expect("parse ABI");
    v["events"]
        .as_array()
        .expect("events array")
        .iter()
        .find(|e| e["name"].as_str() == Some(event))
        .unwrap_or_else(|| panic!("event `{event}` not in ABI"))
        .clone()
}

/// Build an event payload object for `event` straight from the ABI inputs, so a
/// decoder struct is checked against the exact field names the node emits.
fn sample_event_json(abi: &str, event: &str) -> Value {
    let mut obj = Map::new();
    for i in abi_event(abi, event)["inputs"].as_array().expect("inputs array") {
        let name = i["name"].as_str().expect("input name");
        let ty = i["type"].as_str().expect("input type");
        obj.insert(name.to_string(), sample_for_type(ty));
    }
    Value::Object(obj)
}

// ── SuperRoot ────────────────────────────────────────────────────────────

#[test]
fn super_root_params_match_abi() {
    use super::super_root::ParamsOfDeployRootModel;
    use super::super_root::ParamsOfGetRootModelAddress;
    use super::super_root::ParamsOfSetPubkey;

    assert_eq!(
        serialized_keys(&ParamsOfSetPubkey { pubkey: "1".into() }),
        abi_input_names(SUPER_ROOT_ABI, "setPubkey")
    );
    assert_eq!(
        serialized_keys(&ParamsOfGetRootModelAddress { owner_pubkey: "1".into() }),
        abi_input_names(SUPER_ROOT_ABI, "getRootModelAddress")
    );
    assert_eq!(
        serialized_keys(&ParamsOfDeployRootModel { owner_pubkey: "1".into() }),
        abi_input_names(SUPER_ROOT_ABI, "deployRootModel")
    );
}

#[test]
fn super_root_results_decode_abi_shape() {
    use super::super_root::ResultOfGetAddress;
    use super::super_root::ResultOfGetOwnerPubkey;
    use super::super_root::ResultOfGetVersion;

    let addr: ResultOfGetAddress =
        serde_json::from_value(sample_output_json(SUPER_ROOT_ABI, "getRootModelAddress")).unwrap();
    assert_eq!(addr.address, SAMPLE_ADDRESS);

    let owner: ResultOfGetOwnerPubkey =
        serde_json::from_value(sample_output_json(SUPER_ROOT_ABI, "getOwnerPubkey")).unwrap();
    assert_eq!(owner.owner_pubkey, "1");

    let version: ResultOfGetVersion =
        serde_json::from_value(sample_output_json(SUPER_ROOT_ABI, "getVersion")).unwrap();
    assert_eq!(version.version, "00");
    assert_eq!(version.name, "00");
}

// ── RootModel ──────────────────────────────────────────────────────────────

#[test]
fn root_model_params_match_abi() {
    use super::root_model::ParamsOfGetTokenContractAddress;
    use super::root_model::ParamsOfRegisterTokenContract;

    assert_eq!(
        serialized_keys(&ParamsOfRegisterTokenContract { seller_pubkey: "1".into(), nonce: 1 }),
        abi_input_names(ROOT_MODEL_ABI, "registerTokenContract")
    );
    assert_eq!(
        serialized_keys(&ParamsOfGetTokenContractAddress { seller_pubkey: "1".into(), nonce: 1 }),
        abi_input_names(ROOT_MODEL_ABI, "getTokenContractAddress")
    );
}

#[test]
fn root_model_results_decode_abi_shape() {
    use super::root_model::ResultOfGetTokenContractAddress;

    let tc: ResultOfGetTokenContractAddress =
        serde_json::from_value(sample_output_json(ROOT_MODEL_ABI, "getTokenContractAddress"))
            .unwrap();
    assert_eq!(tc.address, SAMPLE_ADDRESS);
}

// ── TokenContract ────────────────────────────────────────────────────────

#[test]
fn token_contract_params_match_abi() {
    use super::token_contract::ParamsOfFundFromOrderBook;
    use super::token_contract::ParamsOfOpen;
    use super::token_contract::ParamsOfWithdrawShell;

    assert_eq!(
        serialized_keys(&ParamsOfOpen { endpoint_cipher: "00".into() }),
        abi_input_names(TOKEN_CONTRACT_ABI, "open")
    );
    assert_eq!(
        serialized_keys(&ParamsOfWithdrawShell { amount: 1 }),
        abi_input_names(TOKEN_CONTRACT_ABI, "withdrawShell")
    );
    assert_eq!(
        serialized_keys(&ParamsOfFundFromOrderBook {
            paid: 1,
            buyer_note: SAMPLE_ADDRESS.into(),
            buyer_pubkey: "1".into(),
            deal_flags: 0,
        }),
        abi_input_names(TOKEN_CONTRACT_ABI, "fundFromOrderBook")
    );
    // `close` is argument-less now that the payout address is derived rather
    // than passed. Pin the emptiness: if it grows inputs again, `close` needs a
    // Params struct and this fails instead of silently sending `{}`.
    assert!(
        abi_input_names(TOKEN_CONTRACT_ABI, "close").is_empty(),
        "TokenContract.close gained inputs — give it a Params struct"
    );
}

#[test]
fn token_contract_results_decode_abi_shape() {
    use super::token_contract::ResultOfGetConfig;
    use super::token_contract::ResultOfGetDeal;
    use super::token_contract::ResultOfGetFees;
    use super::token_contract::ResultOfGetParties;
    use super::token_contract::ResultOfGetSeller;
    use super::token_contract::ResultOfGetSellerBond;
    use super::token_contract::ResultOfGetShellBalance;
    use super::token_contract::ResultOfGetState;

    let state: ResultOfGetState =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getState")).unwrap();
    assert!(state.funded);
    assert_eq!(state.deposit, 1);
    assert_eq!(state.dispute_time, 1);
    assert_eq!(state.funded_time, 1);

    let bond: ResultOfGetSellerBond =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getSellerBond")).unwrap();
    assert!(bond.bond_funded);
    assert_eq!(bond.bond_held, 1);
    assert_eq!(bond.bond_required, 1);

    let config: ResultOfGetConfig =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getConfig")).unwrap();
    assert_eq!(config.platform_fee_bps, 1);
    assert_eq!(config.dispute_window, 1);

    let fees: ResultOfGetFees =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getFees")).unwrap();
    assert_eq!(fees.fee_accrued, 1);
    assert_eq!(fees.rebate_slope_bps, 1);

    let deal: ResultOfGetDeal =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getDeal")).unwrap();
    assert_eq!(deal.tick_size, 1);

    let parties: ResultOfGetParties =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getParties")).unwrap();
    assert_eq!(parties.seller_note, SAMPLE_ADDRESS);

    let seller: ResultOfGetSeller =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getSeller")).unwrap();
    assert_eq!(seller.root_model_address, SAMPLE_ADDRESS);
    assert_eq!(seller.nonce, 1);

    let shell: ResultOfGetShellBalance =
        serde_json::from_value(sample_output_json(TOKEN_CONTRACT_ABI, "getShellBalance")).unwrap();
    assert_eq!(shell.value, 1);
}

// ── InferenceOrderBook ─────────────────────────────────────────────────────

#[test]
fn inference_order_book_params_match_abi() {
    use super::inference_order_book::ParamsOfGetOrder;
    use super::inference_order_book::ParamsOfPlaceBuyOrder;
    use super::inference_order_book::ParamsOfPlaceSellOffer;

    assert_eq!(
        serialized_keys(&ParamsOfPlaceSellOffer {
            price_per_tick: 1,
            max_ticks: 1,
            flags: 0,
            seller_pubkey: "1".into(),
            nonce: 1,
            owner_note: SAMPLE_ADDRESS.into(),
            deadline: 1,
        }),
        abi_input_names(INFERENCE_ORDER_BOOK_ABI, "placeSellOffer")
    );
    assert_eq!(
        serialized_keys(&ParamsOfPlaceBuyOrder {
            client_order_id: 1,
            escrow: 1,
            max_price_per_tick: 1,
            ticks: 1,
            flags: 0,
            deadline: 0,
            buyer_pubkey: "1".into(),
            deposit_hash: "1".into(),
        }),
        abi_input_names(INFERENCE_ORDER_BOOK_ABI, "placeBuyOrder")
    );
    // The getter keys on `id`, not `orderId`.
    assert_eq!(
        serialized_keys(&ParamsOfGetOrder { id: 1 }),
        abi_input_names(INFERENCE_ORDER_BOOK_ABI, "getOrder")
    );
}

#[test]
fn inference_order_book_results_decode_abi_shape() {
    use super::inference_order_book::ResultOfGetBestBidAsk;
    use super::inference_order_book::ResultOfGetOrder;
    use super::inference_order_book::ResultOfGetParams;
    use super::inference_order_book::ResultOfGetQueueSize;
    use super::inference_order_book::ResultOfGetStats;

    let order: ResultOfGetOrder =
        serde_json::from_value(sample_output_json(INFERENCE_ORDER_BOOK_ABI, "getOrder")).unwrap();
    assert_eq!(order.note, SAMPLE_ADDRESS);
    assert_eq!(order.amount, 1);
    assert!(order.is_buy);

    let stats: ResultOfGetStats =
        serde_json::from_value(sample_output_json(INFERENCE_ORDER_BOOK_ABI, "getStats")).unwrap();
    assert_eq!(stats.next_order_id, 1);
    assert_eq!(stats.executed_ticks, 1);

    let queue: ResultOfGetQueueSize =
        serde_json::from_value(sample_output_json(INFERENCE_ORDER_BOOK_ABI, "getQueueSize"))
            .unwrap();
    assert_eq!(queue.size, 1);

    let bba: ResultOfGetBestBidAsk =
        serde_json::from_value(sample_output_json(INFERENCE_ORDER_BOOK_ABI, "getBestBidAsk"))
            .unwrap();
    assert!(bba.has_bid);
    assert_eq!(bba.bid, "1");

    let params: ResultOfGetParams =
        serde_json::from_value(sample_output_json(INFERENCE_ORDER_BOOK_ABI, "getParams")).unwrap();
    assert_eq!(params.platform_fee_bps, 1);
}

// ── Event ids (mirror modifiers/modifiers.sol) ─────────────────────────────

#[test]
fn event_ids_match_modifiers() {
    use super::inference_order_book_events::InferenceOrderBookEvent as Iob;
    use super::root_model_events::RootModelEvent as Rm;
    use super::super_root_events::SuperRootEvent as Sr;
    use super::token_contract_events::TokenContractEvent as Tc;

    assert_eq!(Sr::RootRegistered as u128, 700);

    assert_eq!(Rm::TokenContractRegistered as u128, 702);
    assert_eq!(Rm::ContractDeployed as u128, 703);

    // A relic. The id assertions below duplicate
    // `typed_event_enums_carry_the_ids_the_contracts_emit_on`
    // (`crates/infrastructure/tests/airegistry_event_manifest.rs`), which derives the
    // same numbers from `modifiers.sol` instead of repeating them here. New variants
    // are NOT added here: this table drifting away from the contract is exactly what
    // defect 703 was — the deal deploy moved to `DealDeployedEmit`, the manifest
    // recorded it, and the enum and this line did not.
    assert_eq!(Tc::ContractDeployed as u128, 732);
    assert_eq!(Tc::ContractDestroyed as u128, 709);
    assert_eq!(Tc::ShellWithdrawn as u128, 710);
    assert_eq!(Tc::StreamFunded as u128, 720);
    assert_eq!(Tc::StreamOpened as u128, 721);
    assert_eq!(Tc::TickFinalized as u128, 722);
    assert_eq!(Tc::StreamStopped as u128, 723);
    assert_eq!(Tc::StreamDisputed as u128, 724);
    assert_eq!(Tc::DisputeResolved as u128, 725);
    assert_eq!(Tc::SellerBondFunded as u128, 727);
    assert_eq!(Tc::ProbeAccepted as u128, 728);
    assert_eq!(Tc::ProbeBurned as u128, 729);

    assert_eq!(Iob::OrderPlaced as u128, 1000);
    assert_eq!(Iob::OrderCancelled as u128, 1001);
    assert_eq!(Iob::Refunded as u128, 1002);
    assert_eq!(Iob::Filled as u128, 1003);
    assert_eq!(Iob::Executed as u128, 1004);
    // 1006/1007 are the retired forfeit ids — reserved in modifiers.sol, no
    // matching ABI event, so deliberately absent from the enum.
    assert_eq!(Iob::InferenceOrderBookDeployed as u128, 1008);
    assert_eq!(Iob::OrderCancelRejected as u128, 1009);
}

#[test]
fn event_address_roundtrips_through_try_from() {
    use super::inference_order_book_events::InferenceOrderBookEvent as Iob;
    use super::token_contract_events::TokenContractEvent as Tc;

    // `to_address()` renders `0:<64-hex>`; `TryFrom` parses the hex back. The
    // matching engine emits to exactly these directed external addresses.
    assert_eq!(
        Tc::StreamFunded.to_address(),
        "0:00000000000000000000000000000000000000000000000000000000000002d0"
    );
    assert_eq!(Tc::try_from(Tc::ProbeAccepted.to_string()).unwrap(), Tc::ProbeAccepted);
    assert_eq!(Iob::try_from(Iob::Filled.to_string()).unwrap(), Iob::Filled);
    // The same round trip `every_declared_variant_round_trips_through_try_from` does
    // over `ALL`: without an arm in `TryFrom` the variant is declared but the decoder
    // does not know it.
    use crate::dex::oracle_event_list_events::OracleEventListEvent as Oel;
    assert_eq!(
        Oel::try_from(Oel::RangeEventAdded.to_string()).unwrap(),
        Oel::RangeEventAdded,
        "RangeEventAdded is a declared variant, but TryFrom does not know its id (162)"
    );
    assert!(Iob::try_from(
        ":0000000000000000000000000000000000000000000000000000000000000063".to_string()
    )
    .is_err());
}

// ── Event payload decoders (field names vs ABI event inputs) ───────────────

/// The names of every ABI event.
fn abi_event_names(abi: &str) -> BTreeSet<String> {
    let v: Value = serde_json::from_str(abi).expect("abi json");
    let events = v["events"].as_array().expect("abi.events array");
    // An empty array would make any claim about it true always — that is how a guard
    // goes blind when the key is renamed.
    assert!(!events.is_empty(), "events is empty — the ABI key changed");
    events.iter().map(|e| e["name"].as_str().expect("event name").to_string()).collect()
}

#[test]
fn inference_order_book_enum_covers_every_abi_event() {
    use super::inference_order_book_events::InferenceOrderBookEvent as E;
    let declared: BTreeSet<String> = E::ALL
        .iter()
        .map(|v| {
            let n = format!("{v:?}");
            if n.starts_with("Inference") {
                n
            } else {
                format!("Inference{n}")
            }
        })
        .collect();
    let in_abi = abi_event_names(INFERENCE_ORDER_BOOK_ABI);
    assert_eq!(
        declared,
        in_abi,
        "extra in enum: {:?}; missing: {:?}",
        declared.difference(&in_abi).collect::<Vec<_>>(),
        in_abi.difference(&declared).collect::<Vec<_>>()
    );
}

#[test]
fn token_contract_enum_covers_every_abi_event() {
    use super::token_contract_events::TokenContractEvent as E;
    let declared: BTreeSet<String> = E::ALL.iter().map(|v| format!("{v:?}")).collect();
    let in_abi = abi_event_names(TOKEN_CONTRACT_ABI);
    assert_eq!(
        declared,
        in_abi,
        "extra in enum: {:?}; missing: {:?}",
        declared.difference(&in_abi).collect::<Vec<_>>(),
        in_abi.difference(&declared).collect::<Vec<_>>()
    );
}

#[test]
fn event_payloads_decode_abi_shape() {
    use super::inference_order_book_events as iob;
    use super::root_model_events as rm;
    use super::super_root_events as sr;
    use super::token_contract_events as tc;

    macro_rules! decodes {
        ($ty:ty, $abi:expr, $name:expr) => {
            serde_json::from_value::<$ty>(sample_event_json($abi, $name))
                .unwrap_or_else(|e| panic!("decode event `{}`: {e}", $name))
        };
    }

    // InferenceOrderBook (1000-range). Events carry an `Inference` prefix since
    // v4.0.10. `InferenceFilled.sellerTC` is the field whose camelCase would
    // mis-derive to `sellerTc`, so assert it explicitly.
    decodes!(iob::OrderPlacedData, INFERENCE_ORDER_BOOK_ABI, "InferenceOrderPlaced");
    decodes!(iob::OrderCancelledData, INFERENCE_ORDER_BOOK_ABI, "InferenceOrderCancelled");
    let filled = decodes!(iob::FilledData, INFERENCE_ORDER_BOOK_ABI, "InferenceFilled");
    assert_eq!(filled.seller_tc.as_deref(), Some(SAMPLE_ADDRESS));
    // `sellerNote` is pinned for a different reason than its camelCase neighbour: it
    // is the field v4.0.33 added so the deal link survives orphan repair, where no
    // SELL leg exists to walk. Nothing else guards its presence in the ABI, so if it
    // were dropped the projector would silently fall back to the leg walk — which is
    // exactly the path that has no leg — and the seller would go missing only on the
    // orphan path, where no test looks for him.
    assert_eq!(filled.seller_note.as_deref(), Some(SAMPLE_ADDRESS));
    decodes!(iob::ExecutedData, INFERENCE_ORDER_BOOK_ABI, "InferenceExecuted");
    decodes!(iob::RefundedData, INFERENCE_ORDER_BOOK_ABI, "InferenceRefunded");
    decodes!(iob::OrderBookDeployedData, INFERENCE_ORDER_BOOK_ABI, "InferenceOrderBookDeployed");
    decodes!(
        iob::OrderCancelRejectedData,
        INFERENCE_ORDER_BOOK_ABI,
        "InferenceOrderCancelRejected"
    );
    decodes!(iob::OrderExpiredData, INFERENCE_ORDER_BOOK_ABI, "InferenceOrderExpired");
    decodes!(iob::OrderRejectedData, INFERENCE_ORDER_BOOK_ABI, "InferenceOrderRejected");
    decodes!(tc::TicksClaimedData, TOKEN_CONTRACT_ABI, "TicksClaimed");
    decodes!(tc::EndpointSetData, TOKEN_CONTRACT_ABI, "EndpointSet");
    decodes!(tc::BuyerBondFundedData, TOKEN_CONTRACT_ABI, "BuyerBondFunded");

    // The aliases above go through `use super::… as …`, where `super` is
    // `airegistry`. A foreign module needs the full path from the crate root.
    use crate::dex::oracle_event_list_events as oel;
    decodes!(oel::RangeEventAddedData, ORACLE_EVENT_LIST_ABI, "RangeEventAdded");

    // TokenContract (700-range streaming).
    decodes!(tc::StreamFundedData, TOKEN_CONTRACT_ABI, "StreamFunded");
    decodes!(tc::StreamOpenedData, TOKEN_CONTRACT_ABI, "StreamOpened");
    decodes!(tc::SellerBondFundedData, TOKEN_CONTRACT_ABI, "SellerBondFunded");
    decodes!(tc::ProbeAcceptedData, TOKEN_CONTRACT_ABI, "ProbeAccepted");
    decodes!(tc::ProbeBurnedData, TOKEN_CONTRACT_ABI, "ProbeBurned");
    decodes!(tc::TickFinalizedData, TOKEN_CONTRACT_ABI, "TickFinalized");
    decodes!(tc::StreamStoppedData, TOKEN_CONTRACT_ABI, "StreamStopped");
    decodes!(tc::DisputeResolvedData, TOKEN_CONTRACT_ABI, "DisputeResolved");
    decodes!(tc::ShellWithdrawnData, TOKEN_CONTRACT_ABI, "ShellWithdrawn");
    decodes!(tc::ContractDeployedData, TOKEN_CONTRACT_ABI, "ContractDeployed");

    // Registry.
    decodes!(sr::RootRegisteredData, SUPER_ROOT_ABI, "RootRegistered");
    decodes!(rm::TokenContractRegisteredData, ROOT_MODEL_ABI, "TokenContractRegistered");
}
