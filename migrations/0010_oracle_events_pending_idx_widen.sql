-- The OEL reconciler used to look only at `describe is null`, which matched
-- migration 0008's partial index exactly. After widening the sweep predicate
-- to `describe is null or trust_addr is null` (an event with describe set but
-- trust_addr still missing was being orphaned), we replace the partial index
-- with one that matches the new predicate. Without this the planner falls
-- back to a seq-scan on `oracle_events` for the trust_addr branch once
-- `describe` is broadly populated.

drop index if exists oracle_events_describe_pending_idx;

create index if not exists oracle_events_pending_meta_idx
    on oracle_events (eventlist_id)
    where describe is null or trust_addr is null;
