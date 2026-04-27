// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OracleRow {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub deploy_msg_id: Option<String>,
    pub pubkey: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OracleEventListRow {
    pub id: i64,
    pub msg_id: String,
    pub oracle_id: i64,
    pub address: String,
    pub list_index: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OracleEventRow {
    pub id: i64,
    pub eventlist_id: i64,
    pub internal_id_in_eventlist: String,
    pub event_name: String,
    pub oracle_fee: Option<String>,
    pub deadline: i64,
    pub describe: Option<String>,
    pub count: Option<String>,
    pub trust_addr: Option<String>,
    pub outcome_names_jsonb: serde_json::Value,
    pub is_deleted: bool,
}
