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
use tvm_abi::token::Detokenizer;
use tvm_abi::Contract;
use tvm_abi::Function;
use tvm_types::read_single_root_boc;
use tvm_types::SliceData;

const ABI_ROOT_ORACLE: &str = include_str!("../../../contracts/abi/dex/RootOracle.abi.json");
const ABI_ORACLE: &str = include_str!("../../../contracts/abi/dex/Oracle.abi.json");
const ABI_ORACLE_EVENT_LIST: &str =
    include_str!("../../../contracts/abi/dex/OracleEventList.abi.json");
const ABI_PMP: &str = include_str!("../../../contracts/abi/dex/PMP.abi.json");
const ABI_ORDER_BOOK: &str = include_str!("../../../contracts/abi/dex/OrderBook.abi.json");
const ABI_ROOT_PN: &str = include_str!("../../../contracts/abi/dex/RootPN.abi.json");
const ABI_PRIVATE_NOTE: &str = include_str!("../../../contracts/abi/dex/PrivateNote.abi.json");
const ABI_NULLIFIER: &str = include_str!("../../../contracts/abi/dex/Nullifier.abi.json");

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

#[derive(Debug, Clone, Serialize)]
pub struct DecodedEvent {
    pub contract_kind: &'static str,
    pub event_name: String,
    pub event_type: String,
    pub value: Value,
}

#[derive(Clone)]
pub struct Decoder {
    contracts: HashMap<&'static str, Contract>,
    event_index: HashMap<u32, (&'static str, String)>,
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
        let mut contracts = HashMap::with_capacity(DEX_ABIS.len());
        let mut event_index: HashMap<u32, (&'static str, String)> = HashMap::new();

        for &(kind, abi_json) in DEX_ABIS {
            let contract = Contract::load(Cursor::new(abi_json))
                .with_context(|| format!("load abi for {kind}"))?;

            for (name, event) in contract.events() {
                let id = event.get_id();
                event_index.entry(id).or_insert_with(|| (kind, name.clone()));
            }

            contracts.insert(kind, contract);
        }

        Ok(Self { contracts, event_index })
    }

    pub fn known_events(&self) -> usize {
        self.event_index.len()
    }

    /// Borrow the parsed `Contract` for a given dex kind (e.g. `"PMP"`).
    /// Used by the reconciler to drive off-chain getters.
    pub fn contract(&self, kind: &str) -> Option<&Contract> {
        self.contracts.get(kind)
    }

    pub fn decode_event_body(&self, body_base64: &str) -> anyhow::Result<Option<DecodedEvent>> {
        let bytes = BASE64_STANDARD.decode(body_base64).context("decode base64 body")?;
        let cell = read_single_root_boc(bytes).context("read boc")?;
        let slice = SliceData::load_cell(cell).map_err(|e| anyhow!("slice from cell: {e}"))?;

        let event_id = Function::decode_output_id(slice.clone())
            .map_err(|e| anyhow!("decode event id: {e}"))?;

        let Some((kind, event_name)) = self.event_index.get(&event_id) else {
            return Ok(None);
        };

        let contract = self.contracts.get(kind).expect("contract index consistent with abi list");

        let decoded = contract
            .decode_output(slice, true, true)
            .map_err(|e| anyhow!("decode_output for {kind}.{event_name}: {e}"))?;

        let value = Detokenizer::detokenize_to_json_value(&decoded.tokens)
            .map_err(|e| anyhow!("detokenize {kind}.{event_name}: {e}"))?;

        Ok(Some(DecodedEvent {
            contract_kind: kind,
            event_name: event_name.clone(),
            event_type: format!("{kind}.{event_name}"),
            value,
        }))
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

        // 13 PMP + 2 Oracle + 3 OracleEventList + 8 OrderBook + 1 RootOracle
        // + 4 RootPN + 14 PrivateNote + 0 Nullifier = 45
        assert_eq!(decoder.known_events(), 45, "unexpected total event count");

        // sample lookups
        let pmp_event_ids: Vec<_> = decoder
            .event_index
            .iter()
            .filter(|(_, (kind, _))| *kind == "PMP")
            .map(|(id, (_, name))| (*id, name.clone()))
            .collect();
        assert!(pmp_event_ids.iter().any(|(_, n)| n == "Resolved"));
        assert!(pmp_event_ids.iter().any(|(_, n)| n == "ApprovedByOracle"));
    }

    #[test]
    fn unknown_event_id_returns_none() {
        let decoder = Decoder::new().unwrap();

        // body BOC observed in chain from src=0:111...111 (system msg).
        // Its event id is not in the dex ABIs, so we must return Ok(None).
        let body = "te6ccgEBAgEAOAABTiEI6kGAF2XiReIi2UGGm9TvRXJAgoqOxOYUQGiHnYczEQPg+AhhEAEAF6AAAAACMteYg9IABA==";
        let decoded = decoder.decode_event_body(body).unwrap();
        assert!(decoded.is_none());
    }

    #[test]
    fn invalid_base64_is_error() {
        let decoder = Decoder::new().unwrap();
        assert!(decoder.decode_event_body("not_a_boc!!!").is_err());
    }
}
