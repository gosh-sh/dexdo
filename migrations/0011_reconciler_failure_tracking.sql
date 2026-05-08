-- Failure tracking for the market & OEL reconcilers. Without this both
-- reconcilers always pick the lowest-id pending rows and starve out everything
-- behind a small set of permanently broken contracts (getDetails / _events
-- failing on a parse mismatch, missing account boc, etc). Failed rows now get
-- pushed to the back of the queue (`order by last_reconcile_failed_at nulls
-- first`) and skipped entirely during a cooldown window enforced in the
-- reconciler SQL.

alter table markets
    add column if not exists last_reconcile_failed_at timestamptz,
    add column if not exists reconcile_attempts integer not null default 0;

alter table oracle_event_lists
    add column if not exists last_reconcile_failed_at timestamptz,
    add column if not exists reconcile_attempts integer not null default 0;
