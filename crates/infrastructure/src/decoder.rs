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
const ABI_TOKEN_CONTRACT: &str =
    include_str!("../../../contracts/airegistry/TokenContract.abi.json");
// Loaded NOT for projection — none of its events reaches the read model. It is
// loaded to resolve a collision: `RootModel.ContractDeployed(address self)` is
// byte-for-byte identical to `TokenContract.ContractDeployed`, so their body ids
// are identical too. While this ABI is not loaded the id looks unique, the decoder
// silently hands it to TokenContract, and deploying the root model creates a
// phantom `inference_deals` row under its address — with no signal to the operator
// at all: the ambiguous-id counter does not fire, because with a single loaded ABI
// there is no ambiguity.
const ABI_ROOT_MODEL: &str = include_str!("../../../contracts/airegistry/RootModel.abi.json");

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

const INFERENCE_ABIS: &[(&str, &str)] = &[
    ("InferenceOrderBook", ABI_INFERENCE_ORDER_BOOK),
    ("TokenContract", ABI_TOKEN_CONTRACT),
    ("RootModel", ABI_ROOT_MODEL),
];

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

/// Outcome of decoding one event body. Distinguishes the benign "this id is in
/// no loaded ABI" case from an ambiguous-collision config regression (a
/// colliding id with no dst route) so the latter is countable/alertable rather
/// than lost in unknown-id noise.
#[derive(Debug)]
pub enum DecodeOutcome {
    /// Body decoded against a resolved `(contract, event)`.
    Decoded(DecodedEvent),
    /// The event id is in no loaded ABI — normal (a contract emitted something
    /// the indexer does not index). Benign, uncounted.
    UnknownId,
    /// The event id maps to multiple ABIs and no dst route disambiguated it.
    /// Left undecoded (never silently first-ABI). A new colliding ABI was
    /// likely added without a dst route — a regression worth alerting on.
    AmbiguousCollision { event_id: u32 },
}

impl DecodeOutcome {
    /// The decoded event, or `None` for `UnknownId` / `AmbiguousCollision`.
    pub fn decoded(self) -> Option<DecodedEvent> {
        match self {
            DecodeOutcome::Decoded(e) => Some(e),
            _ => None,
        }
    }
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

        // Route table: dst-based disambiguation for colliding event ids. The two
        // OrderCancelled dsts are pinned defensively — the InferenceOrderBook events
        // carry an `Inference` prefix, so they no longer share an id with
        // OrderBook.OrderCancelled — while the two ContractDeployed dsts resolve a
        // REAL collision: RootModel and TokenContract declare byte-identical events,
        // so only `dst` tells a root-model deploy from a deal deploy. Both routes are
        // mandatory: with either missing, that side falls through to the id lookup,
        // which now reports AmbiguousCollision instead of guessing. `expected_id` is
        // looked up from the loaded contract, never hardcoded.
        let mut routes = HashMap::new();
        Self::add_route(&mut routes, &contracts, 144, "OrderBook", "OrderCancelled")?;
        Self::add_route(
            &mut routes,
            &contracts,
            1001,
            "InferenceOrderBook",
            "InferenceOrderCancelled",
        )?;
        // `ContractDeployedEmit` = 703 is RootModel; `DealDeployedEmit` = 732 is the deal.
        Self::add_route(&mut routes, &contracts, 703, "RootModel", "ContractDeployed")?;
        Self::add_route(&mut routes, &contracts, 732, "TokenContract", "ContractDeployed")?;

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
    ) -> anyhow::Result<DecodeOutcome> {
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
                return Ok(DecodeOutcome::UnknownId);
            }
            return self.decode_with(route.kind, &route.event, slice).map(DecodeOutcome::Decoded);
        }

        // (b) global id lookup — accept only if the id is unique. A colliding id
        // with no dst route is reported as AmbiguousCollision (never silently
        // first-ABI); the caller counts and noise-dedups the warning.
        match self.event_index.get(&event_id).map(Vec::as_slice) {
            Some([(kind, event)]) => {
                self.decode_with(kind, event, slice).map(DecodeOutcome::Decoded)
            }
            Some(multi) if multi.len() > 1 => Ok(DecodeOutcome::AmbiguousCollision { event_id }),
            _ => Ok(DecodeOutcome::UnknownId),
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

        // 57 DEX unique ids + 9 InferenceOrderBook ids + 15 TokenContract ids
        // + 1 RootModel id = 82. RootModel declares TWO events, but its
        // `ContractDeployed` is byte-for-byte identical to TokenContract's and
        // shares its id — only `TokenContractRegistered` brings a new one.
        // distinct ids. (The InferenceOrderBook events carry an `Inference` prefix, so
        // none collides with the DEX OrderBook events.) The DEX side gained seven
        // PrivateNote events (DealCredited, BookCredited, InferenceOrderRemoved,
        // InferenceOrderRejectedMirror, InferenceDealClosed, StakeForfeitConfirmed,
        // StakeDroppedLocally) and RootPN.DealWriteOffReported, and lost
        // Oracle.EventPublished and PMP.NumOutcomesSet. TokenContract gained
        // BuyerBondFunded and EndpointSet in v4.0.35.
        //
        // `PrivateNote.InferenceOrderRemoved` also changed shape there, which moves its
        // id without changing the count — a signature id is a hash of the name AND the
        // input types, so a reshaped event is a different id, not an extra one. Nothing
        // decodes the old id any more; rows carrying it stay undecoded.
        assert_eq!(decoder.known_events(), 82, "unexpected total event id count");

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

    // An arm in `token_contract_projectors.rs` named an event the ABI does not
    // declare: dead code with a forever-green test. This test holds the removal on
    // the same ground the removal was made on — the ABI, not a retelling of it. It
    // is a named check for ONE event.
    //
    // THE PROMISE HAS COME DUE. This comment used to say the general guard "arrives
    // with wave 1 and will subsume this test; at that point it should be deleted
    // rather than kept". Wave 1 arrived: `token_contract_enum_covers_every_abi_event`
    // (crates/contracts) asserts the declared variant set EQUALS the ABI's event set,
    // so a returning `StreamReclaimed` appears in the ABI, is absent from `ALL`, and
    // fails there — strictly stronger than this check, which names one event. The
    // deletion is scheduled rather than done here because removing a test belongs in
    // the pass that removes the other subsumed guards, with its reason recorded.
    #[test]
    fn token_contract_abi_does_not_declare_stream_reclaimed() {
        let abi: serde_json::Value = serde_json::from_str(ABI_TOKEN_CONTRACT).unwrap();
        let names: Vec<&str> = abi["events"]
            .as_array()
            .expect("TokenContract ABI must carry an events array")
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        // Without this check, renaming the `events` key would turn the test into a
        // claim about an empty list — true always.
        assert!(
            !names.is_empty(),
            "events array parsed empty — the ABI key changed and the test went blind"
        );
        assert!(
            !names.contains(&"StreamReclaimed"),
            "StreamReclaimed is back in the ABI: the arm and the close_kind='RECLAIMED' projection were removed as dead — restore them, not this test"
        );
    }

    // The signatures of RootModel.ContractDeployed and
    // TokenContract.ContractDeployed are BYTE-FOR-BYTE identical —
    // `{"name":"ContractDeployed","inputs":[{"name":"self","type":"address"}],
    // "outputs":[]}` in both ABIs — so their body ids are identical too. Only
    // `dst` can tell them apart, and that must be checked on the path that runs in
    // production: the route table could be read directly, but then the test would
    // survive both `decode_event_body` ignoring `dst` and a route pointing at the
    // wrong event of the same ABI.
    #[test]
    fn contract_deployed_body_decodes_per_dst_route() {
        use crate::config::event_type_dst;
        let d = Decoder::new().unwrap();
        let zero = "0:0000000000000000000000000000000000000000000000000000000000000000";
        // One body for both routes — that is exactly what the collision is.
        let body = encode_event_body_b64(
            &d,
            "TokenContract",
            "ContractDeployed",
            serde_json::json!({ "self": zero }),
        );

        // `ContractDeployedEmit` = 703 (RootModel), `DealDeployedEmit` = 732 (the deal).
        let by_root =
            d.decode_event_body(&body, Some(&event_type_dst(703))).unwrap().decoded().unwrap();
        assert_eq!(
            by_root.event_type, "RootModel.ContractDeployed",
            "a root-model deploy must stop creating an inference_deals row"
        );

        let by_deal =
            d.decode_event_body(&body, Some(&event_type_dst(732))).unwrap().decoded().unwrap();
        assert_eq!(by_deal.event_type, "TokenContract.ContractDeployed");

        // Without a dst the id is now ambiguous, and that must be VISIBLE. A silent
        // resolve to the first ABI is exactly the defect that created the phantom
        // rows: the collision counter never fired, because there was no collision.
        assert!(
            matches!(
                d.decode_event_body(&body, None).unwrap(),
                DecodeOutcome::AmbiguousCollision { .. }
            ),
            "a shared id with no dst must count as a collision, not resolve silently"
        );
    }

    #[test]
    fn registers_inference_orderbook_and_counts_unique_ids() {
        let decoder = Decoder::new().unwrap();
        assert!(decoder.contracts.contains_key("InferenceOrderBook"), "inference abi missing");
        // 57 DEX + 9 inference + 15 TokenContract + 1 RootModel = 82 (see above:
        // RootModel.ContractDeployed shares its id with TokenContract.ContractDeployed).
        assert_eq!(decoder.unique_event_ids(), 82, "unexpected unique event-id count");
    }

    #[test]
    fn inference_order_cancelled_resolves_after_rename() {
        // InferenceOrderCancelled used to share an id with OrderBook.OrderCancelled;
        // the `Inference` prefix gives it a unique id, so it resolves by id regardless
        // of dst, and its pinned dst route still decodes it to the same event.
        let decoder = Decoder::new().unwrap();
        let body = inference_cancel_body_b64(&decoder);
        let inf_dst = crate::config::event_type_dst(1001);

        let via_route =
            decoder.decode_event_body(&body, Some(&inf_dst)).unwrap().decoded().unwrap();
        assert_eq!(via_route.event_type, "InferenceOrderBook.InferenceOrderCancelled");

        let by_id = decoder.decode_event_body(&body, None).unwrap().decoded().unwrap();
        assert_eq!(by_id.event_type, "InferenceOrderBook.InferenceOrderCancelled");

        // Unknown dst no longer means ambiguous: the id is unique now, so it resolves.
        let unknown_dst =
            decoder.decode_event_body(&body, Some(":dead")).unwrap().decoded().unwrap();
        assert_eq!(unknown_dst.event_type, "InferenceOrderBook.InferenceOrderCancelled");
    }

    #[test]
    fn non_colliding_inference_event_resolves_by_id() {
        let decoder = Decoder::new().unwrap();
        // InferenceFilled has a unique id, so it resolves even with no dst route.
        let body = inference_filled_body_b64(&decoder);
        let ev = decoder.decode_event_body(&body, None).unwrap().decoded().unwrap();
        assert_eq!(ev.event_type, "InferenceOrderBook.InferenceFilled");
    }

    #[test]
    fn all_inference_events_resolve_uniquely_by_id() {
        // After the `Inference` rename no InferenceOrderBook event collides, so every
        // one maps to a UNIQUE id resolving to InferenceOrderBook.
        let d = Decoder::new().unwrap();
        let inf = d.contracts.get("InferenceOrderBook").expect("inference abi loaded");
        let mut checked = 0;
        for (name, ev) in inf.events() {
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
            checked, 9,
            "expected exactly 9 inference events (InferenceOrderPlaced, InferenceFilled, \
             InferenceExecuted, InferenceRefunded, InferenceOrderCancelled, \
             InferenceOrderBookDeployed, InferenceOrderCancelRejected, \
             InferenceOrderExpired, InferenceOrderRejected)"
        );
    }

    #[test]
    fn unknown_event_id_returns_none() {
        let decoder = Decoder::new().unwrap();

        // body BOC observed in chain from src=0:111...111 (system msg).
        // Its event id is not in the dex ABIs, so we must return Ok(None).
        let body = "te6ccgEBAgEAOAABTiEI6kGAF2XiReIi2UGGm9TvRXJAgoqOxOYUQGiHnYczEQPg+AhhEAEAF6AAAAACMteYg9IABA==";
        let decoded = decoder.decode_event_body(body, None).unwrap();
        assert!(
            matches!(decoded, DecodeOutcome::UnknownId),
            "an id in no ABI is a benign unknown, not a decode error or ambiguous collision"
        );
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

        let decoded =
            decoder.decode_event_body(body, None).unwrap().decoded().expect("event id is known");

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

    #[test]
    fn registers_token_contract_events_uniquely() {
        let decoder = Decoder::new().unwrap();
        assert!(decoder.contracts.contains_key("TokenContract"), "token contract abi missing");
        let tc = decoder.contracts.get("TokenContract").expect("token contract abi loaded");
        for (name, event) in tc.events() {
            let entries =
                decoder.event_index.get(&event.get_id()).map(Vec::as_slice).unwrap_or(&[]);
            if name == "ContractDeployed" {
                // The collision is DELIBERATE: RootModel declares an event with the
                // same signature, and only dst tells them apart. Both sides must be
                // in the index and both must have a route, otherwise resolution
                // comes down to ABI load order and a root-model deploy creates a
                // phantom deal.
                assert_eq!(
                    entries.len(),
                    2,
                    "ContractDeployed must collide with RootModel: {entries:?}"
                );
                let kinds: Vec<_> = entries.iter().map(|(k, _)| *k).collect();
                assert!(kinds.contains(&"TokenContract") && kinds.contains(&"RootModel"));
                // `routes` is private, but the test lives in the same module — no
                // accessor needed.
                assert!(decoder.routes.contains_key(&crate::config::event_type_dst(732)));
                assert!(decoder.routes.contains_key(&crate::config::event_type_dst(703)));
                continue;
            }
            assert_eq!(
                entries.len(),
                1,
                "TokenContract.{name} signature id collides with another ABI: {entries:?} — add a dst route via event_type_dst(the `*Emit` constant in contracts/airegistry/modifiers/modifiers.sol; the `TokenContractEvent` discriminant mirrors it and is pinned against it by airegistry_event_manifest.rs)"
            );
            assert_eq!(entries[0].0, "TokenContract", "{name} resolves to the wrong contract");
        }
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
            "InferenceOrderCancelled",
            serde_json::json!({
                "orderId": "7",
                "refunded": "0",
                "note": "0:0000000000000000000000000000000000000000000000000000000000000000"
            }),
        )
    }

    fn inference_filled_body_b64(d: &Decoder) -> String {
        let zero = "0:0000000000000000000000000000000000000000000000000000000000000000";
        encode_event_body_b64(
            d,
            "InferenceOrderBook",
            "InferenceFilled",
            serde_json::json!({
                "makerId": "1",
                "takerId": "2",
                "ticks": "3",
                "clearingPrice": "0",
                "sellerTC": zero,
                "buyerNote": zero,
                "sellerNote": zero
            }),
        )
    }
}
