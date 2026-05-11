-- The OEL reconciler picked pending rows by `describe is null or trust_addr is
-- null`. Both fields are nullable on chain (`describe` is a string that can be
-- empty; `trustAddr` is declared `optional(uint256)` on `OracleEventList`), so
-- a row whose chain-side metadata is legitimately empty left the predicate
-- true forever — the `LIMIT 16` batch kept reselecting the same OELs every
-- sweep and starved later event lists. Replace value-of-field with a
-- dedicated reconciler-progress marker.
--
-- Backfill: any row with describe or trust_addr already populated was
-- reconciled by this code path. Confirmed writers of these columns are
-- limited to `oracle_event_list_reconciler.rs`; `projectors.rs` (EventAdded /
-- EventConfirmed) only touches event_name, oracle_fee, deadline,
-- confirmed_pmp_address, confirmed_at.
--
-- Rows whose on-chain metadata is genuinely null end the migration with
-- meta_reconciled_at = NULL and get picked up by the next sweep — the
-- updated UPDATE writes meta_reconciled_at unconditionally, so they fall
-- out of the queue after one more reconcile pass.

alter table oracle_events
    add column if not exists meta_reconciled_at timestamptz;

update oracle_events
   set meta_reconciled_at = updated_at
 where meta_reconciled_at is null
   and (describe is not null or trust_addr is not null);

-- Migration 0010 created `oracle_events_pending_meta_idx` over the old
-- predicate. Drop and recreate against the new marker so the partial index
-- still matches the reconciler's SELECT.
drop index if exists oracle_events_pending_meta_idx;

create index if not exists oracle_events_pending_meta_idx
    on oracle_events (eventlist_id)
    where meta_reconciled_at is null;
