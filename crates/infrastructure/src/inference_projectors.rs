// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Projects InferenceOrderBook.* events into inference_markets / inference_orders.
// Mirrors projectors.rs conventions. Every handler that needs a parent row returns
// Deferred with ZERO writes when it is absent (the reprojection loop commits the
// batch tx even on Deferred — see indexer_repo.rs).

use anyhow::Context;
use dodex_contracts::airegistry::inference_order_book_events::FilledData;
use dodex_contracts::airegistry::inference_order_book_events::InferenceOrderBookEvent;
use dodex_contracts::airegistry::inference_order_book_events::OrderPlacedData;
use dodex_contracts::airegistry::inference_order_book_events::RefundedData;
use dodex_contracts::airegistry::inference_order_book_events::BPS_DENOMINATOR;
use dodex_contracts::airegistry::inference_order_book_events::PLATFORM_FEE_BPS;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::error;
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;
use crate::indexer_repo::parse_unix_seconds;
use crate::projectors::node_chain_order;
use crate::projectors::uint256_maybe_hex;
use crate::projectors::uint_field_to_decimal;
use crate::projectors::ProjectionOutcome;

/// The zero address an ABI decodes for an unset `address` field.
pub(crate) const ZERO_ADDRESS: &str =
    "0:0000000000000000000000000000000000000000000000000000000000000000";

/// Map the chain's "absent" encodings onto SQL NULL: a BUY placement carries the zero
/// address for `tokenContract`, and a GTC BUY carries deadline 0. Not a SELL — the
/// contract rejects a zero-deadline offer as malformed, so a NULL `deadline` on a SELL
/// row is a gap the reconciler probe fills, never an intentional value.
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
    // Route by the enum VARIANT, not by a string literal. The variant is pinned
    // to the ABI (`inference_order_book_enum_covers_every_abi_event`), so an arm
    // naming an event that does not exist stops being expressible — that is how
    // `InferenceSubscriptionPlaced` and `StreamReclaimed` lived on as dead arms.
    // The `match` is exhaustive WITHOUT `_`: a new variant will not compile until
    // someone assigns it an outcome.
    use InferenceOrderBookEvent as E;
    let Some(kind) = E::ALL.iter().copied().find(|v| {
        let n = format!("{v:?}");
        let n = if n.starts_with("Inference") { n } else { format!("Inference{n}") };
        n == suffix
    }) else {
        return Ok(ProjectionOutcome::Unknown);
    };
    match kind {
        E::OrderPlaced => apply_inference_order_placed(tx, event, node).await,
        E::OrderCancelled => apply_inference_order_cancelled(tx, event, node).await,
        E::OrderExpired => apply_inference_order_expired(tx, event, node).await,
        E::Filled => apply_inference_filled(tx, event, node).await,
        // Observability-only. `InferenceOrderCancelRejected` fires from `_doCancel`
        // when the cancel matched no resting order or came from a foreign owner —
        // by construction the book did not change, so there is no row to touch.
        // `InferenceOrderRejected` carries no `orderId` — the placement was refused
        // before anything rested, so there is no row to key on, same as
        // `InferenceOrderCancelRejected`.
        E::Refunded => apply_inference_refunded(tx, event, node).await,
        E::Executed | E::OrderCancelRejected | E::OrderRejected | E::InferenceOrderBookDeployed => {
            Ok(ProjectionOutcome::Applied)
        }
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

// Shared resting-order upsert. `is_subscription` comes from the FLAG_SUBSCRIPTION
// bit of the placement event's `flags` field — the ABI carries no separate
// subscription event.
// Same still-fresh conflict guard as
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
                   -- NULL-preserving: a replayed placement may carry NULL where the
                   -- reconciler has since recovered a value from chain, and a replay
                   -- may never erase it.
                   token_contract = coalesce(excluded.token_contract, inference_orders.token_contract),
                   deadline = coalesce(excluded.deadline, inference_orders.deadline),
                   status = 'OPEN',
                   last_chain_order = greatest(inference_orders.last_chain_order, excluded.last_chain_order),
                   chain_created_at = coalesce(inference_orders.chain_created_at, excluded.chain_created_at),
                   chain_updated_at = greatest(inference_orders.chain_updated_at, excluded.chain_updated_at),
                   updated_at = now()
               where inference_orders.status not in ('FILLED','CANCELLED','EXPIRED')
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
    // Exhaustive destructuring: every ABI field must be named.
    let OrderPlacedData { order_id, is_buy, price, ticks, note, token_contract, deadline, flags } =
        serde_json::from_value(event.value.clone())
            .context("InferenceOrderPlaced: payload does not parse against the ABI")?;
    let order_id = order_id.to_string();
    // `price` is a uint256, see `FilledFields::parse`.
    let price = uint256_maybe_hex(&price)?;
    let ticks = ticks.to_string();
    let note = Some(note.as_str());
    let token_contract: Option<&str> = non_zero_address(Some(token_contract.as_str()));
    let deadline: Option<String> = non_zero_uint(Some(deadline.to_string()));
    // `flags` is the event's 8th parameter as of v4.0.33. A subscription is the
    // FLAG_SUBSCRIPTION bit (0x40, contracts/airegistry/modifiers/modifiers.sol),
    // not an event of its own.
    const FLAG_SUBSCRIPTION: u8 = 0x40;
    let is_subscription = flags & FLAG_SUBSCRIPTION != 0;
    let chain_order = node_chain_order(node, "InferenceOrderPlaced")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    upsert_resting_order(
        tx,
        ob,
        &order_id,
        is_buy,
        &price,
        &ticks,
        is_subscription,
        note,
        token_contract,
        deadline.as_deref(),
        &chain_order,
        chain_seconds,
    )
    .await?;
    // A SELL offer is the placement that REGISTERS a deal: `token_contract` is the
    // TokenContract the seller deployed, and `price` is the per-tick ask its
    // constructor was given (`TokenContract.sol:308`). That is the same number
    // `StreamOpened.pricePerTick` used to report, so it is the column's value and
    // not an approximation of it.
    //
    // NOT `InferenceFilled.clearingPrice`, which is the nearest-looking
    // alternative and is wrong: clearing is the price the MATCH struck. It equals
    // the ask only when the seller was the maker; a taker SELL crossing a resting
    // BUY clears at the bid.
    if let Some(tc) = token_contract.filter(|_| !is_buy) {
        record_deal_price(tx, tc, &price, &chain_order).await?;
    }
    Ok(ProjectionOutcome::Applied)
}

/// Seed or enrich the deal row with the per-tick ask its SELL offer carried.
///
/// Upsert rather than update: the placement precedes the fill, so on a normal
/// timeline this is the FIRST thing that knows the deal exists at all — before
/// `InferenceFilled` links its parties. `coalesce` keeps whatever a re-listing
/// or a replay already recorded, so the first price to arrive is the one that
/// stays.
async fn record_deal_price(
    tx: &mut Transaction<'_, Postgres>,
    token_contract: &str,
    price: &str,
    chain_order: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"insert into inference_deals (token_contract_address, price_per_tick, last_chain_order)
           values ($1, $2::numeric, $3)
           on conflict (token_contract_address) do update
               set price_per_tick = coalesce(inference_deals.price_per_tick, excluded.price_per_tick),
                   last_chain_order = greatest(coalesce(inference_deals.last_chain_order, ''), excluded.last_chain_order),
                   updated_at = now()"#,
    )
    .bind(token_contract)
    .bind(price)
    .bind(chain_order)
    .execute(&mut **tx)
    .await
    .context("record inference_deals price_per_tick from the SELL offer")?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct LockedOrder {
    order_id: String,
    is_sweep_cancel: bool,
    /// The leg's side. `isBuyerMaker` is not carried by `InferenceFilled`, so the tape
    /// takes it from whichever leg is locked here.
    is_buy: bool,
    /// Whether this leg was placed as a subscription. Read for the same reason as
    /// `is_buy`: `InferenceFilled` does not carry the deal flags, and the buyer's
    /// bond — and therefore the deal's deposit — depends on this bit.
    is_subscription: bool,
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
    /// The seller named by the event itself (`InferenceFilled`'s 7th field as of
    /// v4.0.33). Before it, the seller was recovered by walking the SELL leg in
    /// `inference_orders`, which lost him exactly when that leg had not arrived —
    /// that is, on orphan repair.
    seller_note: Option<String>,
}

impl FilledFields {
    fn parse(event: &DecodedEvent, node: &EventNode) -> anyhow::Result<Self> {
        // EXHAUSTIVE destructuring — the only form of the "this ABI field is
        // consumed" guard that actually works. The compiler forces every field to
        // be named: one that arrives and is needed by nobody cannot be ignored
        // silently. Checking DTO keys against the ABI does NOT prove this — it
        // stays green when a field is declared and then thrown away (which is
        // exactly how `sellerNote` got lost).
        let FilledData {
            maker_id,
            taker_id,
            ticks,
            clearing_price,
            seller_tc,
            buyer_note,
            seller_note,
        } = serde_json::from_value(event.value.clone())
            .context("InferenceFilled: payload does not parse against the ABI")?;

        let maker_id = maker_id.to_string();
        let taker_id = taker_id.to_string();
        let ids = vec![maker_id.clone(), taker_id.clone()];
        Ok(Self {
            ob: node.src.as_deref().context("Filled: src missing")?.to_string(),
            maker_id,
            taker_id,
            ticks: ticks.to_string(),
            // `clearingPrice` is a uint256; the decoder hands it over as
            // "0x" + 64 hex. The DTO holds the RAW ABI value, so the conversion
            // stays here: without it the hex would reach
            // `inference_trades.price`, which numeric will not accept. Tests with
            // decimal fixtures see no difference — hence the guard
            // `a_uint256_price_arrives_as_decimal_not_hex`.
            clearing_price: uint256_maybe_hex(&clearing_price)?,
            chain_order: node_chain_order(node, "InferenceFilled")?,
            chain_seconds: parse_unix_seconds(node.created_at.as_ref()),
            ids,
            seller_tc: non_zero_address(seller_tc.as_deref()).map(str::to_string),
            buyer_note: Some(buyer_note),
            seller_note: non_zero_address(seller_note.as_deref()).map(str::to_string),
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
        r#"select order_id::text as order_id, (swept_at is not null) as is_sweep_cancel,
                  is_buy, is_subscription
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
                      when o.status = 'EXPIRED' then o.amount_remaining
                      when o.status = 'CANCELLED' and o.swept_at is null then o.amount_remaining
                      else greatest(o.amount_remaining - $3::numeric, 0::numeric)
                  end,
                  status = case
                      when o.status = 'FILLED' then 'FILLED'
                      -- Expiry is terminal: a late fill does not resurrect the row.
                      -- It returns ITS OWN status, not 'FILLED' — the outcome differs.
                      -- This arm sits ABOVE `is_buy = false`, otherwise an expired
                      -- SELL would be intercepted by that one.
                      when o.status = 'EXPIRED' then 'EXPIRED'
                      when o.status = 'CANCELLED' and o.swept_at is null then 'CANCELLED'
                      when o.is_buy = false then 'FILLED'
                      when greatest(o.amount_remaining - $3::numeric, 0::numeric) = 0 then 'FILLED'
                      else 'OPEN'
                  end,
                  swept_at = case
                      when o.status = 'FILLED' then o.swept_at
                      when o.status = 'EXPIRED' then o.swept_at
                      when o.status = 'CANCELLED' and o.swept_at is null then o.swept_at
                      else null
                  end,
                  -- A terminal row — FILLED, EXPIRED, or a real event-cancel
                  -- (CANCELLED with swept_at NULL) — is a FULL no-op: a late or duplicate
                  -- Filled must not advance its chain/bookkeeping columns either (mirrors
                  -- projectors::apply_order_filled). Only OPEN rows and provisional
                  -- sweep-cancels (swept_at NOT NULL, being overridden) mutate.
                  last_chain_order = case
                      when o.status = 'FILLED' or o.status = 'EXPIRED'
                           or (o.status = 'CANCELLED' and o.swept_at is null) then o.last_chain_order
                      else greatest(o.last_chain_order, $4)
                  end,
                  chain_updated_at = case
                      when o.status = 'FILLED' or o.status = 'EXPIRED'
                           or (o.status = 'CANCELLED' and o.swept_at is null) then o.chain_updated_at
                      else greatest(o.chain_updated_at, to_timestamp($5::double precision))
                  end,
                  updated_at = case
                      when o.status = 'FILLED' or o.status = 'EXPIRED'
                           or (o.status = 'CANCELLED' and o.swept_at is null) then o.updated_at
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
/// (`sellerTC`) ↔ its market (`orderbook_address`), seller note (`sellerNote` from
/// the event, falling back to a walk of the SELL leg when the event predates the
/// field), and buyer note (`buyerNote`). Upserts so the row survives
/// whether the deal was first seen here or via an earlier TokenContract.* event.
/// On a well-formed `Filled`, `sellerTC` is always present and non-zero. It reaches
/// this function through `non_zero_address`, so `None` here covers two chain states
/// at once — the field missing (ABI drift) and the field carrying the zero address —
/// which is why the log names the observation rather than guessing the cause. Either
/// way that one case is logged and skipped with `Ok(())` (not `Err`): `apply_filled_decrement` has already mutated rows in this transaction,
/// and failing the event over a decoder/ABI mismatch would defer it forever. A
/// genuine DB error in the queries below still propagates — the reprojection
/// savepoint isolates and retries the event.
async fn link_deal_from_filled(
    tx: &mut Transaction<'_, Postgres>,
    f: &FilledFields,
    // Whether the BUY leg was a subscription, or `None` when no leg is present to
    // ask — the orphan-repair path. See `record_deal_deposit`.
    buy_is_subscription: Option<bool>,
) -> anyhow::Result<()> {
    let Some(seller_tc) = f.seller_tc.as_deref() else {
        warn!(
            orderbook_address = %f.ob,
            chain_order = %f.chain_order,
            "InferenceFilled carried no usable sellerTC (field absent, or present as the zero address); inference_deals orderbook/seller link skipped"
        );
        return Ok(());
    };

    // Walking the SELL leg (is_buy=false) works only if that leg has been
    // PROJECTED. Kept as a fallback — the event now names the seller in a field
    // of its own.
    let seller_from_leg: Option<String> = sqlx::query_scalar(
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

    // The event outranks the walk: the walk loses the seller exactly when the leg
    // has not arrived (orphan repair), whereas `sellerNote` is always there.
    let seller_note = f.seller_note.clone().or(seller_from_leg);

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

    record_deal_deposit(tx, f, seller_tc, buy_is_subscription).await
}

/// Recover the deal's `deposit` — the escrow figure the book sent it — by
/// REPRODUCING the book's own arithmetic.
///
/// WHY IT HAS TO BE COMPUTED. No event carries it. `_match` derives
/// `debit = cost + bond` and passes it to `fundFromOrderBook`, the deal stores it
/// as `_deposit` and reported it in `StreamFunded` — the event ingest no longer
/// captures. Every INPUT is emitted, though: `ticks` and `clearingPrice` come off
/// this `InferenceFilled`, and the two rate constants are pinned against the
/// Solidity source by `dodex-contracts/tests/order_book_fee_constants.rs`.
///
/// THE ARITHMETIC IS SOLIDITY'S, NOT AN APPROXIMATION OF IT. `_unit(p) = p +
/// (p * PLATFORM_FEE_BPS) / BPS_DENOMINATOR` truncates once, on the fee, and
/// `cost = ticks * unit` is exact. `div()` is Postgres integer division, so the
/// truncation lands in the same place; doing this in `numeric` rather than in
/// Rust also keeps it clear of the 128-bit ceiling the chain works under.
///
/// `bond` is `2 * clearingPrice` for a subscription and zero otherwise
/// (`InferenceOrderBook.sol:745-746`) — the buyer's bond rides along with the
/// escrow only on that path, and the deal separates them after.
///
/// UNKNOWN IS NOT ZERO. `InferenceFilled` does not carry the deal flags, so the
/// subscription bit is read off the BUY leg. On the orphan path no leg is present
/// and the answer is `None` — there the deposit is SKIPPED, because assuming "not
/// a subscription" would silently record a figure short by the bond, and a wrong
/// number is worse here than a NULL that says "not known".
///
/// WHAT THIS RECORDS IS WHAT THE BOOK SENT, and on one narrow path that differs
/// from what the deal kept: `fundFromOrderBook` refuses a fill it cannot fund
/// (nonce reuse, under two ticks, over `maxTicks`, a subscription whose bond did
/// not arrive) and refunds the buyer in full without ever setting `_deposit`.
/// The refusal is invisible from the book's events — it happens after the fill is
/// emitted — so this column then names an escrow that came straight back. The
/// book validates most of those conditions before matching, so the path is
/// narrow; it is named here rather than left for someone to discover from a
/// deposit on a deal that never opened.
async fn record_deal_deposit(
    tx: &mut Transaction<'_, Postgres>,
    f: &FilledFields,
    seller_tc: &str,
    buy_is_subscription: Option<bool>,
) -> anyhow::Result<()> {
    let Some(is_subscription) = buy_is_subscription else {
        warn!(
            token_contract = %seller_tc,
            chain_order = %f.chain_order,
            "InferenceFilled orphan: no BUY leg to read the subscription flag from, so the deal's \
             deposit is left NULL rather than computed without its possible bond"
        );
        return Ok(());
    };
    let fee_bps = i64::from(PLATFORM_FEE_BPS);
    let bps_denominator = i64::from(BPS_DENOMINATOR);
    let bond_multiple: i64 = if is_subscription { 2 } else { 0 };
    sqlx::query(
        r#"insert into inference_deals (token_contract_address, deposit, last_chain_order)
           values (
               $1,
               $2::numeric * ($3::numeric + div($3::numeric * $4::numeric, $5::numeric))
                   + $6::numeric * $3::numeric,
               $7
           )
           on conflict (token_contract_address) do update
               set deposit = coalesce(inference_deals.deposit, excluded.deposit),
                   last_chain_order = greatest(coalesce(inference_deals.last_chain_order, ''), excluded.last_chain_order),
                   updated_at = now()"#,
    )
    .bind(seller_tc)
    .bind(&f.ticks)
    .bind(&f.clearing_price)
    .bind(fee_bps)
    .bind(bps_denominator)
    .bind(bond_multiple)
    .bind(&f.chain_order)
    .execute(&mut **tx)
    .await
    .context("record inference_deals deposit from Filled")?;
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
    let buy_is_subscription = locked.iter().find(|r| r.is_buy).map(|r| r.is_subscription);
    link_deal_from_filled(tx, &f, buy_is_subscription).await?;
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
    /// An expiry whose `InferenceOrderPlaced` never arrived: there is nothing to
    /// close, but the order's status is lost — it will never reach `EXPIRED`.
    ExpiryLost,
    /// A refund with no parent: the return happened on chain, but the order it
    /// concerns is absent from the read model. What exactly was lost CANNOT be
    /// stated, and the wording must respect that: before the deadline the same
    /// event type is emitted by `_finalizeTaker` for a taker — including a fully
    /// filled one — and on its own closes nothing (see `apply_inference_refunded`).
    /// Without the row the deadline is invisible too, so it is not even known
    /// whether this is an expiry or the end of a taker's life. What is lost is the
    /// link between the refund and its order — no more than that.
    RefundLost,
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
            // The Filled carries sellerTC + sellerNote + buyerNote; record the deal
            // link even on the orphan path — every one of them comes off the event
            // itself, which is why this works with no leg present at all. The SELL-leg
            // walk is only the fallback for events older than the `sellerNote` field,
            // and it is exactly the case the orphan path cannot satisfy. The normal
            // deferred path never reruns.
            // `None`: no leg is present on this path — that is what makes it an
            // orphan — so the subscription bit cannot be read and the deposit is
            // skipped rather than assumed. See `record_deal_deposit`.
            link_deal_from_filled(tx, &f, None).await?;
            outcome
        }
        "InferenceOrderCancelled" => ExpiredOrphanOutcome::CancelLost,
        "InferenceOrderExpired" => ExpiredOrphanOutcome::ExpiryLost,
        "InferenceRefunded" => ExpiredOrphanOutcome::RefundLost,
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
        ExpiredOrphanOutcome::ExpiryLost => warn!(
            msg_id = %node.msg_id, event_type = %event.event_type,
            "inference OrderExpired orphan past cutoff: the order will never reach EXPIRED (its OrderPlaced was dropped at capture); the phantom sweep is what reconciles the row if a late placement re-opens it"
        ),
        ExpiredOrphanOutcome::RefundLost => warn!(
            msg_id = %node.msg_id, event_type = %event.event_type,
            "inference Refunded orphan past cutoff: the refund happened on chain but its order was never projected, so the refund cannot be attributed; whether the order expired or was a taker ending its life is unknowable without the row's deadline"
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

/// `InferenceRefunded(orderId, note, amount)` — the money went back to its owner,
/// and as of v4.0.33 the event carries an id, i.e. it speaks about a SPECIFIC row.
/// It is NOT a general close signal: the contract emits it from four sites, and
/// only two of them are about an order disappearing on time
/// (`InferenceOrderBook.sol`):
///   * `:1368` — a continuation resumed past its deadline: the ONLY event on this
///     path, `InferenceOrderExpired` is not emitted for it at all;
///   * `:1135` — `_removeExpiredBid` -> `_refundAndRemove`: an
///     `InferenceOrderExpired` follows, so here this is merely an early duplicate;
///   * `:1183` — `_finalizeTaker` for a BUY: the end of a taker's life, including
///     a FULLY FILLED one (`remaining < 2` covers zero too, and the event goes out
///     even with a zero `leftover`);
///   * `:1223` — `_finalizeTaker` for a taker-only SELL that did not match.
///
/// The deadline separates them, and separates them EXACTLY: `:1643` requires
/// `deadline == 0 || deadline > block.timestamp` at submit, and the re-check on
/// entry to `_doPlaceHead` (`:1355`) diverts an expired continuation into the
/// refund branch BEFORE matching. So `_finalizeTaker` is unreachable past the
/// deadline, and both expiry branches are unreachable before it.
///
/// The row is therefore closed ONLY past the deadline and only into `EXPIRED`.
/// There is deliberately no `CANCELLED` branch here. The refund knows nothing
/// about whether the taker filled — `InferenceFilled` knows that, and if it is
/// deferred over a missing leg the drain still moves on (the `Deferred` arms of
/// `reproject_pending_from` leave the row unmarked and take the next one), so the
/// refund would be applied BEFORE its own fill.
/// Closing the row would leave a filled order cancelled forever: on the fill's
/// retry the terminal guard in `apply_filled_decrement` forbids both the
/// decrement and the transition to `FILLED`.
///
/// An IOC/MARKET leftover and dust are closed by the phantom sweep — by
/// reconciling against the chain itself, not by guessing from a payload. That is
/// precisely the sweep's job.
async fn apply_inference_refunded(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let ob = node.src.as_deref().context("Refunded: src missing")?;
    let RefundedData {
        order_id,
        // Who was refunded is known to the note itself; `inference_orders` has no
        // column for it.
        note: _,
        // The read model does not need the refunded amount: closing the row is
        // decided by the deadline, not by the size of the return (see the body).
        amount: _,
    } = serde_json::from_value(event.value.clone())
        .context("InferenceRefunded: payload does not parse against the ABI")?;
    let order_id = order_id.to_string();
    let chain_order = node_chain_order(node, "InferenceRefunded")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());

    // THE PARENT IS CHECKED FIRST, regardless of whether the time is known. An
    // orphan must go to Deferred and from there into the age-based dead letter.
    // An early return on a missing time would mark it Applied, and a late
    // placement would then open an order this refund can never close.
    let parent: Option<(String,)> = sqlx::query_as(
        r#"select status from inference_orders
            where orderbook_address = $1 and order_id = $2::numeric for update"#,
    )
    .bind(ob)
    .bind(&order_id)
    .fetch_optional(&mut **tx)
    .await
    .context("lock inference row for Refunded")?;
    if parent.is_none() {
        return Ok(ProjectionOutcome::Deferred);
    }

    // The parent exists, but the event carries no time: choosing between "it
    // expired" and "the taker finished" is IMPOSSIBLE, and guessing means writing
    // a wrong terminal status forever. The row is left as is, the event is marked
    // Applied, and closing it falls to the sweep — the very one this wave kept
    // precisely as a reconciliation against chain state.
    let Some(chain_seconds) = chain_seconds else {
        warn!(
            orderbook_address = ob, order_id = %order_id, chain_order = %chain_order,
            "inference Refunded without chain time: status left to the phantom sweep rather than guessed"
        );
        return Ok(ProjectionOutcome::Applied);
    };

    // The row is already locked by the SELECT above, so the decision can be made
    // by a predicate in the WHERE: a row that does not qualify is not touched AT
    // ALL, `updated_at` included. Otherwise a "no-op" lies to an observer and
    // churns the stamp on every dust refund against an already filled order.
    //
    // The "who may be stamped expired" predicate is the same one
    // `apply_inference_order_expired` uses: `OPEN`, or a PROVISIONAL sweep mark
    // (`swept_at` NOT NULL). The sweep guessed `CANCELLED` only because the order
    // had vanished from the book, which is exactly what expiry does — so the
    // event corrects it and clears `swept_at`. `FILLED`, `EXPIRED` and a REAL
    // cancel (`swept_at` NULL) stand.
    sqlx::query(
        r#"update inference_orders o
              set status = 'EXPIRED',
                  swept_at = null,
                  last_chain_order = greatest(o.last_chain_order, $3),
                  chain_updated_at = greatest(o.chain_updated_at, to_timestamp($4::double precision)),
                  updated_at = now()
            where o.orderbook_address = $1
              and o.order_id = $2::numeric
              -- A refund may close ONLY a past-deadline order: before the deadline
              -- this is `_finalizeTaker`, where `Filled` decides the order's fate.
              and o.deadline is not null
              and o.deadline <= $4::numeric
              and (o.status = 'OPEN' or (o.status = 'CANCELLED' and o.swept_at is not null))"#,
    )
    .bind(ob)
    .bind(&order_id)
    .bind(&chain_order)
    .bind(chain_seconds)
    .execute(&mut **tx)
    .await
    .context("inference Refunded update")?;

    // Applied regardless of how many rows were affected: the parent's existence
    // is already proven by the locking SELECT, and "did not qualify by deadline"
    // is an answer, not a postponement. Deferred here would mean "wait", and the
    // event would spin forever.
    Ok(ProjectionOutcome::Applied)
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
               select status,
                      -- Expiry is terminal on a par with a fill: a late cancel has
                      -- no right to demote EXPIRED to CANCELLED.
                      status in ('FILLED','EXPIRED') as terminal
                 from inference_orders
                where orderbook_address = $1 and order_id = $2::numeric for update)
           update inference_orders o
              set status = case when prior.terminal then prior.status else 'CANCELLED' end,
                  swept_at = case when prior.terminal then o.swept_at else null end,
                  last_chain_order = case when prior.terminal then o.last_chain_order
                                          else greatest(o.last_chain_order, $3) end,
                  chain_updated_at = case when prior.terminal then o.chain_updated_at
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
