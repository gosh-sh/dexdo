-- migrations/0002_orders_owner_index.sql
--
-- Owner order reads cover all public statuses
-- (NEW / PARTIALLY_FILLED / FILLED / CANCELED / REJECTED), so status
-- filtering is a heap-side predicate over an owner/cursor seek range;
-- per-owner cardinalities are small enough that this is cheaper than
-- maintaining a wider composite index.
--
-- The partial predicate is intentionally narrower than the SQL
-- WHERE clause: the runtime query also checks
-- `chain_updated_at IS NOT NULL`, but the index omits that conjunct.
-- Rationale: the invariant
--   `chain_created_at IS NOT NULL ⇒ chain_updated_at IS NOT NULL`
-- holds because `OrderPlaced` initialises both timestamps from the
-- same gateway value (both NULL or both non-NULL), and subsequent
-- `OrderFilled` / `OrderCancelled` events update only
-- `chain_updated_at` via `greatest(existing, to_timestamp(...))`,
-- which preserves a non-NULL `chain_updated_at` even when the new
-- event has no parseable chain time. Including the second conjunct
-- in the partial predicate would force index maintenance on every
-- `OrderFilled` without buying selectivity. The runtime check stays
-- as a heap filter for defence in depth against the rare ingestion
-- path where the gateway omits `created_at` on an edge.
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
