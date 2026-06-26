// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashMap;
use std::io::Cursor;

use anyhow::anyhow;
use anyhow::Context;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use serde::Serialize;
use serde_json::Value;
use tracing::warn;
use tvm_abi::token::Detokenizer;
use tvm_abi::Contract;
use tvm_abi::Function;
use tvm_types::read_single_root_boc;
use tvm_types::SliceData;

const ABI_ROOT_ORACLE: &str = include_str!("../../../contracts/dex/RootOracle.abi.json");
const ABI_ORACLE: &str = include_str!("../../../contracts/dex/Oracle.abi.json");
const ABI_ORACLE_EVENT_LIST: &str = include_str!("../../../contracts/dex/OracleEventList.abi.json");
const ABI_PMP: &str = include_str!("../../../contracts/dex/PMP.abi.json");
const ABI_ORDER_BOOK: &str = include_str!("../../../contracts/dex/OrderBook.abi.json");
const ABI_ROOT_PN: &str = include_str!("../../../contracts/dex/RootPN.abi.json");
const ABI_PRIVATE_NOTE: &str = include_str!("../../../contracts/dex/PrivateNote.abi.json");
const ABI_NULLIFIER: &str = include_str!("../../../contracts/dex/Nullifier.abi.json");

const ABI_INFERENCE_ORDER_BOOK: &str =
    include_str!("../../../contracts/airegistry/InferenceOrderBook.abi.json");

const DEX_ABIS: &[(&str, &str)] = &[
    ("RootOracle", ABI_ROOT_ORACLE),
    ("Oracle", ABI_ORACLE),
    ("OracleEventList", ABI_ORACLE_EVENT_LIST),
    ("PMP", ABI_PMP),
    ("OrderBook", ABI_ORDER_BOOK),
    ("RootPN", ABI_ROOT_PN),
    ("PrivateNote", ABI_PRIVATE_NOTE),
    ("Nullifier", ABI_NULLIFIER),
];

const INFERENCE_ABIS: &[(&str, &str)] = &[("InferenceOrderBook", ABI_INFERENCE_ORDER_BOOK)];

#[derive(Debug, Clone, Serialize)]
pub struct DecodedEvent {
    pub contract_kind: &'static str,
    pub event_name: String,
    pub event_type: String,
    pub value: Value,
}

/// A route entry: when a message's `dst` matches this key, use this
/// (kind, event) pair, and validate that the body's event_id matches
/// `expected_id`. Only needed for colliding event ids.
#[derive(Clone)]
struct Route {
    kind: &'static str,
    event: String,
    expected_id: u32,
}

#[derive(Clone)]
pub struct Decoder {
    contracts: HashMap<&'static str, Contract>,
    /// id -> every (kind, event_name) that hashes to it. len > 1 means a collision.
    event_index: HashMap<u32, Vec<(&'static str, String)>>,
    /// gateway-encoded dst string -> the (kind, event, expected_id) it routes to.
    routes: HashMap<String, Route>,
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("contracts", &self.contracts.keys().collect::<Vec<_>>())
            .field("known_events", &self.event_index.len())
            .finish()
    }
}

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut contracts = HashMap::new();
        let mut event_index: HashMap<u32, Vec<(&'static str, String)>> = HashMap::new();

        for &(kind, abi_json) in DEX_ABIS.iter().chain(INFERENCE_ABIS.iter()) {
            let contract = Contract::load(Cursor::new(abi_json))
                .with_context(|| format!("load abi for {kind}"))?;

            for (name, event) in contract.events() {
                event_index.entry(event.get_id()).or_default().push((kind, name.clone()));
            }

            contracts.insert(kind, contract);
        }

        // Route table: only the colliding OrderCancelled pair needs disambiguation.
        // Non-colliding events resolve by unique id. Add more dsts here if a future
        // ABI introduces another collision (R3). `expected_id` is looked up from the
        // loaded contract, never hardcoded.
        let mut routes = HashMap::new();
        Self::add_route(&mut routes, &contracts, 144, "OrderBook", "OrderCancelled")?;
        Self::add_route(&mut routes, &contracts, 1001, "InferenceOrderBook", "OrderCancelled")?;

        Ok(Self { contracts, event_index, routes })
    }

    fn add_route(
        routes: &mut HashMap<String, Route>,
        contracts: &HashMap<&'static str, Contract>,
        emit_id: u32,
        kind: &'static str,
        event: &str,
    ) -> anyhow::Result<()> {
        let contract = contracts.get(kind).with_context(|| format!("route abi {kind}"))?;
        let expected_id = contract
            .events()
            .iter()
            .find(|(name, _)| name.as_str() == event)
            .map(|(_, e)| e.get_id())
            .with_context(|| format!("event {kind}.{event} not in abi"))?;
        routes.insert(
            crate::config::event_type_dst(emit_id),
            Route { kind, event: event.to_string(), expected_id },
        );
        Ok(())
    }

    pub fn known_events(&self) -> usize {
        self.event_index.len()
    }

    pub fn unique_event_ids(&self) -> usize {
        // Every id in the index is a distinct id (colliding ids are multiple
        // entries in the vec, not multiple keys). Count all keys.
        self.event_index.len()
    }

    /// Borrow the parsed `Contract` for a given dex kind (e.g. `"PMP"`).
    /// Used by the reconciler to drive off-chain getters.
    pub fn contract(&self, kind: &str) -> Option<&Contract> {
        self.contracts.get(kind)
    }

    pub fn decode_event_body(
        &self,
        body_base64: &str,
        dst: Option<&str>,
    ) -> anyhow::Result<Option<DecodedEvent>> {
        let bytes = BASE64_STANDARD.decode(body_base64).context("decode base64 body")?;
        let cell = read_single_root_boc(bytes).context("read boc")?;
        let slice = SliceData::load_cell(cell).map_err(|e| anyhow!("slice from cell: {e}"))?;

        let event_id = Function::decode_output_id(slice.clone())
            .map_err(|e| anyhow!("decode event id: {e}"))?;

        // (a) known dst route -> use it, validating the body id matches the route.
        if let Some(dst) = dst
            && let Some(route) = self.routes.get(dst)
        {
            if route.expected_id != event_id {
                warn!(
                    dst,
                    event_id,
                    expected = route.expected_id,
                    "dst route id mismatch; leaving undecoded"
                );
                return Ok(None);
            }
            return self.decode_with(route.kind, &route.event, slice).map(Some);
        }

        // (b) global id lookup — accept only if the id is unique.
        match self.event_index.get(&event_id).map(Vec::as_slice) {
            Some([(kind, event)]) => self.decode_with(kind, event, slice).map(Some),
            Some(multi) if multi.len() > 1 => {
                warn!(
                    event_id,
                    dst = ?dst,
                    candidates = ?multi,
                    "ambiguous event_id with no known dst route; leaving undecoded"
                );
                Ok(None) // (c) ambiguous — never silently first-ABI.
            }
            _ => Ok(None), // unknown id
        }
    }

    fn decode_with(
        &self,
        kind: &str,
        event: &str,
        slice: SliceData,
    ) -> anyhow::Result<DecodedEvent> {
        let contract = self.contracts.get(kind).expect("contract index consistent with abi list");

        let decoded = contract
            .decode_output(slice, true, true)
            .map_err(|e| anyhow!("decode_output for {kind}.{event}: {e}"))?;

        let value = Detokenizer::detokenize_to_json_value(&decoded.tokens)
            .map_err(|e| anyhow!("detokenize {kind}.{event}: {e}"))?;

        // Reuse the &'static str key from the contracts map for contract_kind.
        let contract_kind = self.contracts.keys().find(|k| **k == kind).copied().unwrap_or("");

        Ok(DecodedEvent {
            contract_kind,
            event_name: event.to_string(),
            event_type: format!("{kind}.{event}"),
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_all_abis_and_indexes_events() {
        let decoder = Decoder::new().unwrap();

        for &(kind, _) in DEX_ABIS {
            assert!(decoder.contracts.contains_key(kind), "missing contract {kind}");
        }

        // 13 PMP + 2 Oracle + 4 OracleEventList + 8 OrderBook + 1 RootOracle
        // + 6 RootPN + 14 PrivateNote + 0 Nullifier = 48 DEX events
        // + 8 InferenceOrderBook events, but OrderCancelled collides with
        // OrderBook.OrderCancelled -> still 48 + 7 = 55 unique ids, but one id
        // has 2 entries. known_events() counts distinct ids = 55.
        assert_eq!(decoder.known_events(), 55, "unexpected total event id count");

        // sample lookups — find entries for PMP
        let pmp_event_ids: Vec<_> = decoder
            .event_index
            .iter()
            .filter(|(_, entries)| entries.iter().any(|(kind, _)| *kind == "PMP"))
            .map(|(id, entries)| {
                let name =
                    entries.iter().find(|(k, _)| *k == "PMP").map(|(_, n)| n.clone()).unwrap();
                (*id, name)
            })
            .collect();
        assert!(pmp_event_ids.iter().any(|(_, n)| n == "Resolved"));
        assert!(pmp_event_ids.iter().any(|(_, n)| n == "ApprovedByOracle"));
    }

    #[test]
    fn registers_inference_orderbook_and_counts_unique_ids() {
        let decoder = Decoder::new().unwrap();
        assert!(decoder.contracts.contains_key("InferenceOrderBook"), "inference abi missing");
        // 48 DEX unique ids + 8 inference events, of which OrderCancelled collides
        // with OrderBook.OrderCancelled (same (uint128,uint128) signature) => +7 new ids.
        // Total distinct ids = 55 (48 + 7). The colliding id has 2 entries.
        assert_eq!(decoder.unique_event_ids(), 55, "unexpected unique event-id count");
    }

    #[test]
    fn order_cancelled_routes_by_dst() {
        let decoder = Decoder::new().unwrap();
        let body = inference_cancel_body_b64(&decoder);
        let ob_dst = crate::config::event_type_dst(144); // OB_ORDER_CANCELLED
        let inf_dst = crate::config::event_type_dst(1001); // InferenceOrderBook OrderCancelled

        let inf = decoder.decode_event_body(&body, Some(&inf_dst)).unwrap().unwrap();
        assert_eq!(inf.event_type, "InferenceOrderBook.OrderCancelled");

        let ob = decoder.decode_event_body(&body, Some(&ob_dst)).unwrap().unwrap();
        assert_eq!(ob.event_type, "OrderBook.OrderCancelled");

        // Unknown dst on a colliding id => ambiguous => left undecoded (warn), never first-ABI.
        let ambiguous = decoder.decode_event_body(&body, Some(":dead")).unwrap();
        assert!(ambiguous.is_none(), "colliding id with unknown dst must be left undecoded");
    }

    #[test]
    fn non_colliding_inference_event_resolves_by_id() {
        let decoder = Decoder::new().unwrap();
        // Filled has a unique id, so it resolves even with no dst route.
        let body = inference_filled_body_b64(&decoder);
        let ev = decoder.decode_event_body(&body, None).unwrap().unwrap();
        assert_eq!(ev.event_type, "InferenceOrderBook.Filled");
    }

    #[test]
    fn all_non_colliding_inference_events_resolve_uniquely_by_id() {
        // Every InferenceOrderBook event EXCEPT the colliding OrderCancelled
        // must map to a UNIQUE id resolving to InferenceOrderBook.
        let d = Decoder::new().unwrap();
        let inf = d.contracts.get("InferenceOrderBook").expect("inference abi loaded");
        let mut checked = 0;
        for (name, ev) in inf.events() {
            if name.as_str() == "OrderCancelled" {
                continue; // colliding — resolved by dst (separate test)
            }
            let entries = d.event_index.get(&ev.get_id()).expect("event id indexed");
            assert_eq!(
                entries.len(),
                1,
                "{name} unexpectedly collides (id maps to {} entries)",
                entries.len()
            );
            assert_eq!(entries[0].0, "InferenceOrderBook", "{name} resolves to the wrong contract");
            assert_eq!(entries[0].1.as_str(), name.as_str());
            checked += 1;
        }
        assert_eq!(
            checked,
            7,
            "expected exactly 7 non-colliding inference events (OrderPlaced, Filled, Executed, Refunded, SubscriptionPlaced, CycleForfeited, ForfeitClaimed)"
        );
    }

    #[test]
    fn unknown_event_id_returns_none() {
        let decoder = Decoder::new().unwrap();

        // body BOC observed in chain from src=0:111...111 (system msg).
        // Its event id is not in the dex ABIs, so we must return Ok(None).
        let body = "te6ccgEBAgEAOAABTiEI6kGAF2XiReIi2UGGm9TvRXJAgoqOxOYUQGiHnYczEQPg+AhhEAEAF6AAAAACMteYg9IABA==";
        let decoded = decoder.decode_event_body(body, None).unwrap();
        assert!(decoded.is_none());
    }

    #[test]
    fn invalid_base64_is_error() {
        let decoder = Decoder::new().unwrap();
        assert!(decoder.decode_event_body("not_a_boc!!!", None).is_err());
    }

    #[test]
    fn decodes_multicell_order_placed() {
        let decoder = Decoder::new().unwrap();

        // Real OrderBook.OrderPlaced body, captured from event message
        // 65d552e6cecf8ac725fbea4a24e8fd054e2ab11f31251e188523ded2fdc4456e on
        // shellnet — a historical reference that a shellnet redeploy may retire;
        // the base64 fixture is self-contained regardless. The field layout is
        // derived from the OrderPlaced event in OrderBook.abi.json.
        //
        // A TVM cell holds at most 1023 data bits. With the 32-bit event-id
        // prefix, the fields fill the first cell through depositHash at 969
        // bits; the 64-bit opNonce no longer fits and lands in a continuation
        // cell. opNonce is therefore the field that exercises the multi-cell
        // descent, where the prefix offset has to be carried into the next cell.
        let body = "te6ccgEBAgEAhwAB8xucaVcAAAAAAAAAAAAAAAAAAAACAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAJGgAAAAAAAAAAAAAAAn+OzoAAAAAAAAAAAFcScEalnJSVsVKAm0LrR0TbuPbU18Mkb7ENEBG22bNzhvrIubdt2wtAAQAQAAAAAAAAAXM=";

        let decoded = decoder.decode_event_body(body, None).unwrap().expect("event id is known");

        assert_eq!(decoded.event_type, "OrderBook.OrderPlaced");
        assert_eq!(decoded.value["orderId"], "2");
        assert_eq!(decoded.value["outcomeId"], "0");
        assert_eq!(decoded.value["isBuy"], true);
        // price, amount, clientOrderId and depositHash sit in the first cell,
        // which ends after depositHash at the 969-bit boundary.
        assert_eq!(
            decoded.value["price"],
            "0x0000000000000000000000000000000000000000000000000000000000001234"
        );
        assert_eq!(decoded.value["amount"], "21460000000");
        assert_eq!(decoded.value["clientOrderId"], "12548401359218092331");
        assert_eq!(
            decoded.value["depositHash"],
            "0x62a5013685d68e89b771eda9af8648df621a20236db366e70df591736edbb616"
        );
        // opNonce is the sole field in the continuation cell.
        assert_eq!(decoded.value["opNonce"], "371");
    }

    // --- Test helpers: encode event bodies from the loaded contracts ---

    fn encode_event_body_b64(
        decoder: &Decoder,
        kind: &str,
        event_name: &str,
        tokens_json: serde_json::Value,
    ) -> String {
        use tvm_abi::token::TokenValue;
        use tvm_abi::token::Tokenizer;
        use tvm_types::write_boc;
        use tvm_types::BuilderData;
        use tvm_types::IBitstring;

        let contract = decoder.contracts.get(kind).unwrap();
        let events = contract.events();
        let event = events.get(event_name).expect("event in abi");
        let params = event.input_params();
        let tokens = Tokenizer::tokenize_all_params(&params, &tokens_json).unwrap();

        // Event body format: 4-byte event_id + packed token values.
        let mut builder = BuilderData::new();
        builder.append_u32(event.get_id()).unwrap();
        let abi_version = tvm_abi::contract::ABI_VERSION_2_4;
        let data_builder =
            TokenValue::pack_values_into_chain(&tokens, vec![], &abi_version).unwrap();
        builder.append_builder(&data_builder).unwrap();
        let cell = builder.into_cell().unwrap();
        let bytes = write_boc(&cell).unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn inference_cancel_body_b64(d: &Decoder) -> String {
        encode_event_body_b64(
            d,
            "InferenceOrderBook",
            "OrderCancelled",
            serde_json::json!({ "orderId": "7", "refundedShell": "0" }),
        )
    }

    fn inference_filled_body_b64(d: &Decoder) -> String {
        encode_event_body_b64(
            d,
            "InferenceOrderBook",
            "Filled",
            serde_json::json!({
                "makerId": "1",
                "takerId": "2",
                "ticks": "3",
                "clearingPrice": "0",
                "sellerTC": "0:0000000000000000000000000000000000000000000000000000000000000000",
                "buyerNote": "0:0000000000000000000000000000000000000000000000000000000000000000"
            }),
        )
    }
}
