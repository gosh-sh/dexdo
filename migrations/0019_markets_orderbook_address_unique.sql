-- Pins the contract-side invariant at the DB level: each `markets` row has
-- its own OrderBook contract, so `orderbook_address` is unique among
-- reconciled rows. `PMP.getOrderBookAddress()` is deterministic per market
-- (see migration 0014), so duplicates can only arise from operator error or
-- a reconciler regression. Without this constraint, the all-markets variant
-- of `/api/v1/openOrders` (which joins `markets m on m.orderbook_address =
-- lo.orderbook_address` without a per-pmp filter) would silently fan out
-- every order across every market sharing the address.
--
-- Pre-production-safe like 0018: the non-`CONCURRENTLY` create takes a
-- ShareLock on `markets` for the duration of the build. `markets` is small
-- in pre-prod and the project has no deployed indexer yet, so this is
-- unobservable. A prod migration would need `create unique index
-- concurrently`.
--
-- Partial on `orderbook_address IS NOT NULL` so pre-reconcile rows (whose
-- column is legitimately NULL until the first reconciler pass — see 0014)
-- don't collide on the index.

create unique index if not exists markets_orderbook_address_unique
    on markets (orderbook_address)
    where orderbook_address is not null;
