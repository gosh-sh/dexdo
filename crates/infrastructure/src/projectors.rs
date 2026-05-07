// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use anyhow::anyhow;
use anyhow::Context;
use num_bigint::BigUint;
use serde_json::Value;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;
use crate::indexer_repo::parse_unix_seconds;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionOutcome {
    Applied,
    Deferred,
    #[default]
    Unknown,
}

pub async fn project_event(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let context = || format!("project {} (msg_id={})", event.event_type, node.msg_id);
    match event.event_type.as_str() {
        "RootOracle.OracleDeployed" => {
            apply_oracle_deployed(tx, event, node).await.with_context(context)
        }
        "Oracle.OracleEventListDeployed" => {
            apply_oracle_event_list_deployed(tx, event, node).await.with_context(context)
        }
        "OracleEventList.EventAdded" => {
            apply_event_added(tx, event, node).await.with_context(context)
        }
        "OracleEventList.EventConfirmed" => {
            apply_event_confirmed(tx, event, node).await.with_context(context)
        }
        "PrivateNote.PMPDeployed" => {
            apply_pmp_deployed(tx, event, node).await.with_context(context)
        }
        "PMP.TimingsSet" => apply_timings_set(tx, event, node).await.with_context(context),
        "PMP.PoolsFrozen" => apply_pools_frozen(tx, node).await.with_context(context),
        "PMP.Resolved" => apply_resolved(tx, event, node).await.with_context(context),
        "PMP.EventCancelled" => {
            apply_pmp_cancellation(tx, node, "EVENT_CANCELLED").await.with_context(context)
        }
        "PMP.PMPCancelled" => {
            apply_pmp_cancellation(tx, node, "PMP_CANCELLED").await.with_context(context)
        }
        _ => Ok(ProjectionOutcome::Unknown),
    }
}

async fn apply_oracle_deployed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let address = field_str(&event.value, "oracle")?;
    let pubkey = field_str(&event.value, "pubkey")?;
    let name = field_str(&event.value, "name")?;

    sqlx::query(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey, updated_at)
           values ($1, $2, $3, $4, now())
           on conflict (address) do update
               set name = excluded.name,
                   deploy_msg_id = coalesce(oracles.deploy_msg_id, excluded.deploy_msg_id),
                   pubkey = coalesce(oracles.pubkey, excluded.pubkey),
                   updated_at = now()"#,
    )
    .bind(name)
    .bind(address)
    .bind(&node.msg_id)
    .bind(pubkey)
    .execute(&mut **tx)
    .await
    .context("upsert oracles")?;

    Ok(ProjectionOutcome::Applied)
}

async fn apply_oracle_event_list_deployed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let address = field_str(&event.value, "eventListAddress")?;
    let index_raw = field_str(&event.value, "index")?;
    let list_index: i64 = index_raw
        .parse()
        .with_context(|| format!("parse OracleEventListDeployed.index = {index_raw}"))?;

    let oracle_address =
        node.src.as_deref().context("OracleEventListDeployed: src missing on event message")?;

    let parent: Option<(i64,)> = sqlx::query_as("select id from oracles where address = $1")
        .bind(oracle_address)
        .fetch_optional(&mut **tx)
        .await
        .context("lookup oracle id by address")?;

    let Some((oracle_id,)) = parent else {
        warn!(
            oracle_address,
            msg_id = %node.msg_id,
            "OracleEventListDeployed observed before parent OracleDeployed; deferring"
        );
        return Ok(ProjectionOutcome::Deferred);
    };

    sqlx::query(
        r#"insert into oracle_event_lists (msg_id, oracle_id, address, list_index)
           values ($1, $2, $3, $4)
           on conflict (msg_id) do update
               set oracle_id = excluded.oracle_id,
                   address = excluded.address,
                   list_index = excluded.list_index"#,
    )
    .bind(&node.msg_id)
    .bind(oracle_id)
    .bind(address)
    .bind(list_index)
    .execute(&mut **tx)
    .await
    .context("upsert oracle_event_lists")?;

    Ok(ProjectionOutcome::Applied)
}

async fn apply_event_added(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let eventlist_address =
        node.src.as_deref().context("EventAdded: src missing on event message")?;

    let event_id_hex = field_str(&event.value, "eventId")?;
    let event_id_decimal = uint256_hex_to_decimal(event_id_hex)?;

    let event_name = field_str(&event.value, "eventName")?;
    let oracle_fee = field_str(&event.value, "oracleFee")?;
    let deadline_raw = field_str(&event.value, "deadline")?;
    let deadline: i64 = deadline_raw
        .parse()
        .with_context(|| format!("parse EventAdded.deadline = {deadline_raw}"))?;

    let parent: Option<(i64,)> =
        sqlx::query_as("select id from oracle_event_lists where address = $1")
            .bind(eventlist_address)
            .fetch_optional(&mut **tx)
            .await
            .context("lookup oracle_event_lists id by address")?;

    let Some((eventlist_id,)) = parent else {
        warn!(
            eventlist_address,
            msg_id = %node.msg_id,
            "EventAdded observed before parent OracleEventListDeployed; deferring"
        );
        return Ok(ProjectionOutcome::Deferred);
    };

    sqlx::query(
        r#"insert into oracle_events
               (eventlist_id, internal_id_in_eventlist, event_name,
                oracle_fee, deadline, last_seen_at, updated_at)
           values ($1, $2::numeric, $3, $4::numeric, $5, now(), now())
           on conflict (eventlist_id, internal_id_in_eventlist) do update
               set event_name = excluded.event_name,
                   oracle_fee = excluded.oracle_fee,
                   deadline = excluded.deadline,
                   is_deleted = false,
                   last_seen_at = now(),
                   updated_at = now()"#,
    )
    .bind(eventlist_id)
    .bind(&event_id_decimal)
    .bind(event_name)
    .bind(oracle_fee)
    .bind(deadline)
    .execute(&mut **tx)
    .await
    .context("upsert oracle_events on EventAdded")?;

    Ok(ProjectionOutcome::Applied)
}

async fn apply_event_confirmed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let eventlist_address =
        node.src.as_deref().context("EventConfirmed: src missing on event message")?;

    let event_id_hex = field_str(&event.value, "eventId")?;
    let event_id_decimal = uint256_hex_to_decimal(event_id_hex)?;
    let pmp_address = field_str(&event.value, "pmpAddress")?;

    let parent: Option<(i64,)> =
        sqlx::query_as("select id from oracle_event_lists where address = $1")
            .bind(eventlist_address)
            .fetch_optional(&mut **tx)
            .await
            .context("lookup oracle_event_lists id by address")?;

    let Some((eventlist_id,)) = parent else {
        warn!(
            eventlist_address,
            msg_id = %node.msg_id,
            "EventConfirmed observed before parent OracleEventListDeployed; deferring"
        );
        return Ok(ProjectionOutcome::Deferred);
    };

    let updated = sqlx::query(
        r#"update oracle_events
              set confirmed_pmp_address = $1,
                  confirmed_at = now(),
                  updated_at = now()
            where eventlist_id = $2
              and internal_id_in_eventlist = $3::numeric"#,
    )
    .bind(pmp_address)
    .bind(eventlist_id)
    .bind(&event_id_decimal)
    .execute(&mut **tx)
    .await
    .context("update oracle_events on EventConfirmed")?
    .rows_affected();

    if updated == 0 {
        warn!(
            eventlist_address,
            event_id = %event_id_decimal,
            pmp_address,
            msg_id = %node.msg_id,
            "EventConfirmed observed before EventAdded; deferring"
        );
        return Ok(ProjectionOutcome::Deferred);
    }

    Ok(ProjectionOutcome::Applied)
}

async fn apply_pmp_deployed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let pmp_address = field_str(&event.value, "pmpAddress")?;

    let event_id_hex = field_str(&event.value, "eventId")?;
    let event_id_decimal = uint256_hex_to_decimal(event_id_hex)?;

    let token_type_raw = field_str(&event.value, "tokenType")?;
    let token_type: i32 = token_type_raw
        .parse()
        .with_context(|| format!("parse PMPDeployed.tokenType = {token_type_raw}"))?;

    // Resolve token_code via ref_tokens. If the token is unknown — defer; the
    // ref_tokens row may show up later (manual seed update or new migration).
    let token: Option<(String,)> =
        sqlx::query_as("select token_code from ref_tokens where token_type = $1")
            .bind(token_type)
            .fetch_optional(&mut **tx)
            .await
            .context("lookup ref_tokens by token_type")?;

    let Some((token_code,)) = token else {
        warn!(
            pmp_address,
            token_type,
            msg_id = %node.msg_id,
            "PMPDeployed for unknown token_type; deferring until ref_tokens has it"
        );
        return Ok(ProjectionOutcome::Deferred);
    };

    let oracle_event_lists = event.value.get("oracleEventLists").cloned().unwrap_or(Value::Null);
    let oracle_fee = event.value.get("oracleFee").cloned().unwrap_or(Value::Null);
    let block_unix = node_unix_seconds(node);

    sqlx::query(
        r#"insert into markets
               (pmp_address, event_id, token_type, token_code,
                oracle_event_lists_json, oracle_fee_json,
                created_at, updated_at)
           values ($1, $2::numeric, $3, $4, $5, $6,
                   coalesce(to_timestamp($7::bigint), now()),
                   coalesce(to_timestamp($7::bigint), now()))
           on conflict (pmp_address) do update
               set event_id = excluded.event_id,
                   token_type = excluded.token_type,
                   token_code = excluded.token_code,
                   oracle_event_lists_json = excluded.oracle_event_lists_json,
                   oracle_fee_json = excluded.oracle_fee_json,
                   updated_at = excluded.updated_at"#,
    )
    .bind(pmp_address)
    .bind(&event_id_decimal)
    .bind(token_type)
    .bind(&token_code)
    .bind(oracle_event_lists)
    .bind(oracle_fee)
    .bind(block_unix)
    .execute(&mut **tx)
    .await
    .context("upsert markets on PMPDeployed")?;

    Ok(ProjectionOutcome::Applied)
}

async fn apply_timings_set(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let pmp_address = node.src.as_deref().context("TimingsSet: src missing on event message")?;

    let stake_start = parse_u64_field(&event.value, "stakeStart")?;
    let stake_end = parse_u64_field(&event.value, "stakeEnd")?;
    let result_start = parse_u64_field(&event.value, "resultStart")?;
    let result_end = parse_u64_field(&event.value, "resultEnd")?;

    let updated = sqlx::query(
        r#"update markets
              set stake_start = $1,
                  stake_end = $2,
                  result_start = $3,
                  result_end = $4,
                  approved = true,
                  updated_at = now()
            where pmp_address = $5"#,
    )
    .bind(stake_start)
    .bind(stake_end)
    .bind(result_start)
    .bind(result_end)
    .bind(pmp_address)
    .execute(&mut **tx)
    .await
    .context("update markets on TimingsSet")?
    .rows_affected();

    if updated == 0 {
        warn!(pmp_address, msg_id = %node.msg_id, "TimingsSet observed before PMPDeployed; deferring");
        return Ok(ProjectionOutcome::Deferred);
    }
    Ok(ProjectionOutcome::Applied)
}

async fn apply_pools_frozen(
    tx: &mut Transaction<'_, Postgres>,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let pmp_address = node.src.as_deref().context("PoolsFrozen: src missing on event message")?;
    let frozen_at = node_unix_seconds(node);

    let updated = sqlx::query(
        r#"update markets
              set frozen_at = coalesce(frozen_at, $1),
                  updated_at = now()
            where pmp_address = $2"#,
    )
    .bind(frozen_at)
    .bind(pmp_address)
    .execute(&mut **tx)
    .await
    .context("update markets on PoolsFrozen")?
    .rows_affected();

    if updated == 0 {
        warn!(pmp_address, msg_id = %node.msg_id, "PoolsFrozen observed before PMPDeployed; deferring");
        return Ok(ProjectionOutcome::Deferred);
    }
    Ok(ProjectionOutcome::Applied)
}

async fn apply_resolved(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let pmp_address = node.src.as_deref().context("Resolved: src missing on event message")?;
    let outcome_id_raw = field_str(&event.value, "outcomeId")?;
    let outcome_id: i32 = outcome_id_raw
        .parse()
        .with_context(|| format!("parse Resolved.outcomeId = {outcome_id_raw}"))?;
    let resolved_at = node_unix_seconds(node);

    let updated = sqlx::query(
        r#"update markets
              set resolved_at = coalesce(resolved_at, $1),
                  resolved_outcome_id = $2,
                  updated_at = now()
            where pmp_address = $3"#,
    )
    .bind(resolved_at)
    .bind(outcome_id)
    .bind(pmp_address)
    .execute(&mut **tx)
    .await
    .context("update markets on Resolved")?
    .rows_affected();

    if updated == 0 {
        warn!(pmp_address, msg_id = %node.msg_id, "Resolved observed before PMPDeployed; deferring");
        return Ok(ProjectionOutcome::Deferred);
    }
    Ok(ProjectionOutcome::Applied)
}

async fn apply_pmp_cancellation(
    tx: &mut Transaction<'_, Postgres>,
    node: &EventNode,
    reason: &str,
) -> anyhow::Result<ProjectionOutcome> {
    let pmp_address =
        node.src.as_deref().context("Cancellation event: src missing on event message")?;
    let cancelled_at = node_unix_seconds(node);

    let updated = sqlx::query(
        r#"update markets
              set is_cancelled = true,
                  cancelled_at = coalesce(cancelled_at, $1),
                  cancel_reason = coalesce(cancel_reason, $2),
                  updated_at = now()
            where pmp_address = $3"#,
    )
    .bind(cancelled_at)
    .bind(reason)
    .bind(pmp_address)
    .execute(&mut **tx)
    .await
    .context("update markets on cancellation event")?
    .rows_affected();

    if updated == 0 {
        warn!(pmp_address, msg_id = %node.msg_id, reason, "cancellation observed before PMPDeployed; deferring");
        return Ok(ProjectionOutcome::Deferred);
    }
    Ok(ProjectionOutcome::Applied)
}

fn parse_u64_field(value: &Value, key: &str) -> anyhow::Result<i64> {
    let raw = field_str(value, key)?;
    raw.parse::<i64>().with_context(|| format!("parse {key} = {raw}"))
}

fn node_unix_seconds(node: &EventNode) -> Option<i64> {
    parse_unix_seconds(node.created_at.as_ref()).map(|v| v as i64)
}

fn field_str<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value.get(key).and_then(Value::as_str).with_context(|| format!("missing field `{key}`"))
}

pub fn uint256_hex_to_decimal(value: &str) -> anyhow::Result<String> {
    // tvm_abi serialises uint256 as "0x" + 64 lowercase hex chars.
    let stripped = value.strip_prefix("0x").unwrap_or(value);
    let big = BigUint::parse_bytes(stripped.as_bytes(), 16)
        .ok_or_else(|| anyhow!("invalid uint256 hex: {value}"))?;
    Ok(big.to_str_radix(10))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uint256_hex_to_decimal() {
        let zero = "0x0000000000000000000000000000000000000000000000000000000000000000";
        let one = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let u64_max = "0x000000000000000000000000000000000000000000000000ffffffffffffffff";
        let u256_max = "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

        assert_eq!(uint256_hex_to_decimal(zero).unwrap(), "0");
        assert_eq!(uint256_hex_to_decimal(one).unwrap(), "1");
        assert_eq!(uint256_hex_to_decimal(u64_max).unwrap(), "18446744073709551615");
        assert_eq!(
            uint256_hex_to_decimal(u256_max).unwrap(),
            "115792089237316195423570985008687907853269984665640564039457584007913129639935"
        );
        // accepts also no-prefix form
        assert_eq!(uint256_hex_to_decimal("ff").unwrap(), "255");
    }

    #[test]
    fn rejects_invalid_hex() {
        assert!(uint256_hex_to_decimal("0xZZ").is_err());
        assert!(uint256_hex_to_decimal("not_hex").is_err());
    }
}
