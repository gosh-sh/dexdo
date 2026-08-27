// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Projects InferenceOrderBook.* events into inference_markets / inference_orders.
// Mirrors projectors.rs conventions. Every handler that needs a parent row returns
// Deferred with ZERO writes when it is absent (the reprojection loop commits the
// batch tx even on Deferred — see indexer_repo.rs).

use anyhow::Context;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::error;
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;
use crate::indexer_repo::parse_unix_seconds;
use crate::projectors::field_str;
use crate::projectors::node_chain_order;
use crate::projectors::uint_field_to_decimal;
use crate::projectors::ProjectionOutcome;

/// The zero address an ABI decodes for an unset `address` field.
pub(crate) const ZERO_ADDRESS: &str =
    "0:0000000000000000000000000000000000000000000000000000000000000000";

/// Map the chain's "absent" encodings onto SQL NULL: a BUY placement carries the zero
/// address for `tokenContract`, and a resting SELL carries deadline 0.
pub(crate) fn non_zero_address(raw: Option<&str>) -> Option<&str> {
    raw.filter(|a| *a != ZERO_ADDRESS)
}

pub(crate) fn non_zero_uint(raw: Option<String>) -> Option<String> {
    raw.filter(|v| v != "0")
}

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
        "InferenceOrderExpired" => apply_inference_order_expired(tx, event, node).await,
        "InferenceFilled" => apply_inference_filled(tx, event, node).await,
        // Observability-only. `InferenceOrderCancelRejected` fires from `_doCancel`
        // when the cancel matched no resting order or came from a foreign owner —
        // by construction the book did not change, so there is no row to touch.
        // `InferenceOrderRejected` carries no `orderId` — the placement was refused
        // before anything rested, so there is no row to key on, same as
        // `InferenceOrderCancelRejected`.
        "InferenceExecuted"
        | "InferenceRefunded"
        | "InferenceOrderCancelRejected"
        | "InferenceOrderRejected"
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
    token_contract: Option<&str>,
    deadline: Option<&str>,
    chain_order: &str,
    chain_seconds: Option<f64>,
) -> anyhow::Result<()> {
    let res = sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                is_subscription, note_address, token_contract, deadline, status, last_chain_order,
                chain_created_at, chain_updated_at, updated_at)
           values ($1, $2::numeric, $3, $4::numeric, $5::numeric, $5::numeric,
                   $6, $7, $8, $9::numeric, 'OPEN', $10,
                   to_timestamp($11::double precision), to_timestamp($11::double precision), now())
           on conflict (orderbook_address, order_id) do update
               set is_buy = excluded.is_buy,
                   price = excluded.price,
                   amount_initial = excluded.amount_initial,
                   amount_remaining = excluded.amount_remaining,
                   is_subscription = excluded.is_subscription,
                   note_address = excluded.note_address,
                   -- NULL-preserving: a replayed SubscriptionPlaced carries no deadline,
                   -- and neither may erase a value the reconciler recovered from chain.
                   token_contract = coalesce(excluded.token_contract, inference_orders.token_contract),
                   deadline = coalesce(excluded.deadline, inference_orders.deadline),
                   status = 'OPEN',
                   last_chain_order = greatest(inference_orders.last_chain_order, excluded.last_chain_order),
                   chain_created_at = coalesce(inference_orders.chain_created_at, excluded.chain_created_at),
                   chain_updated_at = greatest(inference_orders.chain_updated_at, excluded.chain_updated_at),
                   updated_at = now()
               where inference_orders.status not in ('FILLED','CANCELLED')
                 and inference_orders.amount_remaining = inference_orders.amount_initial"#,
    )
    .bind(ob).bind(order_id).bind(is_buy).bind(price).bind(ticks)
    .bind(is_subscription).bind(note).bind(token_contract).bind(deadline)
    .bind(chain_order).bind(chain_seconds)
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
    // `note` is mandatory in the ABI and the endpoint filters exactly on it; a NULL
    // would hide the order from every `note=X` listing forever, since the sweep
    // repairs only `token_contract` and `deadline`.
    let note = Some(field_str(&event.value, "note")?);
    // `tokenContract` and `deadline` are mandatory in the ABI too — a BUY carries the
    // zero address and a resting SELL carries deadline 0, but neither field is ever
    // absent. Decode strictly and normalize only a successfully decoded zero to NULL:
    // `.ok()` would map ABI/decoder drift onto a NULL insert and still create the row,
    // and nothing would ever repair it once it fills or is cancelled before a sweep.
    let token_contract: Option<&str> =
        non_zero_address(Some(field_str(&event.value, "tokenContract")?));
    let deadline: Option<String> =
        non_zero_uint(Some(uint_field_to_decimal(&event.value, "deadline")?));
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
        token_contract,
        deadline.as_deref(),
        &chain_order,
        chain_seconds,
    )
    .await?;

    // A FRESH SELL ON A FUNDED DEAL MEANS THE PREVIOUS MATCH IS OVER. Since
    // contracts 4.0.36 a buyer no-show runs `cleanupUnopened`, which no longer
    // destroys the deal, so the row would otherwise keep the dead match's buyer
    // and deposit and read as "funded, never opened".
    //
    // This used to be the ONLY thing on chain that said so, because that call
    // emitted nothing. It is not any more: the 4.0.36 update has the deal tell
    // both notes (`onDealClosed`), and each note emits
    // `PrivateNote.InferenceDealClosed` — projected in `projectors.rs` and now
    // the direct signal. This one stays as the backstop, and it is the one that
    // covers a deal whose notes were minted before that update.
    //
    // The inference is sound rather than merely plausible: `postFromNote` opens
    // with `if (_offerPosted || _funded) { return; }`, and a funded deal never
    // clears `_offerPosted` (`onSellClosed` returns early while `_funded`). So a
    // deal that is funded CANNOT put a new ask up, and an ask that reaches the
    // book naming it proves the funding was undone.
    //
    // Only here, not in `apply_inference_subscription_placed`: a subscription is
    // the buyer's side and never names a seller's deal. The guard would hold
    // there too — it asks for a SELL carrying a TokenContract — but the call
    // would be dead weight.
    if !is_buy && let Some(tc) = token_contract {
        crate::token_contract_projectors::end_funding_cycle(tx, tc, &chain_order).await?;
    }
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
    // `buyerNote` is mandatory in the ABI and the endpoint filters exactly on it; a NULL
    // would hide the subscription from every `note=X` listing forever.
    let note = Some(field_str(&event.value, "buyerNote")?);
    let chain_order = node_chain_order(node, "InferenceSubscriptionPlaced")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    // InferenceSubscriptionPlaced carries no tokenContract (a subscription is a bid) and
    // no deadline, though the chain stores one. The reconciler's getter probe is the only
    // source for a subscription row's deadline.
    upsert_resting_order(
        tx,
        ob,
        &order_id,
        true,
        &price,
        &ticks,
        true,
        note,
        None,
        None,
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
    /// The leg's side. `isBuyerMaker` is not carried by `InferenceFilled`, so the tape
    /// takes it from whichever leg is locked here.
    is_buy: bool,
}

/// Parsed `Filled` event fields, shared by the normal projector and the
/// expired-orphan repair so both run the identical decrement.
struct FilledFields {
    ob: String,
    maker_id: String,
    taker_id: String,
    ticks: String,
    clearing_price: String,
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
        let clearing_price = uint_field_to_decimal(&event.value, "clearingPrice")?;
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
            clearing_price,
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
        r#"select order_id::text as order_id, (swept_at is not null) as is_sweep_cancel, is_buy
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
                   -- The buyer is the one field here that a SECOND match at the same
                   -- deal address changes, and since contracts 4.0.36 a second match is
                   -- ordinary: `cleanupUnopened` no longer destroys the deal, so a
                   -- no-show returns the same `TokenContract` to the book and the next
                   -- fill names a different buyer. First-wins would pin the row to a
                   -- buyer who never funded it. Newest-wins by `last_chain_order`, so a
                   -- REPLAY of an older fill (reprojection) still cannot walk it back,
                   -- and `coalesce` on both branches keeps an orphan-repair Filled —
                   -- which carries no `buyerNote` — from blanking a known one.
                   -- `orderbook_address` and `seller_note` need none of this: the deal
                   -- address derives from the seller's key and nonce, so neither can
                   -- change while the address does not.
                   buyer_note = case
                       when excluded.last_chain_order > coalesce(inference_deals.last_chain_order, '')
                           then coalesce(excluded.buyer_note, inference_deals.buyer_note)
                       else coalesce(inference_deals.buyer_note, excluded.buyer_note)
                   end,
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

/// `isBuyerMaker` for the tape. `InferenceFilled` does not carry it, so it is read off the
/// locked rows: the MAKER leg's own side when that leg is present, otherwise the inverse of
/// the taker's (a match has exactly one buyer and one seller). `None` only when neither leg
/// is in the read model — the orphan-repair case where both `InferenceOrderPlaced` events
/// were dropped at capture, and the direction is unrecoverable.
fn resolve_is_buyer_maker(f: &FilledFields, locked: &[LockedOrder]) -> Option<bool> {
    if let Some(maker) = locked.iter().find(|r| r.order_id == f.maker_id) {
        return Some(maker.is_buy);
    }
    locked.iter().find(|r| r.order_id == f.taker_id).map(|taker| !taker.is_buy)
}

/// Append the match to the public tape. Idempotent under reprojection: a replay conflicts on
/// `trade_id` and only coalesces a NULL `chain_time` (first write wins). The `where` clause
/// makes that coalesce conditional on every immutable column matching, so a divergent
/// conflict (drifted payload, or a gateway bug duplicating msg_chain_order) is skipped and
/// error!-logged instead of silently overwriting — mirroring the prediction tape write in
/// `projectors::apply_order_filled`.
async fn append_inference_trade(
    tx: &mut Transaction<'_, Postgres>,
    f: &FilledFields,
    is_buyer_maker: bool,
) -> anyhow::Result<()> {
    if f.chain_seconds.is_none() {
        // error!, not warn!: the public trade stays invisible until an operator repairs the
        // row. A stale chain_updated_at heals itself on the next event; this does not.
        error!(
            orderbook_address = %f.ob,
            trade_id = %f.chain_order,
            "InferenceFilled has no parseable chain time; the tape row lands with NULL chain_time, hidden from /api/v1/inference/trades until repaired (data-schema.md#inference_trades)",
        );
    }
    let result = sqlx::query(
        r#"insert into inference_trades
               (trade_id, orderbook_address, price, qty, is_buyer_maker, chain_time)
           values ($1, $2, $3::numeric, $4::numeric, $5,
                   to_timestamp($6::double precision))
           on conflict (trade_id) do update
               set chain_time = coalesce(inference_trades.chain_time, excluded.chain_time)
             where inference_trades.orderbook_address = excluded.orderbook_address
               and inference_trades.price = excluded.price
               and inference_trades.qty = excluded.qty
               and inference_trades.is_buyer_maker = excluded.is_buyer_maker"#,
    )
    .bind(&f.chain_order)
    .bind(&f.ob)
    .bind(&f.clearing_price)
    .bind(&f.ticks)
    .bind(is_buyer_maker)
    .bind(f.chain_seconds)
    .execute(&mut **tx)
    .await
    .context("append inference trade")?;
    if result.rows_affected() == 0 {
        error!(
            orderbook_address = %f.ob,
            trade_id = %f.chain_order,
            clearing_price = %f.clearing_price,
            ticks = %f.ticks,
            is_buyer_maker,
            "InferenceFilled conflicts with an existing tape row but diverges on an immutable column (book/price/qty/side); first write kept, replay ignored",
        );
    }
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
    // Both legs are present on this path (checked above), so the direction always resolves.
    // The append is not gated on what apply_filled_decrement did to the order rows: a
    // FULL no-op there (terminal maker, real-cancel override) still leaves a genuine
    // on-chain match, so the tape mirrors the InferenceFilled event one-to-one — same
    // reasoning as the prediction tape's unconditional trades insert.
    if let Some(is_buyer_maker) = resolve_is_buyer_maker(&f, &locked) {
        append_inference_trade(tx, &f, is_buyer_maker).await?;
    }
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
            // Record the match even though a leg is missing: the direction is recoverable
            // from whichever leg IS present. With neither leg present it is not, so the
            // match stays off the tape rather than landing with a guessed side.
            match resolve_is_buyer_maker(&f, &locked) {
                Some(is_buyer_maker) => append_inference_trade(tx, &f, is_buyer_maker).await?,
                None => warn!(
                    orderbook_address = %f.ob,
                    trade_id = %f.chain_order,
                    "inference Filled orphan past cutoff: neither leg present, trade direction unrecoverable; match omitted from the public tape",
                ),
            }
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

/// The book removed a resting order because its deadline passed. Terminal, and
/// shaped like the event-cancel below: the CTE locks the row so RETURNING tells
/// present from absent.
///
/// `takes_expiry` decides who wins when the row is already terminal. A provisional
/// sweep-cancel (`swept_at` NOT NULL) yields — the sweep only ever guessed CANCELLED
/// because the order had vanished from the book, which is exactly what expiry does,
/// so the event corrects it and clears `swept_at`. A FILLED row or a real event-cancel
/// (`swept_at` NULL) stands: the order left the book before it could age out.
async fn apply_inference_order_expired(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("OrderExpired: src missing")?;
    let order_id = uint_field_to_decimal(&event.value, "orderId")?;
    let chain_order = node_chain_order(node, "InferenceOrderExpired")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    let prior: Option<(String,)> = sqlx::query_as(
        r#"with prior as (
               select status,
                      (status = 'OPEN'
                       or (status = 'CANCELLED' and swept_at is not null)) as takes_expiry
                 from inference_orders
                where orderbook_address = $1 and order_id = $2::numeric for update)
           update inference_orders o
              set status = case when prior.takes_expiry then 'EXPIRED' else prior.status end,
                  swept_at = case when prior.takes_expiry then null else o.swept_at end,
                  last_chain_order = case when prior.takes_expiry
                                          then greatest(o.last_chain_order, $3)
                                          else o.last_chain_order end,
                  chain_updated_at = case when prior.takes_expiry
                                          then greatest(o.chain_updated_at, to_timestamp($4::double precision))
                                          else o.chain_updated_at end,
                  updated_at = now()
             from prior
            where o.orderbook_address = $1 and o.order_id = $2::numeric
            returning prior.status"#,
    )
    .bind(ob).bind(&order_id).bind(&chain_order).bind(chain_seconds)
    .fetch_optional(&mut **tx).await.context("inference OrderExpired update")?;

    match prior {
        None => Ok(ProjectionOutcome::Deferred), // parent OrderPlaced not seen yet
        Some(_) => Ok(ProjectionOutcome::Applied),
    }
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
