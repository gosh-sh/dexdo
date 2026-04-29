// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use anyhow::Context;
use serde_json::Value;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;

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
    match event.event_type.as_str() {
        "RootOracle.OracleDeployed" => apply_oracle_deployed(tx, event, node)
            .await
            .with_context(|| format!("project {} (msg_id={})", event.event_type, node.msg_id)),
        "Oracle.OracleEventListDeployed" => apply_oracle_event_list_deployed(tx, event, node)
            .await
            .with_context(|| format!("project {} (msg_id={})", event.event_type, node.msg_id)),
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

fn field_str<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value.get(key).and_then(Value::as_str).with_context(|| format!("missing field `{key}`"))
}
