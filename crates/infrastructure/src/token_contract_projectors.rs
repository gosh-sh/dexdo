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
//
// SINCE 2026-08-24 THE TokenContract.* HALF OF THIS MODULE HAS NO LIVE INPUT.
// Ingest scopes capture to a `dst` allow-list (`config::SCOPED_EVENT_IDS`) that
// excludes every TokenContract route, so those handlers run only when retained
// rows are replayed. `project_deal_closed_from_note` below is the exception and
// the reason this module still writes on the live path: it is fed by
// `PrivateNote.InferenceDealClosed`, which IS captured.

use anyhow::Context;
use dodex_contracts::airegistry::token_contract_events::StreamFundedData;
use dodex_contracts::airegistry::token_contract_events::StreamOpenedData;
use dodex_contracts::airegistry::token_contract_events::TickFinalizedData;
use dodex_contracts::airegistry::token_contract_events::TicksClaimedData;
use dodex_contracts::airegistry::token_contract_events::TokenContractEvent;
use dodex_contracts::dex::private_note_events::InferenceDealClosedData;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::graphql::EventNode;
use crate::indexer_repo::parse_unix_seconds;
use crate::projectors::node_chain_order;
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
    // Route by the enum VARIANT, not by a string literal: the variant is pinned to
    // the ABI (`token_contract_enum_covers_every_abi_event`), so an arm naming an
    // event that does not exist stops being expressible — that is how
    // `StreamReclaimed` lived on as a dead arm. The `match` is exhaustive WITHOUT
    // `_`: a new variant will not compile until someone assigns it an outcome.
    use TokenContractEvent as E;
    let Some(kind) = E::ALL.iter().copied().find(|v| format!("{v:?}") == suffix) else {
        return Ok(ProjectionOutcome::Unknown);
    };
    match kind {
        E::ContractDeployed => Ok(ProjectionOutcome::Applied), // skeleton only
        E::StreamFunded => apply_stream_funded(tx, event, node).await,
        E::StreamOpened => apply_stream_opened(tx, event, node).await,
        E::TickFinalized => apply_tick_finalized(tx, event, node).await,
        E::TicksClaimed => apply_ticks_claimed(tx, event, node).await,
        E::StreamStopped => apply_close(tx, node, "STOPPED", true).await,
        E::DisputeResolved => apply_close(tx, node, "DISPUTE_RESOLVED", false).await,
        E::ContractDestroyed => apply_terminal_close(tx, node, "DESTROYED").await,
        E::StreamDisputed => apply_disputed(tx, node).await,
        E::ProbeBurned => apply_terminal_close(tx, node, "PROBE_BURNED").await,
        // Seller bond / probe accept / withdrawal carry no deal-level state the
        // SETTLEMENT read-model needs; the skeleton seed already recorded the deal.
        // `BuyerBondFunded` is the counterpart of `SellerBondFunded` — v4.0.35 made
        // the bond two-sided — and belongs here for the same reason. `EndpointSet`
        // carries the buyer's endpoint as ciphertext only the two parties can read, so
        // there is nothing in it a read model could serve.
        E::SellerBondFunded
        | E::BuyerBondFunded
        | E::ProbeAccepted
        | E::ShellWithdrawn
        | E::EndpointSet => Ok(ProjectionOutcome::Applied),
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

async fn apply_stream_funded(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("StreamFunded: src missing")?;
    // Exhaustive destructuring: every ABI field must be named.
    let StreamFundedData { buyer, deposit } = serde_json::from_value(event.value.clone())
        .context("StreamFunded: payload does not parse against the ABI")?;
    let buyer = buyer.as_str();
    let deposit = deposit.to_string();
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    sqlx::query(
        r#"update inference_deals
              set buyer_note = coalesce(buyer_note, $2),
                  deposit = coalesce(deposit, $3::numeric),
                  funded_at_chain = coalesce(funded_at_chain, to_timestamp($4::double precision)),
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc)
    .bind(buyer)
    .bind(&deposit)
    .bind(chain_seconds)
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
    let StreamOpenedData { buyer, price_per_tick } = serde_json::from_value(event.value.clone())
        .context("StreamOpened: payload does not parse against the ABI")?;
    let buyer = buyer.as_str();
    let ppt = price_per_tick.to_string();
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
    let TicksClaimedData { trusted, claimed } = serde_json::from_value(event.value.clone())
        .context("TicksClaimed: payload does not parse against the ABI")?;
    let trusted = trusted.to_string();
    let claimed = claimed.to_string();
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
    let TickFinalizedData { finalized_owed, deposit } = serde_json::from_value(event.value.clone())
        .context("TickFinalized: payload does not parse against the ABI")?;
    let finalized_owed = finalized_owed.to_string();
    let deposit = deposit.to_string();
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

/// `PrivateNote.InferenceDealClosed` — the only live signal that a deal ended.
///
/// WHY THIS EXISTS AT ALL. `settled_at_chain` used to come from the deal's own
/// close events (`StreamStopped` / `DisputeResolved` / `ContractDestroyed` /
/// `ProbeBurned`). Ingest no longer captures any TokenContract route, so all four
/// stopped arriving and every deal read as never settled — including for
/// `dodex-points-rewards`, which asks exactly `settled_at_chain is not null`
/// (see `tests/rewards_query_compat.rs`).
///
/// WHY IT IS COMPLETE FOR ITS ONE FACT. `TokenContract._die` is the single funnel
/// every close path ends in (`TokenContract.sol:459-467`): it calls
/// `onDealClosed` on BOTH notes, then emits `ContractDestroyed` and
/// self-destructs. The note drops the deal from `_liveDeals` and emits this. So
/// no close can happen without it, whatever branch produced the close.
///
/// THE ADDRESS IS IN THE PAYLOAD, NOT IN `src`. Every other handler in this file
/// keys on `node.src`, because there the emitter IS the deal. Here the emitter is
/// the NOTE and the deal is the payload's only field. Keying on `src` would stamp
/// the settlement onto a row named after a party.
///
/// FIRES TWICE PER DEAL — once from the buyer's note, once from the seller's.
/// Both writes are the same fact, so `coalesce` keeps the first and the second is
/// a no-op; the same property makes it idempotent under replay.
///
/// WHAT IT DELIBERATELY DOES NOT WRITE: `close_kind` and `clean_settlement`. This
/// event says THAT the deal closed, never HOW. The surrounding signals cannot
/// settle it either — telling `STOPPED` from `PROBE_BURNED` from
/// `DISPUTE_RESOLVED` from `cleanupUnopened` would mean reading the shape of the
/// `DealCredited` payments plus the presence of `RootPN.DealWriteOffReported`,
/// and those patterns overlap and depend on amounts. `DealCredited` also lost the
/// earmark parameter in v4.0.33. A guess here would be indistinguishable from a
/// fact to every reader downstream, so the columns stay NULL until the chain
/// names the branch.
pub async fn project_deal_closed_from_note(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let InferenceDealClosedData { deal } = serde_json::from_value(event.value.clone())
        .context("InferenceDealClosed: payload does not parse against the ABI")?;
    // A zero address is not a deal. The note only emits inside
    // `if (_liveDeals.exists(msg.sender))`, so this cannot occur on the live path
    // — it is here because a replayed or hand-built row can carry anything, and a
    // zero-keyed row would be a permanent phantom in a table keyed by address.
    let Some(deal) = crate::inference_projectors::non_zero_address(Some(deal.as_str())) else {
        warn!(
            msg_id = %node.msg_id,
            "InferenceDealClosed carried the zero address as its deal; nothing to settle"
        );
        return Ok(ProjectionOutcome::Applied);
    };
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    let chain_order = node_chain_order(node, "InferenceDealClosed")?;
    // UPSERT, not UPDATE, and NOT `Deferred` when the row is missing. The deal row
    // is created by `InferenceOrderBook.InferenceFilled`, which precedes this on
    // chain, so normally the row is here and this is an update. It can legitimately
    // be absent in exactly one case — the fill happened before this indexer's
    // captured history — and there deferring would be wrong twice over: the parent
    // is not late but unreachable, and `InferenceDealClosed` is not in the
    // dead-letter allow-list, so the row would stay pending forever. Recording a
    // skeleton keeps the one fact this event carries; the columns it cannot fill
    // stay NULL, which is what they mean.
    sqlx::query(
        r#"insert into inference_deals (token_contract_address, settled_at_chain, last_chain_order)
           values ($1, to_timestamp($2::double precision), $3)
           on conflict (token_contract_address) do update
              set settled_at_chain = coalesce(inference_deals.settled_at_chain, excluded.settled_at_chain),
                  last_chain_order = greatest(coalesce(inference_deals.last_chain_order, ''), excluded.last_chain_order),
                  updated_at = now()"#,
    )
    .bind(deal)
    .bind(chain_seconds)
    .bind(&chain_order)
    .execute(&mut **tx)
    .await
    .context("apply InferenceDealClosed")?;
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
