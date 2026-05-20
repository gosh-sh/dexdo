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
-- See docs/tech-specs/read-api.md §Index reliance.

drop index if exists live_orders_open_owner_idx;

create index live_orders_owner_idx
    on live_orders (owner_pn_address, placed_chain_order desc)
    where owner_pn_address is not null
      and chain_created_at is not null;
