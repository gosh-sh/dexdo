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
        "StreamStopped" => apply_close(tx, node, "STOPPED", true).await,
        "DisputeResolved" => apply_close(tx, node, "DISPUTE_RESOLVED", false).await,
        "StreamReclaimed" => apply_close(tx, node, "RECLAIMED", false).await,
        "ContractDestroyed" => apply_destroyed(tx, node).await,
        "StreamDisputed" => apply_disputed(tx, node).await,
        // Probe-tick + withdrawal events carry no deal-level state the SETTLEMENT
        // read-model needs; the skeleton seed already recorded the deal.
        "ProbeCommissionFunded" | "ProbeAccepted" | "ProbeBurned" | "ShellWithdrawn" => {
            Ok(ProjectionOutcome::Applied)
        }
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

async fn apply_stream_funded(
    tx: &mut Transaction<'_, Postgres>,
    event: &DecodedEvent,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("StreamFunded: src missing")?;
    let buyer = field_str(&event.value, "buyer")?;
    let deposit = uint_field_to_decimal(&event.value, "deposit")?;
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

async fn apply_destroyed(
    tx: &mut Transaction<'_, Postgres>,
    node: &EventNode,
) -> anyhow::Result<ProjectionOutcome> {
    let tc = node.src.as_deref().context("ContractDestroyed: src missing")?;
    let chain_seconds = parse_unix_seconds(node.created_at.as_ref());
    sqlx::query(
        r#"update inference_deals
              set settled_at_chain = coalesce(settled_at_chain, to_timestamp($2::double precision)),
                  close_kind = coalesce(close_kind, 'DESTROYED'),
                  updated_at = now()
            where token_contract_address = $1"#,
    )
    .bind(tc)
    .bind(chain_seconds)
    .execute(&mut **tx)
    .await
    .context("apply ContractDestroyed")?;
    Ok(ProjectionOutcome::Applied)
}
