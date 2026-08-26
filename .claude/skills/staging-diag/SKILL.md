---
name: staging-diag
description: Use when the staging deployment (Supabase Postgres) misbehaves — projection lag or sweep lag is growing, a market or oracle event that exists on-chain is missing from the API/read model, raw_events is bloating disk, or the indexer backlog will not drain.
---

# Staging DB Diagnosis

Diagnose stage top-down: L1 ingest → L2 projection → L3 visibility. Each level's query is the same one the indexer's own metrics use (`crates/infrastructure/src/indexer_repo.rs`), so results match the dashboards.

## Connection — never paste secrets

Read the URL from `~/.config/dodex/stage.env` (var `STAGE_DATABASE_URL`). If the file is missing, ask the user to create it (mode 600) — never paste a password into the chat or transcript. URL template: `docs/deployment.md` (Supabase section, ~line 106): `postgresql://<role>.<project-ref>:<password>@aws-...pooler.supabase.com:5432/postgres`, password percent-encoded (`$`→`%24`, `@`→`%40`).

```bash
set -a; . ~/.config/dodex/stage.env; set +a
PGCONNECT_TIMEOUT=8 psql "$STAGE_DATABASE_URL"
```

The URL as a psql argument is visible in the host process list — acceptable on a single-user machine; otherwise use `~/.pgpass` or a `PGSERVICE` entry.

Repo docs and the stage config use the pooler host on port **5432** (session mode) — use that for interactive psql. Port 6543 is Supabase's transaction pooler and `sslmode=require` is the Supabase default (general Supabase behavior; not in repo docs).

## L1 — Is the indexer receiving events?

```sql
-- coalesce: created_at_chain is nullable (gateway may omit block time);
-- max over the raw column alone can fake a dead ingest.
select now() - max(coalesce(created_at_chain, created_at)) as ingest_age from raw_events;
select stream_name, at_head, now() - updated_at as cursor_age
  from indexer_cursors where stream_name = 'blockchain_events';
```

Interpretation: `cursor_age` growing = indexer down or gateway unreachable. `cursor_age` fresh but `ingest_age` old = chain quiet, or ingest filters dropping everything. `at_head = false` = still paging history — this also holds the sweep catch-up gate closed.

## L2 — Projection backlog

```sql
-- Exact predicate of the projection loop (= backlog + lag metrics).
select count(*) as backlog,
       now() - min(coalesce(created_at_chain, created_at)) as oldest_pending
  from raw_events
 where processed_at is null and event_type is not null and decoded is not null;

-- Per-type breakdown; min(chain_order) points at the stuck front row.
select event_type, count(*), min(chain_order)
  from raw_events
 where processed_at is null and event_type is not null and decoded is not null
 group by 1 order by 2 desc;

-- Decode failures (no separate error table: undecoded rows have event_type IS NULL).
select count(*) as undecodable, max(created_at) as latest from raw_events where event_type is null;
```

Interpretation: backlog growing with old `oldest_pending` = projection loop stuck/slow — check indexer logs at the `min(chain_order)` row. A jump in `undecodable` after a contract upgrade = decoder ABI drift (those rows are never projected and never counted in backlog).

**Sweep lag (inference books):**

```sql
select now() - min(reference_price_at) as price_lag,
       now() - min(last_swept_at)      as sweep_lag
  from inference_markets where last_reconciled_at is not null;

select orderbook_address, last_swept_at, sweep_cursor, reconcile_attempts,
       last_reconcile_failed_at, last_reconcile_error
  from inference_markets where last_reconciled_at is not null
 order by last_swept_at asc nulls first limit 5;
```

Interpretation: one stuck book drives `min()`. A sweep also waits on `at_head = true` and no pending raw_events for that book's `src_address` — so an L2 backlog stalls sweeps too.

## L3 — Why is market X not visible?

```sql
-- Prediction market. Row missing => PMPDeployed never projected (go back to L2;
-- also check oracle_events.confirmed_pmp_address for the event->PMP link).
select pmp_address, market_id, name, approved, last_reconciled_at,
       last_reconcile_failed_at, reconcile_attempts, orderbook_address
  from markets where market_id = '<id>' or pmp_address = '<addr>';

-- Inference market lifecycle buckets (= indexer_inference_markets metric).
select count(*) filter (where last_reconciled_at is null and last_reconcile_failed_at is null and superseded_at is null) as discovering,
       count(*) filter (where last_reconciled_at is not null and superseded_at is null) as visible,
       count(*) filter (where last_reconciled_at is null and last_reconcile_failed_at is not null and superseded_at is null) as failing
  from inference_markets;

-- Targeted per-book lookup.
select orderbook_address, last_reconciled_at, last_reconcile_failed_at,
       reconcile_attempts, superseded_at, created_at_chain
  from inference_markets where orderbook_address = '<addr>';
```

Row absent for `<addr>` => the discovery event was never projected — check `raw_events` by `src_address = '<addr>'`, then go back to L2.

Interpretation: `last_reconciled_at IS NULL` = invisible to the read API regardless of `approved`. `reconcile_attempts` climbing with `last_reconcile_failed_at` set = reconciler failing (bad address, ABI drift). `superseded_at` set = book retired as a stale duplicate of a higher-version book for the same model.

## Maintenance — raw_events bloat

Pruning lives in `deploy/sql/prune_raw_events.sql`: a pg_cron job `prune-raw-events` (daily 03:00 UTC) calls `public.prune_raw_events(interval '3 days', 10000)` — batched id-range deletes of **processed** rows older than 3 days, advisory-lock guarded; pending rows are never deleted.

**Read-only checks:**

```sql
-- NOTE: the count(*) filters are a full scan — can be slow/heavy on a bloated table.
select pg_size_pretty(pg_total_relation_size('raw_events')) as total,
       count(*) filter (where processed_at is not null) as processed,
       count(*) filter (where processed_at is null)     as pending
  from raw_events;

select jobid, schedule, active from cron.job where jobname = 'prune-raw-events';
select status, return_message, start_time from cron.job_run_details
 where jobid = (select jobid from cron.job where jobname = 'prune-raw-events')
 order by start_time desc limit 5;
```

**Remediation — mutates data, confirm with the user first** (out of scope for pure diagnosis):

```sql
call public.prune_raw_events(interval '3 days', 10000);  -- manual prune run
vacuum (analyze) raw_events;                              -- after a big purge
```

## Common mistakes

| Mistake | Reality |
|---|---|
| Backlog = `processed_at IS NULL` | Counts undecodable rows the loop never picks up. Use the full predicate (`event_type`/`decoded` not null). |
| Lag from `created_at` alone | `created_at` = local ingest wall-clock; `created_at_chain` = block time (nullable). Use `coalesce(created_at_chain, created_at)`. |
| Ordering by `id`/`created_at` | `chain_order` (lex-sortable text) is the sole projection-ordering key. |
| Judging visibility by `approved` | Read API gates on `last_reconciled_at IS NOT NULL` (`markets` and `inference_markets`). |
| Counting inference books without `superseded_at IS NULL` | Superseded books are excluded from the API and metrics. |
| "Ingest is broken — no `OrderBook.Queued` / `PMP.StakeAccepted` rows" | Stage drops those types before insert (`config/indexer.stage.supabase.yaml` `ignored_event_types`). |
| Blaming the dapp scope filter | `indexer.dapp_id` is unset in the stage config — no dapp filtering on stage; edges without `src_dapp_id` are always kept. |
| `VACUUM FULL` during ops/cron | ACCESS EXCLUSIVE lock on the hot table. Use `vacuum (analyze)`. |
