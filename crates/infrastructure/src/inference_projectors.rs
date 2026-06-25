// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Projects InferenceOrderBook.* events into inference_markets / inference_orders.
// Mirrors projectors.rs conventions. Every handler that needs a parent row returns
// Deferred with ZERO writes when it is absent (the reprojection loop commits the
// batch tx even on Deferred — see indexer_repo.rs).

use anyhow::Context;
use sqlx::{Postgres, Transaction};
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;
use crate::indexer_repo::parse_unix_seconds;
use crate::projectors::{field_str, node_chain_order, uint_field_to_decimal, ProjectionOutcome};

pub async fn project_inference_event(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    // Shared discovery pre-step: seed a market skeleton for ANY inference event on
    // an unknown book, BEFORE event-specific handling. Backstop for out-of-order /
    // gap delivery; under from-genesis the first event is always a placement.
    seed_market_skeleton(tx, event, node).await?;

    // Route by the PERSISTED event_type suffix, NOT event_name. The reprojection
    // loop's `pending_row_to_inputs` rebuilds DecodedEvent with event_name EMPTY
    // (only event_type is stored in raw_events), so matching on event_name would
    // send every live captured row to the seed-only path. event_type is set in both
    // the live loop and the direct tests.
    let suffix = event.event_type.strip_prefix("InferenceOrderBook.").unwrap_or(event.event_type.as_str());
    match suffix {
        "OrderPlaced" => apply_inference_order_placed(tx, event, node).await,
        "SubscriptionPlaced" => apply_inference_subscription_placed(tx, event, node).await,
        "OrderCancelled" => apply_inference_order_cancelled(tx, event, node).await,
        "Filled" => apply_inference_filled(tx, event, node).await,
        "Executed" | "Refunded" | "CycleForfeited" | "ForfeitClaimed" => Ok(ProjectionOutcome::Applied),
        other => {
            warn!(event_type = %event.event_type, other, "unknown InferenceOrderBook event; seeded only");
            Ok(ProjectionOutcome::Applied)
        }
    }
}

async fn seed_market_skeleton(
    tx: &mut Transaction<'_, Postgres>, _event: &DecodedEvent, node: &EventNode,
) -> anyhow::Result<()> {
    let ob = node.src.as_deref().context("inference event: src missing")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    sqlx::query(
        r#"insert into inference_markets (orderbook_address, created_at_chain)
           values ($1, to_timestamp($2::double precision))
           on conflict (orderbook_address) do nothing"#,
    )
    .bind(ob)
    .bind(chain_seconds)
    .execute(&mut **tx)
    .await
    .context("seed inference_markets skeleton")?;
    Ok(())
}

// Shared resting-order upsert for OrderPlaced (is_subscription=false) and
// SubscriptionPlaced (is_subscription=true). Same still-fresh conflict guard as
// projectors::apply_order_placed: a replay onto a closed or partially-filled-OPEN
// row is a no-op, so it never resets amount_remaining and corrupts depth.
#[allow(clippy::too_many_arguments)]
async fn upsert_resting_order(
    tx: &mut Transaction<'_, Postgres>,
    ob: &str, order_id: &str, is_buy: bool, price: &str, ticks: &str,
    is_subscription: bool, note: Option<&str>, chain_order: &str, chain_seconds: Option<f64>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                is_subscription, note_address, status, last_chain_order,
                chain_created_at, chain_updated_at, updated_at)
           values ($1, $2::numeric, $3, $4::numeric, $5::numeric, $5::numeric,
                   $6, $7, 'OPEN', $8,
                   to_timestamp($9::double precision), to_timestamp($9::double precision), now())
           on conflict (orderbook_address, order_id) do update
               set is_buy = excluded.is_buy,
                   price = excluded.price,
                   amount_initial = excluded.amount_initial,
                   amount_remaining = excluded.amount_remaining,
                   is_subscription = excluded.is_subscription,
                   note_address = excluded.note_address,
                   status = 'OPEN',
                   last_chain_order = greatest(inference_orders.last_chain_order, excluded.last_chain_order),
                   chain_created_at = coalesce(inference_orders.chain_created_at, excluded.chain_created_at),
                   chain_updated_at = greatest(inference_orders.chain_updated_at, excluded.chain_updated_at),
                   updated_at = now()
               where inference_orders.status not in ('FILLED','CANCELLED')
                 and inference_orders.amount_remaining = inference_orders.amount_initial"#,
    )
    .bind(ob).bind(order_id).bind(is_buy).bind(price).bind(ticks)
    .bind(is_subscription).bind(note).bind(chain_order).bind(chain_seconds)
    .execute(&mut **tx).await.context("upsert inference_orders resting")?;
    Ok(())
}

async fn apply_inference_order_placed(
    tx: &mut Transaction<'_, Postgres>, event: &DecodedEvent, node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("OrderPlaced: src missing")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let is_buy = event.value.get("isBuy").and_then(serde_json::Value::as_bool)
        .context("OrderPlaced: missing isBuy")?;
    let price = uint_field_to_decimal(&event.value, "price")?;
    let ticks = uint_field_to_decimal(&event.value, "ticks")?;
    let note = field_str(&event.value, "note").ok();
    let chain_order = node_chain_order(node, "OrderPlaced")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    upsert_resting_order(tx, ob, &order_id, is_buy, &price, &ticks, false, note, &chain_order, chain_seconds).await?;
    Ok(ProjectionOutcome::Applied)
}

async fn apply_inference_subscription_placed(
    tx: &mut Transaction<'_, Postgres>, event: &DecodedEvent, node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("SubscriptionPlaced: src missing")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let price = uint_field_to_decimal(&event.value, "maxPrice")?;
    let ticks = uint_field_to_decimal(&event.value, "ticks")?;
    let note = field_str(&event.value, "buyerNote").ok();
    let chain_order = node_chain_order(node, "SubscriptionPlaced")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    upsert_resting_order(tx, ob, &order_id, true, &price, &ticks, true, note, &chain_order, chain_seconds).await?;
    Ok(ProjectionOutcome::Applied)
}

#[derive(sqlx::FromRow)]
struct LockedOrder { order_id: String, is_sweep_cancel: bool }

async fn apply_inference_filled(
    tx: &mut Transaction<'_, Postgres>, event: &DecodedEvent, node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("Filled: src missing")?;
    let maker_id = uint_field_to_decimal(&event.value, "makerId")?;
    let taker_id = uint_field_to_decimal(&event.value, "takerId")?;
    let ticks = uint_field_to_decimal(&event.value, "ticks")?;
    let chain_order = node_chain_order(node, "Filled")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());

    let ids = vec![maker_id.clone(), taker_id.clone()];

    // Lock both named rows. ZERO writes before we know both are present.
    let locked: Vec<LockedOrder> = sqlx::query_as(
        r#"select order_id::text as order_id, (swept_at is not null) as is_sweep_cancel
             from inference_orders
            where orderbook_address = $1 and order_id = any($2::numeric[])
            for update"#,
    )
    .bind(ob).bind(&ids)
    .fetch_all(&mut **tx).await.context("Filled lock both rows")?;

    let present: std::collections::HashSet<&str> = locked.iter().map(|r| r.order_id.as_str()).collect();
    if !present.contains(maker_id.as_str()) || !present.contains(taker_id.as_str()) {
        return Ok(ProjectionOutcome::Deferred); // parent(s) not seen yet — zero writes
    }
    let any_sweep_override = locked.iter().any(|r| r.is_sweep_cancel);

    // Decrement both. Per-row CASE: FILLED and real-cancel (swept_at NULL) rows are
    // terminal no-ops; a SELL offer (is_buy=false) closes on the first fill; a sweep-
    // cancel (swept_at NOT NULL) is overridden (swept_at cleared, decremented).
    sqlx::query(
        r#"update inference_orders o
              set amount_remaining = case
                      when o.status = 'FILLED' then o.amount_remaining
                      when o.status = 'CANCELLED' and o.swept_at is null then o.amount_remaining
                      else greatest(o.amount_remaining - $3::numeric, 0::numeric)
                  end,
                  status = case
                      when o.status = 'FILLED' then 'FILLED'
                      when o.status = 'CANCELLED' and o.swept_at is null then 'CANCELLED'
                      when o.is_buy = false then 'FILLED'
                      when greatest(o.amount_remaining - $3::numeric, 0::numeric) = 0 then 'FILLED'
                      else 'OPEN'
                  end,
                  swept_at = case
                      when o.status = 'FILLED' then o.swept_at
                      when o.status = 'CANCELLED' and o.swept_at is null then o.swept_at
                      else null
                  end,
                  -- A terminal row (FILLED, or real-cancel CANCELLED+swept_at NULL) is a
                  -- FULL no-op: a late/duplicate Filled must not advance its chain/bookkeeping
                  -- columns either (mirrors projectors::apply_order_filled). Only OPEN rows
                  -- and provisional sweep-cancels (swept_at NOT NULL, being overridden) mutate.
                  last_chain_order = case
                      when o.status = 'FILLED' or (o.status = 'CANCELLED' and o.swept_at is null) then o.last_chain_order
                      else greatest(o.last_chain_order, $4)
                  end,
                  chain_updated_at = case
                      when o.status = 'FILLED' or (o.status = 'CANCELLED' and o.swept_at is null) then o.chain_updated_at
                      else greatest(o.chain_updated_at, to_timestamp($5::double precision))
                  end,
                  updated_at = case
                      when o.status = 'FILLED' or (o.status = 'CANCELLED' and o.swept_at is null) then o.updated_at
                      else now()
                  end
            where o.orderbook_address = $1 and o.order_id = any($2::numeric[])"#,
    )
    .bind(ob).bind(&ids).bind(&ticks).bind(&chain_order).bind(chain_seconds)
    .execute(&mut **tx).await.context("Filled decrement both rows")?;

    // If we overrode a provisional sweep-cancel while the book is still in discovery,
    // reset the discovery sweep cursor so the reopened (lower) id is re-checked before
    // the visibility stamp (reconciler Queue A step 5).
    if any_sweep_override {
        // Reset the discovery cursor (restart the cycle from the bottom) AND bump the
        // monotonic override seq so the discovery completion stamp can detect this even
        // on a first-tick (prev_cursor = NULL) cycle — see reconciler Task 9.
        sqlx::query(
            r#"update inference_markets
                  set sweep_cursor = null,
                      sweep_override_seq = sweep_override_seq + 1,
                      updated_at = now()
                where orderbook_address = $1 and last_reconciled_at is null"#,
        )
        .bind(ob).execute(&mut **tx).await.context("reset discovery sweep_cursor + bump override seq")?;
    }

    Ok(ProjectionOutcome::Applied)
}

async fn apply_inference_order_cancelled(
    tx: &mut Transaction<'_, Postgres>, event: &DecodedEvent, node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("OrderCancelled: src missing")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let chain_order = node_chain_order(node, "OrderCancelled")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    // CTE locks the row; the UPDATE always matches it (so RETURNING distinguishes
    // present-from-absent), the CASE keeps a FILLED row terminal. swept_at -> NULL
    // makes this an authoritative event-cancel (overriding any provisional sweep).
    let prior: Option<(String,)> = sqlx::query_as(
        r#"with prior as (
               select status from inference_orders
                where orderbook_address = $1 and order_id = $2::numeric for update)
           update inference_orders o
              set status = case when prior.status = 'FILLED' then 'FILLED' else 'CANCELLED' end,
                  swept_at = case when prior.status = 'FILLED' then o.swept_at else null end,
                  last_chain_order = case when prior.status = 'FILLED' then o.last_chain_order
                                          else greatest(o.last_chain_order, $3) end,
                  chain_updated_at = case when prior.status = 'FILLED' then o.chain_updated_at
                                          else greatest(o.chain_updated_at, to_timestamp($4::double precision)) end,
                  updated_at = now()
             from prior
            where o.orderbook_address = $1 and o.order_id = $2::numeric
            returning prior.status"#,
    )
    .bind(ob).bind(&order_id).bind(&chain_order).bind(chain_seconds)
    .fetch_optional(&mut **tx).await.context("inference OrderCancelled update")?;

    match prior {
        None => Ok(ProjectionOutcome::Deferred), // parent OrderPlaced not seen yet
        Some(_) => Ok(ProjectionOutcome::Applied),
    }
}
