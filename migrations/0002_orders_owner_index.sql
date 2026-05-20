-- migrations/0002_orders_owner_index.sql
--
-- /api/v1/orders supersedes the OPEN-only /api/v1/openOrders. The new
-- endpoint returns rows across all five public statuses
-- (NEW / PARTIALLY_FILLED / FILLED / CANCELED / REJECTED), so the
-- partial predicate on `status = 'OPEN' AND amount_remaining > 0`
-- no longer fits. Status filtering becomes a heap-side predicate;
-- per-owner cardinalities are small enough that this is cheaper than
-- maintaining a wider composite index.
--
-- The partial predicate is intentionally narrower than the SQL
-- WHERE clause: the runtime query also checks
-- `chain_updated_at IS NOT NULL`, but the index omits that conjunct.
-- Rationale: the projector writes `chain_created_at` and
-- `chain_updated_at` together (greatest(...) updates on every
-- order-book event, so once `chain_created_at` is non-null
-- `chain_updated_at` is non-null as well). Including the second
-- conjunct in the partial predicate would force index maintenance on
-- every `OrderFilled` (which advances `chain_updated_at`) without
-- buying selectivity. The runtime check stays as a heap filter for
-- defence in depth against the rare ingestion path where the gateway
-- omits `created_at` on an edge.
--
-- `CREATE INDEX` (no `CONCURRENTLY`) matches the rest of the project's
-- sqlx-driven migrations, which run each file inside a transaction —
-- `CREATE INDEX CONCURRENTLY` is illegal in a transaction. On a hot
-- `live_orders` table the deploy briefly takes a `SHARE` lock; that is
-- the trade-off sqlx's transactional model imposes. If a future deploy
-- needs zero-downtime here, switch the migration runner to a
-- non-transactional path before changing this statement.
--
-- See docs/tech-specs/read-api.md §Index reliance.

drop index if exists live_orders_open_owner_idx;

create index live_orders_owner_idx
    on live_orders (owner_pn_address, placed_chain_order desc)
    where owner_pn_address is not null
      and chain_created_at is not null;
