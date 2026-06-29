// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Projects InferenceOrderBook.* events into inference_markets / inference_orders.
// Mirrors projectors.rs conventions. Every handler that needs a parent row returns
// Deferred with ZERO writes when it is absent (the reprojection loop commits the
// batch tx even on Deferred — see indexer_repo.rs).

use anyhow::Context;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;
use crate::indexer_repo::parse_unix_seconds;
use crate::projectors::field_str;
use crate::projectors::node_chain_order;
use crate::projectors::uint_field_to_decimal;
use crate::projectors::ProjectionOutcome;

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
    let suffix =
        event.event_type.strip_prefix("InferenceOrderBook.").unwrap_or(event.event_type.as_str());
    match suffix {
        "InferenceOrderPlaced" => apply_inference_order_placed(tx, event, node).await,
        "InferenceSubscriptionPlaced" => apply_inference_subscription_placed(tx, event, node).await,
        "InferenceOrderCancelled" => apply_inference_order_cancelled(tx, event, node).await,
        "InferenceFilled" => apply_inference_filled(tx, event, node).await,
        "InferenceExecuted"
        | "InferenceRefunded"
        | "InferenceCycleForfeited"
        | "InferenceForfeitClaimed"
        | "InferenceOrderBookDeployed" => Ok(ProjectionOutcome::Applied),
        _ => Ok(ProjectionOutcome::Unknown),
    }
}

async fn seed_market_skeleton(
    tx: &mut Transaction<'_, Postgres>,
    _event: &DecodedEvent,
    node: &EventNode,
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
    ob: &str,
    order_id: &str,
    is_buy: bool,
    price: &str,
    ticks: &str,
    is_subscription: bool,
    note: Option<&str>,
    chain_order: &str,
    chain_seconds: Option<f64>,
) -> anyhow::Result<()> {
    let res = sqlx::query(
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

    // rows_affected = 0 only when the conflict arm's WHERE rejected the update:
    // the placement replay hit a terminal row, or a partially-filled OPEN row,
    // and the projector refused to overwrite mutated state. Surface it (the
    // handler still reports Applied — the event was processed by being
    // intentionally dropped) so an operator-replay cutover or out-of-order
    // delivery is diagnosable from logs. Mirrors projectors::apply_order_placed.
    if res.rows_affected() == 0 {
        warn!(
            orderbook_address = ob,
            order_id = %order_id,
            chain_order = %chain_order,
            is_subscription,
            "inference placement replay refused on mutated row (terminal status or partial fill); partial-replay cutover suspected",
        );
    }
    Ok(())
}

async fn apply_inference_order_placed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("OrderPlaced: src missing")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let is_buy = event
        .value
        .get("isBuy")
        .and_then(serde_json::Value::as_bool)
        .context("OrderPlaced: missing isBuy")?;
    let price = uint_field_to_decimal(&event.value, "price")?;
    let ticks = uint_field_to_decimal(&event.value, "ticks")?;
    let note = field_str(&event.value, "note").ok();
    let chain_order = node_chain_order(node, "InferenceOrderPlaced")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    upsert_resting_order(
        tx,
        ob,
        &order_id,
        is_buy,
        &price,
        &ticks,
        false,
        note,
        &chain_order,
        chain_seconds,
    )
    .await?;
    Ok(ProjectionOutcome::Applied)
}

async fn apply_inference_subscription_placed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("SubscriptionPlaced: src missing")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let price = uint_field_to_decimal(&event.value, "maxPrice")?;
    let ticks = uint_field_to_decimal(&event.value, "ticks")?;
    let note = field_str(&event.value, "buyerNote").ok();
    let chain_order = node_chain_order(node, "InferenceSubscriptionPlaced")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    upsert_resting_order(
        tx,
        ob,
        &order_id,
        true,
        &price,
        &ticks,
        true,
        note,
        &chain_order,
        chain_seconds,
    )
    .await?;
    Ok(ProjectionOutcome::Applied)
}

#[derive(sqlx::FromRow)]
struct LockedOrder {
    order_id: String,
    is_sweep_cancel: bool,
}

/// Parsed `Filled` event fields, shared by the normal projector and the
/// expired-orphan repair so both run the identical decrement.
struct FilledFields {
    ob: String,
    maker_id: String,
    taker_id: String,
    ticks: String,
    chain_order: String,
    chain_seconds: Option<f64>,
    /// `[maker_id, taker_id]` — the row ids the decrement touches.
    ids: Vec<String>,
    /// Deal-link fields (present on the normal `Filled`; unused by orphan repair).
    seller_tc: Option<String>,
    buyer_note: Option<String>,
}

impl FilledFields {
    fn parse(event: &DecodedEvent, node: &EventNode) -> anyhow::Result<Self> {
        let ob = node.src.as_deref().context("Filled: src missing")?.to_string();
        let maker_id = uint_field_to_decimal(&event.value, "makerId")?;
        let taker_id = uint_field_to_decimal(&event.value, "takerId")?;
        let ticks = uint_field_to_decimal(&event.value, "ticks")?;
        let chain_order = node_chain_order(node, "InferenceFilled")?;
        let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
        let seller_tc = field_str(&event.value, "sellerTC").ok().map(str::to_string);
        let buyer_note = field_str(&event.value, "buyerNote").ok().map(str::to_string);
        let ids = vec![maker_id.clone(), taker_id.clone()];
        Ok(Self {
            ob,
            maker_id,
            taker_id,
            ticks,
            chain_order,
            chain_seconds,
            ids,
            seller_tc,
            buyer_note,
        })
    }
}

/// Lock the maker/taker rows `FOR UPDATE` (whichever exist) and report whether
/// each was a provisional sweep-cancel.
async fn lock_filled_rows(
    tx: &mut Transaction<'_, Postgres>,
    ob: &str,
    ids: &[String],
) -> anyhow::Result<Vec<LockedOrder>> {
    sqlx::query_as(
        r#"select order_id::text as order_id, (swept_at is not null) as is_sweep_cancel
             from inference_orders
            where orderbook_address = $1 and order_id = any($2::numeric[])
            for update"#,
    )
    .bind(ob)
    .bind(ids)
    .fetch_all(&mut **tx)
    .await
    .context("Filled lock rows")
}

/// Apply the fill decrement to whichever of the named rows exist (`any($2)` only
/// touches present rows). The normal projector calls this once both legs are
/// present; the expired-orphan repair calls it for the present leg(s) when the
/// missing leg's `OrderPlaced` was dropped at capture and will never arrive.
async fn apply_filled_decrement(
    tx: &mut Transaction<'_, Postgres>,
    f: &FilledFields,
    locked: &[LockedOrder],
) -> anyhow::Result<()> {
    let any_sweep_override = locked.iter().any(|r| r.is_sweep_cancel);

    // Decrement present rows. Per-row CASE: FILLED and real-cancel (swept_at NULL) rows are
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
    .bind(&f.ob).bind(&f.ids).bind(&f.ticks).bind(&f.chain_order).bind(f.chain_seconds)
    .execute(&mut **tx).await.context("Filled decrement rows")?;

    // If we overrode a provisional sweep-cancel while the book is still in discovery,
    // reset the discovery sweep cursor so the reopened (lower) id is re-checked before
    // the visibility stamp (reconciler Queue A step 5).
    if any_sweep_override {
        // Reset the discovery cursor (restart the cycle from the bottom) AND bump the
        // monotonic override seq so the discovery completion stamp can detect this even
        // on a first-tick (prev_cursor = NULL) cycle — see the discovery sweep's
        // visibility-stamp guard in the inference reconciler.
        sqlx::query(
            r#"update inference_markets
                  set sweep_cursor = null,
                      sweep_override_seq = sweep_override_seq + 1,
                      updated_at = now()
                where orderbook_address = $1 and last_reconciled_at is null"#,
        )
        .bind(&f.ob)
        .execute(&mut **tx)
        .await
        .context("reset discovery sweep_cursor + bump override seq")?;
    }

    Ok(())
}

/// Records the deal link a `Filled` uniquely carries: the TokenContract
/// (`sellerTC`) ↔ its market (`orderbook_address`), seller note (the SELL leg,
/// `is_buy=false`), and buyer note (`buyerNote`). Upserts so the row survives
/// whether the deal was first seen here or via an earlier TokenContract.* event.
/// On a well-formed `Filled`, `sellerTC` is always present; its absence signals
/// ABI drift, so that one case alone is logged and skipped with `Ok(())` (not
/// `Err`): `apply_filled_decrement` has already mutated rows in this transaction,
/// and failing the event over a decoder/ABI mismatch would defer it forever. A
/// genuine DB error in the queries below still propagates — the reprojection
/// savepoint isolates and retries the event.
async fn link_deal_from_filled(
    tx: &mut Transaction<'_, Postgres>,
    f: &FilledFields,
) -> anyhow::Result<()> {
    let Some(seller_tc) = f.seller_tc.as_deref() else {
        warn!(
            orderbook_address = %f.ob,
            chain_order = %f.chain_order,
            "InferenceFilled event missing mandatory sellerTC field; inference_deals orderbook/seller link skipped — possible ABI drift"
        );
        return Ok(());
    };

    // Seller = the note on the SELL leg (is_buy=false) of this match.
    let seller_note: Option<String> = sqlx::query_scalar(
        r#"select note_address from inference_orders
            where orderbook_address = $1 and order_id = any($2::numeric[]) and is_buy = false
            limit 1"#,
    )
    .bind(&f.ob)
    .bind(&f.ids)
    .fetch_optional(&mut **tx)
    .await
    .context("resolve seller_note for deal link")?
    .flatten();

    sqlx::query(
        r#"insert into inference_deals
               (token_contract_address, orderbook_address, seller_note, buyer_note, last_chain_order)
           values ($1, $2, $3, $4, $5)
           on conflict (token_contract_address) do update
               set orderbook_address = coalesce(inference_deals.orderbook_address, excluded.orderbook_address),
                   seller_note = coalesce(inference_deals.seller_note, excluded.seller_note),
                   buyer_note = coalesce(inference_deals.buyer_note, excluded.buyer_note),
                   last_chain_order = greatest(coalesce(inference_deals.last_chain_order, ''), excluded.last_chain_order),
                   updated_at = now()"#,
    )
    .bind(seller_tc)
    .bind(&f.ob)
    .bind(seller_note)
    .bind(&f.buyer_note)
    .bind(&f.chain_order)
    .execute(&mut **tx)
    .await
    .context("link inference_deals from Filled")?;
    Ok(())
}

async fn apply_inference_filled(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let f = FilledFields::parse(event, node)?;

    // Lock both named rows. ZERO writes before we know both are present.
    let locked = lock_filled_rows(tx, &f.ob, &f.ids).await?;
    let present: std::collections::HashSet<&str> =
        locked.iter().map(|r| r.order_id.as_str()).collect();
    if !present.contains(f.maker_id.as_str()) || !present.contains(f.taker_id.as_str()) {
        return Ok(ProjectionOutcome::Deferred); // parent(s) not seen yet — zero writes
    }
    apply_filled_decrement(tx, &f, &locked).await?;
    link_deal_from_filled(tx, &f).await?;
    Ok(ProjectionOutcome::Applied)
}

/// What the dead-letter repair did to the read model for one expired inference
/// orphan, used to log the actual data consequence (and asserted in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpiredOrphanOutcome {
    /// A `Filled` orphan whose present resting leg(s) were decremented, so depth
    /// is corrected before the drop. `legs` is how many of maker/taker existed
    /// (1 in the usual taker-only-missing case). The missing counterparty's
    /// `OrderPlaced` was dropped at capture and is not recorded.
    FilledDepthRepaired { legs: usize },
    /// A `Filled` orphan with neither leg present (both `OrderPlaced` dropped):
    /// nothing to decrement.
    FilledNoLegPresent,
    /// An `OrderCancelled` orphan: the order to cancel was never placed (its
    /// `OrderPlaced` was dropped), so the authoritative cancel is lost. If a late
    /// placement re-opens the order the phantom sweep reconciles it.
    CancelLost,
    /// Any other inference event past cutoff with no resting row to repair.
    Nothing,
}

/// Best-effort read-model repair for an inference orphan that has exceeded the
/// dead-letter cutoff: at least one leg's parent `OrderPlaced` was dropped at
/// capture and will never arrive (so the projector returned `Deferred` forever).
/// For a `Filled`, decrement whichever resting leg IS present so its depth is
/// corrected rather than left permanently too-high (the phantom sweep cannot
/// heal a partial fill — it only cancels rows that read zero on chain). For an
/// `OrderCancelled` there is no row to cancel, so this only records the loss.
/// Emits one `warn` naming the actual consequence and returns the outcome.
pub async fn repair_expired_inference_orphan(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ExpiredOrphanOutcome> {
    let suffix =
        event.event_type.strip_prefix("InferenceOrderBook.").unwrap_or(event.event_type.as_str());
    let outcome = match suffix {
        "InferenceFilled" => {
            let f = FilledFields::parse(event, node)?;
            let locked = lock_filled_rows(tx, &f.ob, &f.ids).await?;
            let outcome = if locked.is_empty() {
                ExpiredOrphanOutcome::FilledNoLegPresent
            } else {
                apply_filled_decrement(tx, &f, &locked).await?;
                ExpiredOrphanOutcome::FilledDepthRepaired { legs: locked.len() }
            };
            // The Filled carries sellerTC + buyerNote; record the deal link even on
            // the orphan path (orderbook + buyer are leg-independent; seller resolves
            // from the SELL leg when present) — the normal deferred path never reruns.
            link_deal_from_filled(tx, &f).await?;
            outcome
        }
        "InferenceOrderCancelled" => ExpiredOrphanOutcome::CancelLost,
        _ => ExpiredOrphanOutcome::Nothing,
    };

    match &outcome {
        ExpiredOrphanOutcome::FilledDepthRepaired { legs } => warn!(
            msg_id = %node.msg_id, event_type = %event.event_type, legs,
            "inference Filled orphan past cutoff: decremented present resting leg(s) so depth stays correct; the missing counterparty's OrderPlaced was dropped at capture and is not recorded"
        ),
        ExpiredOrphanOutcome::FilledNoLegPresent => warn!(
            msg_id = %node.msg_id, event_type = %event.event_type,
            "inference Filled orphan past cutoff: neither leg present (both OrderPlaced dropped); nothing to repair"
        ),
        ExpiredOrphanOutcome::CancelLost => warn!(
            msg_id = %node.msg_id, event_type = %event.event_type,
            "inference OrderCancelled orphan past cutoff: authoritative cancel lost (its OrderPlaced was dropped); the phantom sweep reconciles it if a late placement re-opens the order"
        ),
        ExpiredOrphanOutcome::Nothing => warn!(
            msg_id = %node.msg_id, event_type = %event.event_type,
            "inference orphan past cutoff dead-lettered; no resting row to repair"
        ),
    }
    Ok(outcome)
}

async fn apply_inference_order_cancelled(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("OrderCancelled: src missing")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let chain_order = node_chain_order(node, "InferenceOrderCancelled")?;
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
