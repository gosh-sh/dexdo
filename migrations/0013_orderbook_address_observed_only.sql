-- The reconciler used to copy `orderbook_address` from
-- `PMP.getOrderBookAddress()` on every pass. That getter is deterministic
-- and returns the precomputed address even before the OrderBook contract is
-- observed on chain (`contracts/PMP.sol:1360`), which violates
-- tech-spec.md invariant #5: `orderBookAddress` MUST be null until the
-- OrderBook is observed.
--
-- After the fix the reconciler only stamps the column when
-- `frozen_at is not null` (PoolsFrozen fires *after* the OrderBook deploys,
-- see docs/dex-events-routing.md:77). Existing rows seeded under the old
-- behaviour can carry a non-null `orderbook_address` while
-- `frozen_at is null`; this backfill clears those so the read-model matches
-- the new contract. Once `frozen_at` lands the reconciler's widened SELECT
-- predicate picks the row up and re-stamps the column.
update markets
   set orderbook_address = null
 where frozen_at is null
   and orderbook_address is not null;
