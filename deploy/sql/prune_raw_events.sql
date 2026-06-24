-- ============================================================================
--  raw_events retention via pg_cron (Supabase)
-- ----------------------------------------------------------------------------
--  Why: raw_events is an append-only event log and the single biggest table.
-- The live projection loop only needs rows with processed_at IS NULL; everything
-- already projected is kept solely for reprojection / on-call heals within a
-- retention window. This job deletes *processed* rows older than that window,
-- in small batches so it never holds a long lock on the hot table the indexer
-- is writing to.
--
--  Trade-off: after a row is pruned you can no longer rebuild the read-model
--  "from scratch" deeper than the window. Pending (un-projected) rows are NEVER
--  deleted, so reprojection of the live backlog is unaffected.
--
--  Run this whole file ONCE as a privileged role (Supabase SQL Editor runs as
--  `postgres`). pg_cron must live in the `postgres` database -- which is where
--  this schema already is. Schedule times are UTC.
-- ============================================================================

-- 1) Enable pg_cron (no-op if already enabled via Dashboard -> Database -> Extensions).
create extension if not exists pg_cron;

-- 2) Batched, lock-friendly purge procedure.
--    Drives deletion off the bigserial PK: created_at is monotonic with id, so
--    one id ceiling bounds the time window without scanning the whole table per
--    batch, and each batch is a short PK-range delete + COMMIT.
create or replace procedure public.prune_raw_events(
    retention   interval default interval '3 days',
    batch_size  bigint   default 10000
)
language plpgsql
as $$
declare
    cutoff_id bigint;
    lo_id     bigint;
    hi_id     bigint;
    deleted   bigint;
    total     bigint := 0;
begin
    -- Single-run guard: skip if a previous (slow) run is still draining.
    if not pg_try_advisory_lock(hashtext('prune_raw_events')) then
        raise notice 'prune_raw_events: another run holds the lock, skipping';
        return;
    end if;

    -- Highest id that is safely older than the retention window.
    select max(id) into cutoff_id
      from raw_events
     where created_at < now() - retention;

    if cutoff_id is null then
        raise notice 'prune_raw_events: nothing older than %, skipping', retention;
        perform pg_advisory_unlock(hashtext('prune_raw_events'));
        return;
    end if;

    select min(id) into lo_id from raw_events;

    while lo_id is not null and lo_id <= cutoff_id loop
        hi_id := least(lo_id + batch_size - 1, cutoff_id);

        delete from raw_events
         where id between lo_id and hi_id
           and processed_at is not null;     -- never drop un-projected rows

        get diagnostics deleted = row_count;
        total := total + deleted;

        commit;                              -- keep each batch a short txn
        lo_id := hi_id + 1;
    end loop;

    raise notice 'prune_raw_events: deleted % rows older than %', total, retention;
    perform pg_advisory_unlock(hashtext('prune_raw_events'));
end;
$$;

-- 3) Schedule: daily at 03:00 UTC, keep 7 days. Re-running cron.schedule with
--    the same job name updates the existing job (idempotent).
select cron.schedule(
    'prune-raw-events',
    '0 3 * * *',
    $$ call public.prune_raw_events(interval '3 days', 10000) $$
);

-- ============================================================================
--  Operations
-- ============================================================================
-- First purge will leave dead tuples; autovacuum reclaims space for reuse.
-- To return disk to the OS after the initial large cleanup, run during a quiet
-- window (VACUUM FULL takes an ACCESS EXCLUSIVE lock -- do NOT put it in cron):
--     vacuum (analyze) raw_events;          -- reclaims for reuse, no hard lock
--     -- vacuum full raw_events;            -- returns disk to OS, locks the table
--
-- Run it manually once now (optional, before waiting for 03:00):
--     call public.prune_raw_events(interval '3 days', 10000);
--
-- Inspect the job + recent runs:
--     select jobid, schedule, command, active from cron.job where jobname = 'prune-raw-events';
--     select status, return_message, start_time, end_time
--       from cron.job_run_details
--      where jobid = (select jobid from cron.job where jobname = 'prune-raw-events')
--      order by start_time desc limit 10;
--
-- Change the window (e.g. to 14 days): re-run the cron.schedule call above with
--     $$ call public.prune_raw_events(interval '14 days', 10000) $$
--
-- Disable / remove:
--     select cron.unschedule('prune-raw-events');
-- ============================================================================
