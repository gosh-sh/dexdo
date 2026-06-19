use wasm_bindgen::prelude::*;

use crate::client::DexClient;
use crate::client::DexConfig;

#[wasm_bindgen]
pub struct Dex {
    inner: DexClient,
}

#[wasm_bindgen]
impl Dex {
    #[wasm_bindgen(constructor)]
    pub fn new(endpoints: Vec<String>, api_token: Option<String>) -> Result<Dex, JsError> {
        let inner = DexClient::new(DexConfig { endpoints, api_token, max_rps: None })
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(Self { inner })
    }

    // ── Market discovery ─────────────────────────────────────────

    /// Returns all oracles deployed via RootOracle.
    #[wasm_bindgen(js_name = discover_oracles)]
    pub async fn discover_oracles(&self) -> Result<JsValue, JsError> {
        let oracles = self
            .inner
            .market()
            .discover_oracles()
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&oracles).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    /// Returns all markets (PMPs) for a given token_type.
    #[wasm_bindgen(js_name = discover_markets)]
    pub async fn discover_markets(&self, token_type: u32) -> Result<JsValue, JsError> {
        let markets = self
            .inner
            .market()
            .discover_markets(token_type)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&markets).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    /// Returns only active markets (approved, not cancelled, not resolved).
    #[wasm_bindgen(js_name = discover_active_markets)]
    pub async fn discover_active_markets(&self, token_type: u32) -> Result<JsValue, JsError> {
        let markets = self
            .inner
            .market()
            .discover_active_markets(token_type)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&markets).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    // ── Oracle events ────────────────────────────────────────────

    /// Returns parsed events from an OracleEventList.
    #[wasm_bindgen(js_name = get_parsed_events)]
    pub async fn get_parsed_events(&self, event_list_address: String) -> Result<JsValue, JsError> {
        let raw = self
            .inner
            .oracle()
            .get_events(&event_list_address)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        let parsed: Vec<_> = raw
            .events
            .iter()
            .filter_map(|(id, val)| {
                // Contract getter returns ABI-camelCase keys, lookups must
                // match (eventName / oracleFee, not snake_case).
                let event_name = val.get("eventName")?.as_str()?.to_string();
                let oracle_fee = val.get("oracleFee")?.as_str()?.parse::<u128>().ok()?;
                let deadline = val.get("deadline")?.as_str()?.parse::<u64>().ok()?;
                let describe =
                    val.get("describe").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let outcome_names: std::collections::HashMap<u32, String> = val
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

                Some(serde_json::json!({
                    "event_id": id,
                    "event_name": event_name,
                    "oracle_fee": oracle_fee,
                    "deadline": deadline,
                    "describe": describe,
                    "outcome_names": outcome_names,
                }))
            })
            .collect();

        serde_wasm_bindgen::to_value(&parsed).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    // ── PMP details ──────────────────────────────────────────────

    /// Returns PMP details for a given address.
    #[wasm_bindgen(js_name = get_pmp_details)]
    pub async fn get_pmp_details(&self, pmp_address: String) -> Result<JsValue, JsError> {
        let details = self
            .inner
            .pmp()
            .get_details(&pmp_address)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&details).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    // ── PrivateNote read ─────────────────────────────────────────

    /// Returns PrivateNote details.
    #[wasm_bindgen(js_name = get_private_note_details)]
    pub async fn get_private_note_details(&self, pn_address: String) -> Result<JsValue, JsError> {
        let details = self
            .inner
            .private_note()
            .get_details(&pn_address)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&details).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    /// Returns stakes for a PrivateNote.
    #[wasm_bindgen(js_name = get_stakes)]
    pub async fn get_stakes(&self, pn_address: String) -> Result<JsValue, JsError> {
        let stakes = self
            .inner
            .private_note()
            .get_stakes(&pn_address)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&stakes).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    /// Returns PrivateNote address by deposit_identifier_hash.
    #[wasm_bindgen(js_name = get_private_note_address)]
    pub async fn get_private_note_address(
        &self,
        deposit_identifier_hash: String,
    ) -> Result<String, JsError> {
        self.inner
            .private_note()
            .get_private_note_address(
                dodex_contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress {
                    deposit_identifier_hash,
                },
            )
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Discover PrivateNotes matching derived public keys.
    /// `derived_pubkeys_dec_js` — JS array of decimal pubkey strings.
    #[wasm_bindgen(js_name = discover_my_notes)]
    pub async fn discover_my_notes(
        &self,
        derived_pubkeys_dec_js: Vec<String>,
    ) -> Result<JsValue, JsError> {
        let pubkeys: std::collections::HashSet<String> =
            derived_pubkeys_dec_js.into_iter().collect();
        let notes = self
            .inner
            .private_note()
            .discover_notes(&pubkeys)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&notes).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    // ── PN history ────────────────────────────────────────────────

    /// Paginated history of PN events (stakes, claims, transfers, etc.).
    /// Pass multiple addresses to merge history across notes.
    #[wasm_bindgen(js_name = get_notes_history)]
    pub async fn get_notes_history(
        &self,
        pn_addresses: Vec<String>,
        limit: u32,
        cursor: Option<String>,
    ) -> Result<JsValue, JsError> {
        let result = self
            .inner
            .private_note()
            .get_notes_history(&pn_addresses, limit, cursor)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&result).map_err(|e| JsError::new(&format!("serialize: {e}")))
    }

    // ── Oracle admin read ────────────────────────────────────────

    /// Returns oracle address by name.
    #[wasm_bindgen(js_name = get_oracle_address)]
    pub async fn get_oracle_address(&self, name: String) -> Result<String, JsError> {
        self.inner.oracle().get_oracle_address(name).await.map_err(|e| JsError::new(&e.to_string()))
    }

    /// Returns event list address for an oracle.
    #[wasm_bindgen(js_name = get_event_list_address)]
    pub async fn get_event_list_address(
        &self,
        oracle_address: String,
        index: u32,
    ) -> Result<String, JsError> {
        self.inner
            .oracle()
            .get_event_list_address(
                &oracle_address,
                dodex_contracts::dex::oracle::ParamsOfGetEventListAddress { index: index as u128 },
            )
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }
}
