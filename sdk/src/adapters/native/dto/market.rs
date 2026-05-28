use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;

use crate::services::market::MarketInfo as InnerMarketInfo;
use crate::services::market::OracleInfo as InnerOracleInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleInfo {
    pub name: String,
    pub address: String,
    pub pubkey: String,
}

impl From<InnerOracleInfo> for OracleInfo {
    fn from(o: InnerOracleInfo) -> Self {
        Self { name: o.name, address: o.address, pubkey: o.pubkey }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketInfo {
    pub pmp_address: String,
    pub event_id: String,
    pub event_name: String,
    pub oracle_name: String,
    pub oracle_fee: u128,
    pub token_type: u32,
    pub total_pool: u128,
    pub num_outcomes: u32,
    pub outcome_names: HashMap<u32, String>,
    pub approved: bool,
    pub is_cancelled: bool,
    pub resolved_outcome: Option<u32>,
    pub stake_start: u64,
    pub stake_end: u64,
    pub result_start: u64,
    pub result_end: u64,
    pub frozen: bool,
}

impl From<InnerMarketInfo> for MarketInfo {
    fn from(m: InnerMarketInfo) -> Self {
        Self {
            pmp_address: m.pmp_address,
            event_id: m.event_id,
            event_name: m.event_name,
            oracle_name: m.oracle_name,
            oracle_fee: m.oracle_fee,
            token_type: m.token_type,
            total_pool: m.total_pool,
            num_outcomes: m.num_outcomes,
            outcome_names: m.outcome_names,
            approved: m.approved,
            is_cancelled: m.is_cancelled,
            resolved_outcome: m.resolved_outcome,
            stake_start: m.stake_start,
            stake_end: m.stake_end,
            result_start: m.result_start,
            result_end: m.result_end,
            frozen: m.frozen,
        }
    }
}
