use std::collections::HashMap;

use dodex_contracts::dex::pmp::ResultOfGetDetails as KitPmpDetails;
use dodex_contracts::dex::pmp::ResultOfGetShutdownState as KitPmpShutdownState;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmpDetails {
    pub name: String,
    pub token_type: u32,
    pub event_id: String,
    pub oracle_list_hash: String,
    pub deployer: String,
    pub private_note_code_hash: String,
    pub total_pool: u128,
    pub approved: bool,
    pub num_outcomes: u32,
    pub resolved_outcome: Option<u32>,
    pub stake_start: u64,
    pub stake_end: u64,
    pub result_start: u64,
    pub result_end: u64,
    pub is_cancelled: bool,
    pub number_of_oracle_events: u128,
    pub approved_oracle_events: u128,
    pub outcome_names: HashMap<u32, String>,
    pub creator_fee: u128,
    pub frozen: bool,
    pub base_total_pool: u128,
}

/// PMP-side shutdown state. Distinct from the OrderBook's
/// `getShutdownState`: this one tells you whether the OB has reported
/// `onOrderBookShutdownComplete` back to the PMP, which is required
/// before `claim()` is accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmpShutdownState {
    /// True once the OB drained its queue and called
    /// `onOrderBookShutdownComplete` — gate on this before claiming.
    pub order_book_done: bool,
    pub shutdown_triggered: bool,
}

impl From<KitPmpShutdownState> for PmpShutdownState {
    fn from(s: KitPmpShutdownState) -> Self {
        Self { order_book_done: s.order_book_done, shutdown_triggered: s.shutdown_triggered }
    }
}

impl From<KitPmpDetails> for PmpDetails {
    fn from(d: KitPmpDetails) -> Self {
        Self {
            name: d.name,
            token_type: d.token_type,
            event_id: d.event_id,
            oracle_list_hash: d.oracle_list_hash,
            deployer: d.deployer,
            private_note_code_hash: d.private_note_code_hash,
            total_pool: d.total_pool,
            approved: d.approved,
            num_outcomes: d.num_outcomes,
            resolved_outcome: d.resolved_outcome,
            stake_start: d.stake_start,
            stake_end: d.stake_end,
            result_start: d.result_start,
            result_end: d.result_end,
            is_cancelled: d.is_cancelled,
            number_of_oracle_events: d.number_of_oracle_events,
            approved_oracle_events: d.approved_oracle_events,
            outcome_names: d.outcome_names,
            creator_fee: d.creator_fee,
            frozen: d.frozen,
            base_total_pool: d.base_total_pool,
        }
    }
}
