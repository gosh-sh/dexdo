---
name: indexer-db-diag
description: Use when a dodex indexer database misbehaves, on stage OR prod — projection lag or sweep lag is growing, a market or oracle event that exists on-chain is missing from the API/read model, raw_events is bloating disk, undecodable rows appear, or the indexer backlog will not drain.
---

# Indexer DB Diagnosis

Diagnose top-down: L1 ingest → L2 projection → L3 visibility. Each level's query is the same one the indexer's own metrics use (`crates/infrastructure/src/indexer_repo.rs`), so results match the dashboards.

Both environments run the same migrations and the same schema, so **every query below is identical on stage and prod**. What differs is what the numbers MEAN — see [Reading the numbers per environment](#reading-the-numbers-per-environment). Ignoring that section is how a normal prod reads as an outage and a real one reads as normal.

## Environment — pick one deliberately

| | stage | prod |
|---|---|---|
| Credentials | `~/.config/dodex/stage.env` → `STAGE_DATABASE_URL` | `~/.config/dodex/prod.env` → `PROD_DATABASE_URL` |
| Chain | shellnet | mainnet |
| Writes | allowed after confirming with the user | **never — read-only, no exceptions** |

**Default to stage.** Only touch prod when the user names it, and say which one you are on in your first reply — a number reported without its environment is worse than no number.

**Prod is read-only.** Run `select` only. The Remediation section at the end does not apply to prod: `call prune_raw_events(...)` and `vacuum` are real operations against live data, and nothing in a diagnosis needs them. If prod turns out to need a mutation, hand the statement to the user rather than running it.

If the env file is missing, ask the user to create it (mode 600) — never paste a password into the chat or transcript. URL template: `docs/deployment.md` (Supabase section, ~line 106): `postgresql://<role>.<project-ref>:<password>@aws-...pooler.supabase.com:5432/postgres`, password percent-encoded (`$`→`%24`, `@`→`%40`).

```bash
set -a; . ~/.config/dodex/stage.env; set +a      # or prod.env → PROD_DATABASE_URL
PGCONNECT_TIMEOUT=8 psql "$STAGE_DATABASE_URL"
```

The URL as a psql argument is visible in the host process list — acceptable on a single-user machine; otherwise use `~/.pgpass` or a `PGSERVICE` entry.

Both indexer configs (`config/indexer.stage.supabase.yaml`, `~/.config/dodex/indexer.prod.yaml`) use the pooler host on port **5432** (session mode) — use that for interactive psql. `sslmode=require` is the Supabase default (general Supabase behaviour; not in repo docs).

Session mode has 15 slots and they do run out, including through no fault of yours — a running indexer holds some. The symptom is `FATAL: (EMAXCONNSESSION) max clients reached in session mode - max clients are limited to pool_size: 15`. Port **6543** is the transaction pooler and has its own budget, so rewrite the port rather than waiting:

```bash
TX=$(printf '%s' "$STAGE_DATABASE_URL" | sed 's#:5432/#:6543/#')   # never echo either URL
PGCONNECT_TIMEOUT=10 psql "$TX" -c '...'
```

Transaction mode does not keep session state (no `SET`, no advisory locks held across statements), which none of the queries below need.

## Reading the numbers per environment

The queries are identical; these four readings are not. Each is a case where the honest answer on one environment is an incident on the other.

| Reading | stage | prod |
|---|---|---|
| Old `ingest_age` | suspicious — e2e drives near-constant traffic | **normal** — mainnet is nearly idle |
| `markets` empty | broken — prediction markets live here | **normal** — none are deployed to mainnet |
| `undecodable > 0` | drift, act on it | check the DATE first, see below |
| Remediation section | applies | **does not apply** |

**Chain liveness.** Mainnet DEX-dapp activity is close to zero: a gateway query from a 2026-08-01 cursor returned a single event. So on prod an `ingest_age` of days means the chain is quiet, not that ingest died — the discriminator is `cursor_age`, which must stay fresh regardless of whether anything is arriving. On stage the two move together, so a quiet `ingest_age` there is worth chasing.

**Undecodable on prod carries an immovable historical population.** The L2 rule below — any undecodable row means drift — is about rows arriving *now*. Prod also holds 31085 rows on dst 101 (`RootPN.PrivateNoteDeployed`), every one ingested on 2026-08-20 by the initial backfill, carrying a signature id from an older contract generation. The same id decodes cleanly on stage, so this is not a loaded-ABI problem; those bodies simply predate the current ABI and there is no re-decode path, so they will never resolve, never project and never become prunable. Judge prod drift by `max(created_at)` and by date buckets, never by the total.

**Environments do not run the same build.** A fix merged to `dev` deploys to stage; prod deploys from `main` (`.woodpecker/deploy.yml`). Before concluding that prod behaves differently *by nature*, check whether it simply has an older binary — the decoder's loaded-ABI set is compiled in, so a decode difference between environments is usually a deploy difference.

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

Interpretation: backlog growing with old `oldest_pending` = projection loop stuck/slow — check indexer logs at the `min(chain_order)` row.

**Any undecodable row arriving NOW means ABI drift — there is no benign background.** (On prod, read this together with the historical-backfill caveat above: judge by date, not by total.) Every contract that emits to a scoped `dst` has its ABI loaded into the decoder (`DEX_ABIS` + `INFERENCE_ABIS`, twelve of them, `RootModel` and `SuperRoot` included). Of the ABIs that ship unloaded, `ModelRegistry` declares no events at all and `Multisig` is a wallet outside the ingest scope, so neither can put a row here. A non-zero count therefore means the decoder no longer matches the deployed contracts: rebuild and redeploy.

This is a recent sharpening. Until the registry ABIs were loaded, most undecodable rows were ordinary `RootModel`/`SuperRoot` events and a steady background WAS normal — on stage they were the entire undecodable population, 1058 rows. Text written against that era reads a flat count as harmless; it no longer is.

Undecodable rows are also never projected, never counted in the backlog above, and — because `processed_at` stays NULL forever — permanently out of the pruner's reach. Identify the contract before reporting a count:

```sql
-- Group by the routing id, not the raw address. dst_address holds `:` + 64 zero-padded
-- hex digits, so right(...,16) is the id; a suffix LIKE would also match ordinary
-- contract addresses that happen to end in the same digits.
select ('x' || right(dst_address, 16))::bit(64)::bigint as dst_id,
       count(*), min(created_at)::date as first, max(created_at)::date as last
  from raw_events where event_type is null group by 1 order by 2 desc limit 15;
```

Name the id from the `EVENT_ID` constants in `contracts/*/modifiers/modifiers.sol` — the same set `crates/infrastructure/tests/ingest_scope.rs` re-derives. An id that resolves to a loaded contract, yet decodes to nothing, is a signature-id collision needing a `dst` route in `Decoder::new` rather than a rebuild.

**Sweep lag (inference books):**

```sql
select now() - min(reference_price_at) as price_lag,
       now() - min(last_swept_at)      as sweep_lag
  from inference_markets where last_reconciled_at is not null;

-- There is no error-text column. `inference_markets` records only THAT a reconcile
-- failed (last_reconcile_failed_at) and how often it was retried; the reason is in the
-- indexer log at that timestamp.
select orderbook_address, last_swept_at, sweep_cursor, reconcile_attempts,
       last_reconcile_failed_at, sweep_cycle_max, sweep_override_seq
  from inference_markets where last_reconciled_at is not null
 order by last_swept_at asc nulls first limit 5;

-- min() hides how wide the staleness is. Check the shape before blaming one book.
select count(*) filter (where last_swept_at is null)                    as never_swept,
       count(*) filter (where last_swept_at < now() - interval '7 days') as older_7d,
       count(*) filter (where last_swept_at > now() - interval '1 hour') as fresh_1h,
       count(*)                                                         as total
  from inference_markets where last_reconciled_at is not null;

-- THE ONE THAT MEANS SOMETHING: lag over the books the sweep is actually due to visit.
-- Compare it against the `sweep_lag` above before concluding anything.
select now() - min(m.last_swept_at) as sweep_lag_actionable, count(*) as books_due
  from inference_markets m
 where m.last_reconciled_at is not null and m.superseded_at is null
   and exists (select 1 from inference_orders o
                where o.orderbook_address = m.orderbook_address and o.status = 'OPEN');
```

**`sweep_lag` over all reconciled books is not a health metric.** A book with no `OPEN` order is never re-swept, deliberately: the candidate query in `inference_reconciler.rs` admits a book for sweeping only via `... and exists (select 1 from inference_orders o where ... o.status='OPEN')`, and `run_sweep_step` is gated again on `has_open_orders` — stamping `last_swept_at` for a book with nothing resting would spend `getQueueSize`/`getStats` getters for no work. So `min(last_swept_at)` grows without bound as books finish trading, on every environment, forever. Use `sweep_lag_actionable`; treat the raw `sweep_lag` as a number that only ever moves one way.

Worked example (stage, 2026-09-02): raw `sweep_lag` read **5 days**, and 316 of 522 books had not been swept since the post-wipe catch-up on 08-28 — which looks like a mass stall. Every one of those 316 had **zero** open orders, while the 79 books swept within 20h held 10 between them. `sweep_lag_actionable` over the 10 books actually due: **2m17s**.

**When `sweep_lag_actionable` is NULL it says nothing about liveness — read the two lags against each other instead.** With `books_due = 0` the actionable query returns NULL, which is the right answer to "how stale are the books due a sweep" and no answer at all to "is the reconciler running". The pair does answer it, because Queue B's candidate predicate is an OR of two independent due-conditions (`select_refresh_books_scoped`, `inference_reconciler.rs`):

```
(reference_price_at is null or reference_price_at < now() - reference_price_refresh)
or
((last_swept_at is null or last_swept_at < now() - sweep_interval)
   and exists (select 1 from inference_orders o where ... o.status = 'OPEN'))
```

The `OPEN`-order gate hangs on the sweep half **only**. An idle book is therefore still visited on the price half, at the `reference_price_refresh` interval — it simply leaves `last_swept_at` untouched. So the two lags moving differently is itself the signal:

| `price_lag` | `sweep_lag` | Reading |
|---|---|---|
| advancing — `min` moves forward | grows by exactly the elapsed time | loop alive, sweep idle by design |
| grows by exactly the elapsed time | grows by exactly the elapsed time | Queue B is not turning — suspect the reconciler, not the data |

Both numbers are `now() - min(...)`, so both grow on their own: take two readings minutes apart, or read the timestamps absolutely, and compare the growth against the wall-clock gap between them. Measured on prod, 2026-09-02, two readings ~1h36m apart: `sweep_lag` grew by the full 1h36m while `price_lag` grew by 9m35s. In absolute terms all 8 books carried a `reference_price_at` from the past hour, while `last_swept_at` spanned 08-25 to 08-31. Prod had zero `OPEN` orders anywhere, so `sweep_lag_actionable` was NULL and this pair was the only liveness evidence available.

**`price_lag` carries the same latent flaw `sweep_lag` does.** The shipped metric (`inference_staleness_seconds`, `indexer_repo.rs`) takes `min(reference_price_at)` over `last_reconciled_at is not null` and does **not** exclude superseded books, while the refresh candidate query does — so a superseded book's price timestamp freezes and pins the metric upward for good. Check before believing a large `price_lag`:

```sql
select now() - min(reference_price_at) as price_lag_metric,
       now() - min(reference_price_at) filter (where superseded_at is null) as price_lag_live,
       count(*) filter (where superseded_at is not null) as superseded
  from inference_markets where last_reconciled_at is not null;
```

Both environments held zero superseded books on 2026-09-02, so this one is structural — read off the two queries, not measured in the wild.

Only after `sweep_lag_actionable` itself reads badly is the gate worth checking: a sweep also waits on `at_head = true` and on no pending `raw_events` for the book's `src_address`, so an L2 backlog *can* stall it. Do not assume that link either — join the stale books against pending rows for the same `src_address` and measure. Recorded negative result: on an earlier stage dataset 2732 of 2901 books stale for over 7 days had no pending rows at all.

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

`deploy/sql/prune_raw_events.sql` describes the intended setup: a pg_cron job `prune-raw-events` (daily 03:00 UTC) calling `public.prune_raw_events(interval '3 days', 10000)` — batched id-range deletes of **processed** rows older than 3 days, advisory-lock guarded, pending rows never touched.

That is the repo's intent, not a statement about the database in front of you. **Check that the procedure exists before diagnosing the job**, because the role in `STAGE_DATABASE_URL` can read `pg_proc` but not `cron`:

```sql
-- Step 1. Does the procedure exist at all? Readable by any role.
select n.nspname, p.proname, p.prokind
  from pg_proc p join pg_namespace n on n.oid = p.pronamespace
 where p.proname = 'prune_raw_events';

-- Step 2. Only meaningful if step 1 returned a row. Expect
-- `ERROR: permission denied for schema cron` — see the interpretation below.
select jobid, schedule, active from cron.job where jobname = 'prune-raw-events';
select status, return_message, start_time from cron.job_run_details
 where jobid = (select jobid from cron.job where jobname = 'prune-raw-events')
 order by start_time desc limit 5;

-- Step 3. What pruning would reclaim right now, and the table it would reclaim it from.
-- This works regardless of steps 1-2 and is the only one of the three that measures
-- OUTCOME rather than configuration.
select count(*) as prunable_now from raw_events
 where processed_at is not null and created_at < now() - interval '3 days';

-- NOTE: the count(*) filters below are a full scan — slow on a bloated table.
select pg_size_pretty(pg_total_relation_size('raw_events')) as total,
       count(*) filter (where processed_at is not null) as processed,
       count(*) filter (where processed_at is null)     as pending
  from raw_events;
```

Interpretation. **Step 1 returning zero rows means the pruner is not installed** — no job can be running, whatever `cron` would have said. That is the state BOTH environments were found in on 2026-09-02 — no `prune_raw_events` in `pg_proc` on either, with 16330 rows already eligible on stage; the fix is to apply `deploy/sql/prune_raw_events.sql` — which installs the procedure *and* schedules the job — not to hunt for a broken schedule. It has to run as a privileged role (on Supabase, the SQL editor, which runs as `postgres`); the pooler role can neither create the extension nor schedule jobs. See [`docs/deployment.md`](../../../docs/deployment.md) § raw_events retention. `permission denied for schema cron` in step 2 means "cannot check from here", **not** "the job is missing" — confirm through the Supabase dashboard (Database → Cron) or a privileged role. A `prunable_now` that keeps climbing across days is the outcome-level symptom either way.

**Remediation — STAGE ONLY, mutates data, confirm with the user first** (out of scope for pure diagnosis). On prod, hand these to the user instead of running them:

```sql
call public.prune_raw_events(interval '3 days', 10000);  -- only if step 1 found it
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
| "Ingest is broken — no `OrderBook.Queued` / `PMP.StakeAccepted` rows" | Both environments drop those three types before insert — `ignored_event_types` is byte-identical in `config/indexer.stage.supabase.yaml`, `~/.config/dodex/indexer.prod.yaml` and the ansible template. The list is an allow-list validated at startup, so it cannot grow through a config-only deploy. |
| "There is no dapp filtering, `indexer.dapp_id` is unset" | The config key is indeed unset, but scoping is not done there: the gateway query itself is parameterised by `src_dapp_id` and pinned to the compile-time `DEX_DAPP_ID`, so ingest is dapp-scoped server-side, at fetch. What the unset key means is only that no *additional* local filter runs. |
| Reading a flat `undecodable` count as harmless background | No longer true since the registry ABIs were loaded: nothing that reaches a scoped `dst` is undecodable by design any more, so any row there is drift. |
| `permission denied for schema cron` = "the job is missing" | It means the role cannot look. Check `pg_proc` for the procedure first — that IS readable, and its absence is the answer the `cron` query was being asked for. |
| Matching a `dst` by suffix (`like '%2bf'`) | `dst_address` holds both routing ids and ordinary 64-hex contract addresses; a suffix match catches addresses that merely end the same way. Compare the full `':' \|\| lpad(to_hex(id),64,'0')`, or extract with `right(dst_address,16)`. |
| Reading `sweep_lag` (`min(last_swept_at)`) as sweep health | It includes books with no `OPEN` orders, which are never re-swept by design, so it only ever grows. Measure over books that have an open order — on stage that was 5 days versus 2m17s for the same database. |
| `sweep_lag_actionable` came back NULL, so nothing is wrong | NULL means no book is due a sweep at all. That is silence about the reconciler, not a clean bill of health — compare how `price_lag` and `sweep_lag` grew between two readings. |
| A large `price_lag` means prices are stale | The shipped metric does not exclude superseded books, whose `reference_price_at` never advances again. Re-measure with `filter (where superseded_at is null)` first. |
| `VACUUM FULL` during ops/cron | ACCESS EXCLUSIVE lock on the hot table. Use `vacuum (analyze)`. |
