//! Misc small helpers: time, account-active wait, NACKL balance read,
//! GraphQL event-entry destructuring.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::traits::AccountAccessor;

use crate::common::context::TOKEN_TYPE_NACKL;

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_secs()
}

pub async fn wait_active<T: AccountAccessor>(contract: &T, label: &str) {
    contract
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(30),
            attempts_timeout: Some(2_000),
        })
        .await
        .unwrap_or_else(|e| panic!("wait {label} active: {e:?}"));
}

pub fn pn_nackl(details: &dodex_sdk::PrivateNoteDetails) -> u128 {
    details.balance.get(&TOKEN_TYPE_NACKL.to_string()).copied().unwrap_or_default()
}

/// Pull `eventName` from an `OracleEventList._events` map entry. The getter
/// returns ABI-camelCase fields, so we look up `eventName` (not snake_case).
pub fn event_entry_name(entry: &serde_json::Value) -> Option<&str> {
    entry.get("eventName").and_then(|v| v.as_str())
}
