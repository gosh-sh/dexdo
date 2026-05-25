// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use anyhow::anyhow;
use anyhow::Context;
use num_bigint::BigUint;
use serde_json::Value;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::debug;
use tracing::error;
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
        "PMP.PMPRejected" => {
            apply_pmp_cancellation(tx, node, "PMP_REJECTED_BY_ORACLE").await.with_context(context)
        }
        "OrderBook.OrderPlaced" => apply_order_placed(tx, event, node).await.with_context(context),
        "OrderBook.OrderFilled" => apply_order_filled(tx, event, node).await.with_context(context),
        "OrderBook.OrderCancelled" => {
            apply_order_cancelled(tx, event, node).await.with_context(context)
        }
        "PrivateNote.OrderPlacedConfirmed" => {
            apply_order_placed_confirmed(tx, event, node).await.with_context(context)
        }
        // Observability-only OrderBook events; state of the book does not change.
        "OrderBook.PartialFill"
        | "OrderBook.FullyFilled"
        | "OrderBook.Queued"
        | "OrderBook.Rejected"
        | "OrderBook.CallbackBounced" => Ok(ProjectionOutcome::Applied),
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

async fn apply_order_placed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let orderbook_address =
        node.src.as_deref().context("OrderPlaced: src missing on event message")?;

    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let outcome_id_raw = field_str(&event.value, "outcomeId")?;
    let outcome_id: i32 = outcome_id_raw
        .parse()
        .with_context(|| format!("parse OrderPlaced.outcomeId = {outcome_id_raw}"))?;
    let is_buy = event
        .value
        .get("isBuy")
        .and_then(Value::as_bool)
        .context("OrderPlaced: missing field `isBuy`")?;
    let price = uint_field_to_decimal(&event.value, "price")?;
    let amount = uint_field_to_decimal(&event.value, "amount")?;
    // `clientOrderId` is genuinely optional (some placements omit it),
    // so absent / JSON null collapse to `None`. A present-but-non-string
    // payload is schema drift, not "no clientOrderId" — propagate as an
    // error so the user-correlatable id does not silently land NULL.
    let client_order_id = match event.value.get("clientOrderId") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            return Err(anyhow!(
                "OrderPlaced: field `clientOrderId` has unexpected JSON type: {other:?}"
            ));
        }
    };
    let chain_order = node_chain_order(node, "OrderPlaced")?;

    // chain_created_at / chain_updated_at survive sub-second precision via
    // to_timestamp(::double precision). They are display-only — the primary
    // sort key for /api/v1/orders is placed_chain_order (bound from
    // chain_order, $8), which is globally unique and lex-monotonic by
    // gateway design. node.created_at collides on a shared chain second
    // and is not safe as a sort key.
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    if chain_seconds.is_none() {
        // The row will land with NULL `chain_created_at` and stay invisible
        // to `/orders` because of the partial-index predicate. The path
        // is documented as rare; surface it so we notice if it stops being.
        warn!(
            orderbook_address,
            msg_id = %node.msg_id,
            created_at = ?node.created_at,
            "OrderPlaced has no parseable chain time; live_orders row will be \
             hidden from /orders by the chain_created_at IS NOT NULL heap \
             filter. placed_chain_order is unaffected (chain_order is NOT NULL).",
        );
    }

    let result = sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, client_order_id, status, last_chain_order,
                chain_created_at, chain_updated_at,
                placed_chain_order,
                updated_at)
           values ($1, $2::numeric, $3, $4, $5::numeric,
                   $6::numeric, $6::numeric, $7, 'OPEN', $8,
                   to_timestamp($9::double precision), to_timestamp($9::double precision),
                   $8,
                   now())
           -- The ON CONFLICT arm is WHERE-guarded to fire only on a row
           -- that is still in its fresh, unmutated state — `status` is
           -- non-terminal AND `amount_remaining = amount_initial` (no
           -- fills observed yet). The legitimate case is a genuine
           -- idempotent replay of the same OrderPlaced (no-op-equivalent
           -- write). Two attack shapes are refused:
           --   * terminal-status row → an isolated OrderPlaced replay
           --     cannot demote `FILLED` / `CANCELLED` / `REJECTED` back
           --     to OPEN;
           --   * partial-fill OPEN row → an isolated OrderPlaced replay
           --     cannot silently overwrite `amount_remaining` back to
           --     `amount_initial`, erasing the OrderFilled history.
           -- Either case surfaces at `warn!` (rows_affected = 0) so an
           -- operator-driven partial replay is diagnosable from logs.
           -- Data-bearing cutovers still wipe live_orders and reproject
           -- the full lifecycle in chain_order order; the WHERE guard
           -- does not affect that path because the row does not yet
           -- exist when OrderPlaced lands. See
           -- docs/migrations/orders-cancel-remainder-cutover.md.
           on conflict (orderbook_address, order_id) do update
               set outcome_id = excluded.outcome_id,
                   is_buy = excluded.is_buy,
                   price = excluded.price,
                   amount_initial = excluded.amount_initial,
                   amount_remaining = excluded.amount_remaining,
                   client_order_id = excluded.client_order_id,
                   status = 'OPEN',
                   last_chain_order = greatest(live_orders.last_chain_order,
                                               excluded.last_chain_order),
                   -- `chain_created_at` is the order's moment of birth and
                   -- must never move once set. `least(...)` would let a
                   -- replay carrying an earlier chain time pull the value
                   -- backward; pagination cursors and the API contract
                   -- both rely on the timestamp staying fixed. Use
                   -- `coalesce` for first-write-wins.
                   chain_created_at = coalesce(live_orders.chain_created_at,
                                               excluded.chain_created_at),
                   chain_updated_at = greatest(live_orders.chain_updated_at,
                                               excluded.chain_updated_at),
                   placed_chain_order = coalesce(live_orders.placed_chain_order,
                                                 excluded.placed_chain_order),
                   updated_at = now()
               where live_orders.status not in ('FILLED', 'CANCELLED', 'REJECTED')
                 and live_orders.amount_remaining = live_orders.amount_initial"#,
    )
    .bind(orderbook_address)
    .bind(&order_id)
    .bind(outcome_id)
    .bind(is_buy)
    .bind(&price)
    .bind(&amount)
    .bind(client_order_id)
    .bind(&chain_order)
    .bind(chain_seconds)
    .execute(&mut **tx)
    .await
    .context("upsert live_orders for OrderBook.OrderPlaced")?;

    // Postgres reports `rows_affected = 0` only when the conflict arm's
    // WHERE filter rejected the update — the INSERT path always counts 1,
    // and an unfiltered conflict path also counts 1. Zero here means an
    // OrderPlaced event hit either a terminal row or a partially-filled
    // OPEN row, and the projector refused to overwrite mutated state.
    // Surface it so the operator-replay path is diagnosable from logs
    // even though the projector still reports Applied (the event was
    // processed by being intentionally dropped).
    if result.rows_affected() == 0 {
        warn!(
            orderbook_address,
            order_id = %order_id,
            msg_id = %node.msg_id,
            chain_order = %chain_order,
            "OrderPlaced replay refused on mutated row (terminal status or partial fill); partial-replay cutover suspected",
        );
    }

    Ok(ProjectionOutcome::Applied)
}

async fn apply_order_placed_confirmed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    // `as_deref().context(...)` alone only catches `None`. An empty
    // string from the gateway would otherwise bind `owner_pn_address =
    // ""` into `live_orders` — a row no `/orders` query or
    // `resolve_for_cancel` predicate can ever match, leaving the order
    // stuck on the book with no API path to cancel.
    let owner_pn_address = node
        .src
        .as_deref()
        .filter(|s| !s.is_empty())
        .context("OrderPlacedConfirmed: src missing or empty on event message")?;
    let orderbook_address = field_str(&event.value, "orderBook")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;

    // Single statement: CTE locks the row, UPDATE attaches the owner
    // when the prior value was NULL (no-op otherwise), RETURNING yields
    // the prior owner so the caller can distinguish four outcomes:
    //   * no row returned → row not yet present → defer (the same
    //     batch's OrderPlaced has not arrived);
    //   * prior owner NULL → just attached on this UPDATE → Applied;
    //   * prior owner == incoming → idempotent retry → Applied + debug;
    //   * prior owner != incoming → misattribution attempt → Applied
    //     + error. Marked Applied because retry cannot self-resolve a
    //     misattribution; the `error!` line is the operator signal.
    // Mirrors the apply_order_filled / apply_order_cancelled pattern
    // (CTE + FOR UPDATE), so the row is row-locked across the read
    // and the conditional write — no race window between the two.
    let prior_owner: Option<Option<String>> = sqlx::query_scalar(
        r#"with prior as (
              select owner_pn_address
                from live_orders
               where orderbook_address = $1 and order_id = $2::numeric
               for update
           )
           update live_orders as lo
              set owner_pn_address = case
                                      when lo.owner_pn_address is null then $3
                                      else lo.owner_pn_address
                                  end,
                  updated_at = case
                                when lo.owner_pn_address is null then now()
                                else lo.updated_at
                            end
             from prior
            where lo.orderbook_address = $1 and lo.order_id = $2::numeric
            returning prior.owner_pn_address"#,
    )
    .bind(orderbook_address)
    .bind(&order_id)
    .bind(owner_pn_address)
    .fetch_optional(&mut **tx)
    .await
    .context("attach owner_pn_address on OrderPlacedConfirmed")?;

    match prior_owner {
        None => {
            warn!(
                orderbook_address,
                order_id = %order_id,
                owner_pn_address,
                msg_id = %node.msg_id,
                "OrderPlacedConfirmed observed before OrderPlaced; deferring"
            );
            Ok(ProjectionOutcome::Deferred)
        }
        Some(None) => Ok(ProjectionOutcome::Applied),
        Some(Some(persisted_owner)) if persisted_owner == owner_pn_address => {
            debug!(
                orderbook_address,
                order_id = %order_id,
                owner_pn_address,
                "OrderPlacedConfirmed idempotent retry; owner already attached"
            );
            Ok(ProjectionOutcome::Applied)
        }
        Some(Some(persisted_owner)) => {
            error!(
                orderbook_address,
                order_id = %order_id,
                persisted_owner = %persisted_owner,
                incoming_owner = %owner_pn_address,
                msg_id = %node.msg_id,
                "OrderPlacedConfirmed attribution conflict; refusing to overwrite"
            );
            Ok(ProjectionOutcome::Applied)
        }
    }
}

async fn apply_order_filled(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let orderbook_address =
        node.src.as_deref().context("OrderFilled: src missing on event message")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let filled_amount = uint_field_to_decimal(&event.value, "filledAmount")?;
    let chain_order = node_chain_order(node, "OrderFilled")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    if chain_seconds.is_none() {
        // For a non-terminal row this event advances `amount_remaining`,
        // `status`, and `last_chain_order`; `chain_updated_at` is left
        // alone because the gateway time is unparseable, so public
        // `updateTime` can lag behind the cursor state. For a terminal
        // prior row the SQL CASE guards below ignore the event entirely
        // (all four mutation columns are held).
        warn!(
            orderbook_address,
            order_id = %order_id,
            msg_id = %node.msg_id,
            created_at = ?node.created_at,
            "OrderFilled has no parseable chain time; public updateTime will remain stale on a non-terminal mutation",
        );
    }

    let prior: Option<(String, bool, bool)> = sqlx::query_as(
        r#"with prior as (
              select status, amount_remaining
                from live_orders
               where orderbook_address = $1 and order_id = $2::numeric
               for update
           )
           update live_orders as lo
              set amount_remaining = case
                                      when prior.status in ('FILLED', 'CANCELLED', 'REJECTED') then lo.amount_remaining
                                      else greatest(lo.amount_remaining - $3::numeric, 0::numeric)
                                  end,
                  status = case
                           when prior.status in ('FILLED', 'CANCELLED', 'REJECTED') then prior.status
                           -- Sentinel state: status='OPEN' with
                           -- amount_remaining=0 is a corrupt-row
                           -- shape (operator edit, legacy projector
                           -- residue) that `order_from_row` already
                           -- drops via the `fully_filled` guard.
                           -- Refuse to auto-heal it to FILLED — a
                           -- non-zero filled_amount would otherwise
                           -- satisfy `lo.amount_remaining - $3 <= 0`
                           -- and the row would surface as a fake
                           -- fully-filled order with executed_qty =
                           -- amount_initial.
                           when lo.amount_remaining = 0 then lo.status
                           when lo.amount_remaining - $3::numeric <= 0 then 'FILLED'
                           else lo.status
                       end,
                  last_chain_order = case
                                      when prior.status in ('FILLED', 'CANCELLED', 'REJECTED') then lo.last_chain_order
                                      else greatest(lo.last_chain_order, $4)
                                  end,
                  chain_updated_at = case
                                      when prior.status in ('FILLED', 'CANCELLED', 'REJECTED') then lo.chain_updated_at
                                      else greatest(lo.chain_updated_at, to_timestamp($5::double precision))
                                  end,
                  updated_at = now()
             from prior
            where lo.orderbook_address = $1 and lo.order_id = $2::numeric
            returning
                prior.status,
                (prior.status not in ('FILLED', 'CANCELLED', 'REJECTED')
                 and prior.amount_remaining > 0
                 and $3::numeric > prior.amount_remaining) as overshoot,
                (prior.status = 'OPEN' and prior.amount_remaining = 0) as sentinel"#,
    )
    .bind(orderbook_address)
    .bind(&order_id)
    .bind(&filled_amount)
    .bind(&chain_order)
    .bind(chain_seconds)
    .fetch_optional(&mut **tx)
    .await
    .context("apply OrderFilled")?;

    let Some((prior_status, overshoot, sentinel)) = prior else {
        warn!(
            orderbook_address,
            order_id = %order_id,
            msg_id = %node.msg_id,
            "OrderFilled observed before OrderPlaced; deferring"
        );
        return Ok(ProjectionOutcome::Deferred);
    };
    if matches!(prior_status.as_str(), "FILLED" | "CANCELLED" | "REJECTED") {
        warn!(
            orderbook_address,
            order_id = %order_id,
            filled_amount = %filled_amount,
            msg_id = %node.msg_id,
            chain_order = %chain_order,
            prior_status = %prior_status,
            "OrderFilled applied to terminal row; fill amount ignored"
        );
    } else if sentinel {
        // Corrupt-row shape (status='OPEN' with amount_remaining=0
        // before the fill). The SQL CASE refused to auto-heal the
        // status to 'FILLED' — surface it loudly so an operator can
        // investigate the source (manual edit, legacy projector
        // residue). The row stays invisible to /orders via the
        // existing `fully_filled` guard in `order_from_row`.
        warn!(
            orderbook_address,
            order_id = %order_id,
            filled_amount = %filled_amount,
            msg_id = %node.msg_id,
            chain_order = %chain_order,
            "OrderFilled applied to sentinel row (OPEN with amount_remaining=0); auto-heal to FILLED refused, fill amount ignored"
        );
    } else if overshoot {
        // filledAmount exceeded the row's (positive) remaining
        // quantity. The CASE arms above flip status to 'FILLED' and
        // clamp amount_remaining to 0, so the user-facing state is
        // correct — but the contract-side invariant `filledAmount <=
        // amount_remaining` was violated and the excess is lost.
        // Surface it: per-occurrence warn so operators can triage.
        warn!(
            orderbook_address,
            order_id = %order_id,
            filled_amount = %filled_amount,
            msg_id = %node.msg_id,
            "OrderFilled exceeded amount_remaining; clamping to FILLED and dropping excess"
        );
    }
    Ok(ProjectionOutcome::Applied)
}

async fn apply_order_cancelled(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let orderbook_address =
        node.src.as_deref().context("OrderCancelled: src missing on event message")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let chain_order = node_chain_order(node, "OrderCancelled")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    if chain_seconds.is_none() {
        // For a non-terminal row this event advances `status`,
        // `last_chain_order`, and `updated_at`; `chain_updated_at` is
        // left alone because the gateway time is unparseable. For a
        // terminal prior row the SQL CASE guards below ignore the
        // event entirely (all three mutation columns are held).
        warn!(
            orderbook_address,
            order_id = %order_id,
            msg_id = %node.msg_id,
            created_at = ?node.created_at,
            "OrderCancelled has no parseable chain time; public updateTime will remain stale on a non-terminal mutation",
        );
    }

    // `status` uses a CASE expression so an OrderCancelled that races a
    // terminal fill cannot demote a `FILLED` row to `CANCELLED`, and a
    // future rejected row cannot be rewritten by a late cancel. The chain
    // contract is supposed to prevent the filled race (see
    // docs/tech-specs/write-api.md §Response — "FILLED if matching raced
    // the cancel"), but the guard is cheap and keeps this path fail-closed
    // if contract ordering ever drifts. `amount_remaining` is intentionally
    // left unchanged so `executedQty` remains > 0 for partially-filled
    // canceled rows.
    // `last_chain_order` and `chain_updated_at` both mirror the status
    // CASE: an event the row's public state ignores must not move
    // `/orders` updateTime or `/depth` lastUpdateId for this row.
    let prior_status: Option<String> = sqlx::query_scalar(
        r#"with prior as (
              select status
                from live_orders
               where orderbook_address = $1 and order_id = $2::numeric
               for update
           )
           update live_orders as lo
              set status = case
                           when prior.status in ('FILLED', 'CANCELLED', 'REJECTED') then prior.status
                           else 'CANCELLED'
                       end,
                  last_chain_order = case
                                      when prior.status in ('FILLED', 'CANCELLED', 'REJECTED') then lo.last_chain_order
                                      else greatest(lo.last_chain_order, $3)
                                  end,
                  chain_updated_at = case
                                      when prior.status in ('FILLED', 'CANCELLED', 'REJECTED') then lo.chain_updated_at
                                      else greatest(lo.chain_updated_at, to_timestamp($4::double precision))
                                  end,
                  updated_at = now()
             from prior
            where lo.orderbook_address = $1 and lo.order_id = $2::numeric
            returning prior.status"#,
    )
    .bind(orderbook_address)
    .bind(&order_id)
    .bind(&chain_order)
    .bind(chain_seconds)
    .fetch_optional(&mut **tx)
    .await
    .context("apply OrderCancelled")?;

    if prior_status.is_none() {
        warn!(
            orderbook_address,
            order_id = %order_id,
            msg_id = %node.msg_id,
            "OrderCancelled observed before OrderPlaced; deferring"
        );
        return Ok(ProjectionOutcome::Deferred);
    }
    if matches!(prior_status.as_deref(), Some("FILLED" | "CANCELLED" | "REJECTED")) {
        warn!(
            orderbook_address,
            order_id = %order_id,
            prior_status = %prior_status.as_deref().unwrap_or(""),
            msg_id = %node.msg_id,
            chain_order = %chain_order,
            "OrderCancelled applied to terminal row; status preserved"
        );
    }
    Ok(ProjectionOutcome::Applied)
}

fn uint_field_to_decimal(value: &Value, key: &str) -> anyhow::Result<String> {
    let raw = field_str(value, key)?;
    if raw.starts_with("0x") || raw.starts_with("0X") {
        uint256_hex_to_decimal(raw)
    } else {
        // Decoder returns small uints (uint128 / uint64) as decimal strings.
        // Validate by re-parsing through BigUint to reject non-numerics.
        BigUint::parse_bytes(raw.as_bytes(), 10)
            .map(|b| b.to_str_radix(10))
            .ok_or_else(|| anyhow!("invalid uint field `{key}`: {raw}"))
    }
}

fn parse_u64_field(value: &Value, key: &str) -> anyhow::Result<i64> {
    let raw = field_str(value, key)?;
    raw.parse::<i64>().with_context(|| format!("parse {key} = {raw}"))
}

fn node_unix_seconds(node: &EventNode) -> Option<i64> {
    parse_unix_seconds(node.created_at.as_ref()).map(|v| v as i64)
}

/// Returns the strict-monotonic chain-order key (`msg_chain_order` from the
/// GraphQL gateway). Missing on a row that reaches the projector is an
/// invariant violation — `persist_page` drops events without it and
/// `pending_row_to_inputs` pulls it out of the NOT NULL column. Bubble up as
/// an error so the projector fails the row instead of silently writing a
/// stale value.
fn node_chain_order(node: &EventNode, event_label: &str) -> anyhow::Result<String> {
    node.msg_chain_order
        .clone()
        .with_context(|| format!("{event_label}: msg_chain_order missing on EventNode"))
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
