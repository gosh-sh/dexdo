# Market Data — Indexer: Backend Notes

Implementation-facing notes for the indexer side of the market-data path. This document covers how data gets *into* the read-model: ingestion from events stream, projection of contract events, and reconciliation of fields that events alone do not carry. The HTTP layer that serves these tables is described in [market-data-api.md](market-data-api.md). Postgres tables referenced below are documented column-by-column in [data-schema.md](data-schema.md).

## Ingestion

The indexer follows a GraphQL message-edge stream. Every edge becomes one row in [`raw_events`](data-schema.md#raw_events) regardless of whether it could be decoded — the raw log is the recovery boundary, and any downstream table can be rebuilt from `raw_events` plus a clean schema.

Sequence per edge:

1. Try to decode the message body against the ABI bundle (`crates/infrastructure/src/decoder.rs`). On success, store the decoded JSON payload alongside `event_type`.
2. Persist the row in `raw_events` with `processed_at = NULL`. The unique `msg_id` constraint deduplicates overlapping page fetches.
3. If decoding produced a known event, dispatch the projector inside the same transaction. The projector outcome decides whether `processed_at` is stamped now (`Applied`, `Unknown`) or left null for retry (`Deferred`).
4. After the page commits, persist the resume cursor in [`indexer_cursors`](data-schema.md#indexer_cursors). A restart resumes from this cursor — already-projected rows are not replayed.

## Projection — lifecycle events

Lifecycle events drive transitions on [`markets`](data-schema.md#markets) and the [`oracles`](data-schema.md#oracles) / [`oracle_event_lists`](data-schema.md#oracle_event_lists) / [`oracle_events`](data-schema.md#oracle_events) hierarchy. Each projector identifies its row by `pmp_address` (or the relevant parent address); if that row does not exist yet, the projector returns `Deferred` so the reprojection loop will retry once the parent event has landed.

| Event | Read-model effect |
| --- | --- |
| `RootOracle.OracleDeployed` | Inserts into [`oracles`](data-schema.md#oracles). Sets `address`, `name`, `pubkey`. |
| `Oracle.OracleEventListDeployed` | Inserts [`oracle_event_lists`](data-schema.md#oracle_event_lists) under the parent oracle. |
| `OracleEventList.EventAdded` | Upserts [`oracle_events`](data-schema.md#oracle_events) with `event_name`, `oracle_fee`, `deadline`. Does NOT carry `describe` or `trust_addr` — those come from the OEL reconciler. |
| `OracleEventList.EventConfirmed` | Stamps `oracle_events.confirmed_pmp_address` and `confirmed_at`. Links an event to the PMP that will market it. |
| `PrivateNote.PMPDeployed` | Inserts a row in [`markets`](data-schema.md#markets) with `pmp_address`, `event_id`, `token_type`, `token_code`. Lifecycle columns (`stake_*`, `result_*`, `frozen_at`, etc.) stay NULL — they belong to later events. The row is invisible to the API until the reconciler stamps `last_reconciled_at`. |
| `PMP.TimingsSet` | Updates `stake_start`, `stake_end`, `result_start`, `result_end`, sets `approved = true`. May fire repeatedly while `now < resultStart` — keep the latest by block time. This projector is the **sole writer** of the four timing columns. |
| `PMP.PoolsFrozen` | Sets `frozen_at` via `coalesce` (never overwritten). This is the on-chain signal that the OrderBook contract has been deployed (see [dex-events-routing.md](../dex-events-routing.md): "after deploy OrderBook"). |
| `PMP.Resolved` | Sets `resolved_at` and `resolved_outcome_id`. |
| `PMP.PMPCancelled` | Sets `is_cancelled = true`, `cancelled_at`, `cancel_reason = 'PMP_CANCELLED'`. |
| `PMP.EventCancelled` | Same shape but `cancel_reason = 'EVENT_CANCELLED'`. The two reasons distinguish cancellation source and have different UI meaning. |

## Projection — order events

OrderBook events drive [`live_orders`](data-schema.md#live_orders), the per-order read-model that backs `/api/v1/depth`. Three events mutate state; five more are observability-only.

| Event | Effect |
| --- | --- |
| `OrderBook.OrderPlaced` | Upserts into `live_orders` with `status = 'OPEN'` and full `amount_remaining`. `last_event_lt` set to the chain timestamp. A conflict on `(orderbook_address, order_id)` resets the row to OPEN. |
| `OrderBook.OrderFilled` | Decrements `amount_remaining` by `filledAmount`. Flips `status` to `FILLED` when the remainder reaches zero. Updates `last_event_lt` via `greatest(existing, new)`. |
| `OrderBook.OrderCancelled` | `status = 'CANCELLED'`, `amount_remaining = 0`, monotonic `last_event_lt` update. |
| `OrderBook.PartialFill` / `FullyFilled` / `Queued` / `Rejected` / `CallbackBounced` | Observability-only. The row is recorded in `raw_events` for audit but no read-model table is touched. |

`PartialFill` / `FullyFilled` are derived aggregates the contract emits for MM-friendly UX; the underlying state is already captured by `OrderFilled`. `Queued` / `Rejected` happen at queue level, before any order id is assigned. `CallbackBounced` is a diagnostic — OrderBook state is not auto-rolled back, and the bounced credit needs operator-driven recovery.

Out-of-order delivery from the chain is handled exclusively through the `greatest(existing, new)` clause on `last_event_lt`. The depth handler relies on this for a monotonic `lastUpdateId`; see [market-data-api.md](market-data-api.md#lastupdateid).

## Reconciliation

Two reconcilers fill metadata that the event stream alone does not carry. Both run on a fixed cadence (`reconciliation_interval_ms`, `oracle_event_list_reconciliation_interval_ms` in `config/indexer.*.yaml`) and share a failure-backoff pattern (`last_reconcile_failed_at`, `reconcile_attempts` on the parent row) so a permanently broken contract cannot starve the queue.

### Market reconciler

For each [`markets`](data-schema.md#markets) row that needs catch-up, the reconciler:

1. Fetches the PMP account BOC from chain.
2. Runs `PMP.getDetails()` off-chain through the local TVM emulator (`crates/infrastructure/src/tvm_runner.rs`).
3. Runs `PMP.getOrderBookAddress()` the same way.
4. Writes `market_id`, `name`, `oracle_list_hash`, `approved`, `is_cancelled`, `num_outcomes`, and outcome rows in [`market_outcomes`](data-schema.md#market_outcomes).
5. Stamps `last_reconciled_at`. The market becomes visible to the API only after this point.

Two invariants the reconciler enforces on the write side:

- **`orderbook_address` is only stamped when `frozen_at IS NOT NULL`.** The getter is deterministic and would return a precomputed address even pre-freeze, but the spec requires the read-model to expose it only after on-chain observation. `PoolsFrozen` is the observation signal (emitted after the OrderBook deploys). Migration 0013 backfilled legacy pre-freeze values to NULL.
- **Timing columns (`stake_*`, `result_*`) are never written by the reconciler.** On pre-`TimingsSet` PMPs `getDetails()` returns contract-default zeros; copying those would make the API flip out of PENDING. The `TimingsSet` projector is the sole writer of those columns.

Queue ordering (the SELECT in `MarketReconciler::run_once`):

- A row enters the queue when `last_reconciled_at IS NULL` (never reconciled) OR when `frozen_at IS NOT NULL AND orderbook_address IS NULL` (`PoolsFrozen` landed since the last pass — re-queue to stamp the address).
- Failed rows go to the back via `nulls first` ordering on `last_reconcile_failed_at`. A 5-minute backoff filter excludes recently-failed rows entirely so they don't dominate the batch.

### OEL reconciler

For each [`oracle_event_lists`](data-schema.md#oracle_event_lists) row that has at least one event still missing reconciler-only metadata, the OEL reconciler runs `OracleEventList._events` and fills `describe` / `trust_addr` per event.

Key column: [`oracle_events.meta_reconciled_at`](data-schema.md#oracle_events). The reconciler stamps this **unconditionally** on every successful pass — even when the on-chain `trustAddr` is legitimately null, the marker is set so the row drops out of the pending queue. The marker replaced an earlier `describe IS NULL OR trust_addr IS NULL` predicate that never cleared for events with null on-chain metadata; the change shipped in migration 0012 with a backfill that stamps `meta_reconciled_at` for rows that already had either field populated.

The pending-row predicate is:

```sql
exists (select 1 from oracle_events oe
         where oe.eventlist_id = oel.id
           and oe.meta_reconciled_at is null)
```

## Failure handling

Two outcomes leave a `raw_events` row pending:

- **`Deferred`** — the projector knows it cannot apply this event yet (typically a child arriving before its parent). `processed_at` stays NULL and the reprojection loop retries the row on every tick.
- **`Err`** — the projector hit an unexpected error. Same effect on `processed_at`, plus a warn log and an increment in the failure counter. Useful for spotting ABI drift.

The reprojection loop (`indexer_repo.rs::reproject_pending`) picks pending rows in chain-arrival order, uses `for update skip locked` to coexist with the main fetch loop, and reuses the already-decoded payload from `raw_events.decoded` — bodies are not re-decoded.

Reconciler-side failures use a separate mechanism — `last_reconcile_failed_at` and `reconcile_attempts` on the [`markets`](data-schema.md#markets) and [`oracle_event_lists`](data-schema.md#oracle_event_lists) rows. The 5-minute backoff window prevents a permanently broken `getDetails()` from blocking the batch every tick.

## Schema invariants — write side

| Invariant | Enforced by |
| --- | --- |
| `markets.orderbook_address IS NOT NULL ⇒ frozen_at IS NOT NULL` | Reconciler `CASE WHEN frozen_at IS NOT NULL THEN $X ELSE orderbook_address END` clause. Backfilled by migration 0013. |
| Lifecycle timings (`stake_*`, `result_*`) are projector-only | Reconciler does not write these columns. |
| `oracle_events.meta_reconciled_at` set after every successful reconciler pass | OEL reconciler UPDATE always stamps it. |
| `live_orders.last_event_lt` monotonic per row | `greatest(existing, new)` on every UPDATE. |
| Cancellation reason matches its source | Projector picks `PMP_CANCELLED` or `EVENT_CANCELLED` based on event type, never NULL. |

The API enforces complementary read-side invariants on the assembled DTO — see [market-data-api.md](market-data-api.md#fail-closed-validation). Together they guarantee that an inconsistent indexer state (e.g. `PMP.Resolved` indexed before `PoolsFrozen`) cannot leak into a client response.

## Visibility gate

A market is visible to the public API only when `markets.last_reconciled_at IS NOT NULL`. Until the market reconciler runs, the row exists internally (`PMPDeployed` inserted it) but is hidden — clients see consistent, fully-populated markets only.

This pairs with the API's PENDING short-circuit: a row with NULL timing columns (reconciled but pre-`TimingsSet`) surfaces as `status = "PENDING"` with `timings = null`, per [market-data-api.md](market-data-api.md#status-derivation).
