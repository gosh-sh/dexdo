-- Reverses migration 0013's "observed-only" policy: the PMP getter
-- `getOrderBookAddress()` is deterministic and returns the precomputed
-- address even before `PoolsFrozen` (`contracts/PMP.sol:1360`), so a
-- reconciled market always has a known address. Hiding it pre-freeze
-- broke the public api-spec.md contract (`orderBookAddress` is null only
-- when the backend has not resolved the address yet).
--
-- Two steps:
--   1. Re-queue every row that 0013 cleared by un-stamping
--      `last_reconciled_at`. The next reconciler pass will run
--      `getOrderBookAddress()` and write the deterministic value.
--   2. Add a CHECK constraint that pins the new invariant: any row
--      visible to the API (`last_reconciled_at IS NOT NULL`) must carry
--      `orderbook_address`. The pre-reconcile window between `PMPDeployed`
--      and the first reconciler pass remains the only state where the
--      column is legitimately null — and such rows are invisible to the
--      API via the `last_reconciled_at IS NOT NULL` visibility filter.
update markets
   set last_reconciled_at = null
 where last_reconciled_at is not null
   and orderbook_address is null;

alter table markets
  add constraint markets_orderbook_address_set_after_reconcile
  check (last_reconciled_at is null or orderbook_address is not null);
