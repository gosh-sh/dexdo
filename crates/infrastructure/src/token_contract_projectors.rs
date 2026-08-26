// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Projects TokenContract.* events (per-deal streaming escrow, inference
// SETTLEMENT phase) into the inference_deals read-model + inference_ticks log.
// Mirrors inference_projectors.rs: a deal skeleton is seeded for ANY
// TokenContract event by src_address (the TokenContract address) before
// event-specific handling, so out-of-order delivery still records the deal.
// The orderbook_address + seller_note link is filled by the
// InferenceOrderBook.Filled handler (the only event carrying sellerTC +
// buyerNote together).

use anyhow::Context;
use sqlx::Postgres;
use sqlx::Transaction;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;
use crate::indexer_repo::parse_unix_seconds;
use crate::projectors::field_str;
use crate::projectors::node_chain_order;
use crate::projectors::uint_field_to_decimal;
use crate::projectors::ProjectionOutcome;

pub async fn project_token_contract_event(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    // Discovery pre-step: seed a deal skeleton for ANY TokenContract event before
    // event-specific handling, so a TokenContract event arriving before its Filled
    // still records the deal (Filled later enriches orderbook + seller).
    seed_deal_skeleton(tx, node).await?;

    let suffix =
        event.event_type.strip_prefix("TokenContract.").unwrap_or(event.event_type.as_str());
    match suffix {
        "ContractDeployed" => Ok(ProjectionOutcome::Applied), // skeleton only
        "StreamFunded" => apply_stream_funded(tx, event, node).await,
        "StreamOpened" => apply_stream_opened(tx, event, node).await,
        "TickFinalized" => apply_tick_finalized(tx, event, node).await,
        "TicksClaimed" => apply_ticks_claimed(tx, event, node).await,
        "StreamStopped" => apply_close(tx, node, "STOPPED", true).await,
        "DisputeResolved" => apply_close(tx, node, "DISPUTE_RESOLVED", false).await,
        "StreamReclaimed" => apply_close(tx, node, "RECLAIMED", false).await,
        "ContractDestroyed" => apply_terminal_close(tx, node, "DESTROYED").await,
        "StreamDisputed" => apply_disputed(tx, node).await,
        "ProbeBurned" => apply_terminal_close(tx, node, "PROBE_BURNED").await,
        // Seller bond / probe accept / withdrawal carry no deal-level state the
        // SETTLEMENT read-model needs; the skeleton seed already recorded the deal.
        // `BuyerBondFunded` is the counterpart of `SellerBondFunded` — v4.0.35 made
        // the bond two-sided — and belongs here for the same reason. `EndpointSet`
        // carries the buyer's endpoint as ciphertext only the two parties can read, so
        // there is nothing in it a read model could serve.
        "SellerBondFunded" | "BuyerBondFunded" | "ProbeAccepted" | "ShellWithdrawn"
        | "EndpointSet" => Ok(ProjectionOutcome::Applied),
        _ => Ok(ProjectionOutcome::Unknown),
    }
}

async fn seed_deal_skeleton(
    tx: &mut Transaction<'_, Postgres>,
    node: &EventNode,
) -> anyhow::Result<()> {
    let tc = node.src.as_deref().context("TokenContract event: src missing")?;
    sqlx::query(
        r#"insert into inference_deals (token_contract_address)
           values ($1)
           on conflict (token_contract_address) do nothing"#,
    )
    .bind(tc)
    .execute(&mut **tx)
    .await
    .context("seed inference_deals skeleton")?;
    Ok(())
}

/// A deal is funded — and, since contracts 4.0.36, possibly not for the first
/// time at this address.
///
/// `cleanupUnopened` used to end in `_die`, so a no-show destroyed the deal and
/// the next match needed a fresh deploy at a fresh address. It no longer does:
/// it puts the state back to unfunded and the SAME `TokenContract` takes a new
/// offer through `postFromNote`, with a different buyer and a different deposit.
/// The address is this table's primary key, so a second cycle lands on the first
/// cycle's row.
///
/// Everything written per cycle is therefore cleared before the new figures go
/// in — `buyer_note`, `deposit` and `funded_at_chain` included, since the second
/// statement writes those three through `coalesce` and would otherwise keep
/// cycle one's buyer forever while the row went on reporting a settlement that
/// has been superseded. What is
/// NOT cleared is what the address itself fixes: `orderbook_address` and
/// `seller_note` derive from the seller's key and nonce and cannot change while
/// the address does not.
///
/// Guarded by `last_chain_order` rather than run unconditionally, because
/// reprojection replays rows: only a funding NEWER than everything already
/// folded into this row starts a cycle, so replaying cycle one's `StreamFunded`
/// after cycle two began is inert instead of destructive. The first funding of a
/// deal has no cycle to clear (`funded_at_chain is null`) and skips the reset
/// outright.
///
/// Known gap, left open deliberately: this orders cycles, not events within
/// one. A cycle-one event delivered out of order AFTER cycle two's funding still
/// writes into cycle two through its own `coalesce`. Closing that needs a cycle
/// number on every deal row and on every event that touches one — a migration
/// and a wider key — and the case it guards against has not been observed.
async fn apply_stream_funded(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("StreamFunded: src missing")?;
    let buyer = field_str(&event.value, "buyer")?;
    let deposit = uint_field_to_decimal(&event.value, "deposit")?;
    let chain_order = node_chain_order(node, "StreamFunded")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());

    let starts_new_cycle = sqlx::query_scalar::<_, bool>(
        r#"update inference_deals
              set buyer_note = null,
                  deposit = null,
                  funded_at_chain = null,
                  price_per_tick = null,
                  opened_at_chain = null,
                  settled_at_chain = null,
                  close_kind = null,
                  clean_settlement = null,
                  disputed_at_chain = null,
                  finalized_ticks = 0,
                  trusted_ticks = null,
                  claimed_ticks = null,
                  updated_at = now()
            where token_contract_address = $1
              and funded_at_chain is not null
              and $2 > coalesce(last_chain_order, '')
        returning true"#,
    )
    .bind(tc)
    .bind(&chain_order)
    .fetch_optional(&mut **tx)
    .await
    .context("reset inference_deals for a new funding cycle")?
    .unwrap_or(false);

    // The per-tick log goes with the counter it backs: `apply_tick_finalized`
    // increments `finalized_ticks` only when its insert is a real insert, so a
    // counter reset that left the old rows behind would make the two disagree
    // forever — the next cycle's first tick would conflict on
    // `(token_contract_address, chain_order)` only if the gateway ever reused an
    // ordering key, but the COUNT would read as one cycle's worth of ticks split
    // across two. The settled history of cycle one is in `inference_trades`.
    if starts_new_cycle {
        sqlx::query(r#"delete from inference_ticks where token_contract_address = $1"#)
            .bind(tc)
            .execute(&mut **tx)
            .await
            .context("clear inference_ticks for a new funding cycle")?;
    }

    sqlx::query(
        r#"update inference_deals
              set buyer_note = coalesce(buyer_note, $2),
                  deposit = coalesce(deposit, $3::numeric),
                  funded_at_chain = coalesce(funded_at_chain, to_timestamp($4::double precision)),
                  last_chain_order = greatest(coalesce(last_chain_order, ''), $5),
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc)
    .bind(buyer)
    .bind(&deposit)
    .bind(chain_seconds)
    .bind(&chain_order)
    .execute(&mut **tx)
    .await
    .context("apply StreamFunded")?;
    Ok(ProjectionOutcome::Applied)
}

async fn apply_stream_opened(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("StreamOpened: src missing")?;
    let buyer = field_str(&event.value, "buyer")?;
    let ppt = uint_field_to_decimal(&event.value, "pricePerTick")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    sqlx::query(
        r#"update inference_deals
              set buyer_note = coalesce(buyer_note, $2),
                  price_per_tick = coalesce(price_per_tick, $3::numeric),
                  opened_at_chain = coalesce(opened_at_chain, to_timestamp($4::double precision)),
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc)
    .bind(buyer)
    .bind(&ppt)
    .bind(chain_seconds)
    .execute(&mut **tx)
    .await
    .context("apply StreamOpened")?;
    Ok(ProjectionOutcome::Applied)
}

/// The seller's running claim against the deal, reported by `claimTokens`.
///
/// Distinct from [`apply_tick_finalized`], which records the settled figure at a
/// weekly boundary: this is the position between boundaries, and a subscription
/// week is long enough that a deal would otherwise look motionless for days.
///
/// `greatest` rather than a plain assignment: both figures are cumulative on
/// chain, so a replayed or out-of-order event must not walk them back. That also
/// makes the write idempotent without a per-event key of its own.
async fn apply_ticks_claimed(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("TicksClaimed: src missing")?;
    let trusted = uint_field_to_decimal(&event.value, "trusted")?;
    let claimed = uint_field_to_decimal(&event.value, "claimed")?;
    let chain_order = node_chain_order(node, "TicksClaimed")?;

    sqlx::query(
        r#"update inference_deals
              set trusted_ticks = greatest(coalesce(trusted_ticks, 0), $2::numeric),
                  claimed_ticks = greatest(coalesce(claimed_ticks, 0), $3::numeric),
                  last_chain_order = greatest(last_chain_order, $4),
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc)
    .bind(&trusted)
    .bind(&claimed)
    .bind(&chain_order)
    .execute(&mut **tx)
    .await
    .context("inference TicksClaimed update")?;

    Ok(ProjectionOutcome::Applied)
}

async fn apply_tick_finalized(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("TickFinalized: src missing")?;
    let finalized_owed = uint_field_to_decimal(&event.value, "finalizedOwed")?;
    let deposit = uint_field_to_decimal(&event.value, "deposit")?;
    let chain_order = node_chain_order(node, "TickFinalized")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());

    // Per-tick row keyed by (tc, chain_order) — idempotent under reprojection replay.
    let res = sqlx::query(
        r#"insert into inference_ticks
               (token_contract_address, chain_order, finalized_owed, deposit, chain_at)
           values ($1, $2, $3::numeric, $4::numeric, to_timestamp($5::double precision))
           on conflict (token_contract_address, chain_order) do nothing"#,
    )
    .bind(tc)
    .bind(&chain_order)
    .bind(&finalized_owed)
    .bind(&deposit)
    .bind(chain_seconds)
    .execute(&mut **tx)
    .await
    .context("insert inference_ticks")?;

    // Bump the finalized-tick COUNT only on a real insert, so a replay of an
    // already-recorded tick does not double-count. (finalized_ticks counts
    // TickFinalized events; it is not the contract's on-chain _ticksFinalized.)
    if res.rows_affected() == 1 {
        sqlx::query(
            r#"update inference_deals
                  set finalized_ticks = finalized_ticks + 1,
                      updated_at = now()
                where token_contract_address = $1"#,
        )
        .bind(tc)
        .execute(&mut **tx)
        .await
        .context("bump inference_deals finalized_ticks")?;
    }
    Ok(ProjectionOutcome::Applied)
}

async fn apply_close(
    tx: &mut Transaction<'_, Postgres>,
    node: &EventNode,
    close_kind: &str,
    clean: bool,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("deal close: src missing")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    sqlx::query(
        r#"update inference_deals
              set settled_at_chain = coalesce(settled_at_chain, to_timestamp($2::double precision)),
                  close_kind = coalesce(close_kind, $3),
                  clean_settlement = coalesce(clean_settlement, $4),
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc)
    .bind(chain_seconds)
    .bind(close_kind)
    .bind(clean)
    .execute(&mut **tx)
    .await
    .context("apply deal close")?;
    Ok(ProjectionOutcome::Applied)
}

async fn apply_disputed(
    tx: &mut Transaction<'_, Postgres>,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("StreamDisputed: src missing")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    sqlx::query(
        r#"update inference_deals
              set disputed_at_chain = coalesce(disputed_at_chain, to_timestamp($2::double precision)),
                  clean_settlement = false,
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc).bind(chain_seconds)
    .execute(&mut **tx).await.context("apply StreamDisputed")?;
    Ok(ProjectionOutcome::Applied)
}

/// Terminal close that records `close_kind` + `settled_at_chain` but does NOT
/// set `clean_settlement` (it stays NULL/false — not a clean settlement). Used
/// by `ContractDestroyed` and `ProbeBurned`, both of which terminate the deal
/// without a clean stop.
async fn apply_terminal_close(
    tx: &mut Transaction<'_, Postgres>,
    node: &EventNode,
    close_kind: &str,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("terminal close: src missing")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    sqlx::query(
        r#"update inference_deals
              set settled_at_chain = coalesce(settled_at_chain, to_timestamp($2::double precision)),
                  close_kind = coalesce(close_kind, $3),
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc)
    .bind(chain_seconds)
    .bind(close_kind)
    .execute(&mut **tx)
    .await
    .context("apply terminal close")?;
    Ok(ProjectionOutcome::Applied)
}
