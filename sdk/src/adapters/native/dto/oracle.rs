use std::collections::HashMap;

use ackinacki_kit::contracts::dex::oracle_event_list::ResultOfGetEvents as KitEvents;
use serde::Deserialize;
use serde::Serialize;

/// Parsed event from OracleEventList.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventInfo {
    pub event_id: String,
    pub event_name: String,
    pub oracle_fee: u128,
    pub deadline: u64,
    pub describe: String,
    pub outcome_names: HashMap<u32, String>,
}

/// Raw events map — for cases when typed parsing fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleEvents {
    pub events: HashMap<String, serde_json::Value>,
}

impl From<KitEvents> for OracleEvents {
    fn from(e: KitEvents) -> Self {
        Self { events: e.events }
    }
}

impl OracleEvents {
    /// Parse raw events into typed EventInfo structs. The contract getter
    /// `OracleEventList._events` returns ABI-camelCase keys, so we look up
    /// `eventName` / `oracleFee` (not snake_case).
    pub fn parse(&self) -> Vec<EventInfo> {
        self.events
            .iter()
            .filter_map(|(id, val)| {
                let event_name = val.get("eventName")?.as_str()?.to_string();
                let oracle_fee = val.get("oracleFee")?.as_str()?.parse::<u128>().ok()?;
                let deadline = val.get("deadline")?.as_str()?.parse::<u64>().ok()?;
                let describe =
                    val.get("describe").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let outcome_names = val
                    .get("outcomeNames")
                    .and_then(|v| v.as_object())
                    .map(|map| {
                        map.iter()
                            .filter_map(|(k, v)| {
                                Some((k.parse::<u32>().ok()?, v.as_str()?.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                Some(EventInfo {
                    event_id: id.clone(),
                    event_name,
                    oracle_fee,
                    deadline,
                    describe,
                    outcome_names,
                })
            })
            .collect()
    }
}
