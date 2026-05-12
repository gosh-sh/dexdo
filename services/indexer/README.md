# dodex-indexer

Writer-side service. Pulls the global event stream from the Acki Nacki
GraphQL gateway, decodes message bodies against the vendored ABIs in
`contracts/abi/dex/`, and projects decoded events into the Postgres
read-model that `services/api` serves. Two reconciler tasks run in parallel
and fill metadata the events do not carry (PMP timings / orderbook address,
OEL `_events` map → `describe` / `trust_addr`).

Behaviour, pipeline stages, projectors, reconciler queue semantics,
fail-closed invariants:

- [`docs/tech-specs/market-data-indexer.md`](../../docs/tech-specs/market-data-indexer.md)
- [`docs/tech-specs/data-schema.md`](../../docs/tech-specs/data-schema.md)

## Configuration

YAML at `config/indexer.<env>.yaml`. Override the path with
`APP_CONFIG=/path/to/file.yaml`. Schema (enforced via
`serde(deny_unknown_fields)`):

```yaml
app:
  env: local
  log_level: info

database:
  url: postgresql://<role>.<project_ref>:<password>@aws-1-eu-central-1.pooler.supabase.com:5432/postgres
  max_connections: 10
  min_connections: 1
  connect_timeout_ms: 3000

graphql:
  endpoint: https://shellnet.ackinacki.org/graphql
  page_size: 100
  request_timeout_ms: 10000

indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
  reprojection_interval_ms: 30000
  reprojection_batch_size: 500
  oracle_event_list_reconciliation_interval_ms: 60000
  ignored_addresses:
    - "0:1111111111111111111111111111111111111111111111111111111111111111"
```

`indexer.ignored_addresses` is a narrow allowlist of source addresses whose
events are dropped before persistence — only addresses that emit confirmed
noise (system / null-route).

### SIGUSR1 reload

Send `SIGUSR1` to the running process to reload the config (handled by
`signal::run_config_reload_loop` in `crates/infrastructure/src/signal.rs`).
What the new values actually affect is narrower than a full restart:

| Knob                                                                | On `SIGUSR1`                                                                                |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| `graphql.endpoint`                                                  | Picked up by the **main loop only** — its GraphQL client is rebuilt when endpoint/timeout differ from the live values. |
| `graphql.request_timeout_ms`                                        | Same — main loop only.                                                                      |
| `graphql.page_size`                                                 | Read fresh each tick of the main loop.                                                      |
| `indexer.polling_interval_ms`                                       | Read fresh each tick of the main loop.                                                      |
| `indexer.ignored_addresses`                                         | Read fresh each tick of the main loop.                                                      |
| `indexer.reconciliation_interval_ms`                                | **Pinned at startup** for `MarketReconciler`. Restart to change.                            |
| `indexer.reprojection_interval_ms` / `_batch_size`                  | **Pinned at startup** for the reprojection loop. Restart to change.                         |
| `indexer.oracle_event_list_reconciliation_interval_ms`              | **Pinned at startup** for `OracleEventListReconciler`. Restart to change.                   |
| `graphql.*` for the **reconciler / OEL reconciler** GraphQL clients | **Pinned at startup**. They keep their own `GraphqlClient` instance, frozen at boot.        |
| `database.*` (URL, pool sizes, connect timeout)                     | **Pinned at startup**. The `sqlx::PgPool` is built once and shared; SIGUSR1 does not rebuild it. |

In short: SIGUSR1 retunes the **main fetch loop**. Anything that touches the
reconciler tasks, the reprojection loop, or the database pool requires a
process restart.

## Database and migrations

The indexer applies SQL migrations from `migrations/` automatically at
startup through `sqlx::migrate!`. Migrations are numbered `NNNN_*.sql` and
applied in order; individual files document their intent in their comment
header. Column-level semantics live in
[`docs/tech-specs/data-schema.md`](../../docs/tech-specs/data-schema.md).

**Reindex required for `0016_chain_order.sql`** — the migration truncates
`raw_events`, `live_orders`, and `indexer_cursors` because the new NOT-NULL
`chain_order` / `last_chain_order` keys live on chain messages and cannot be
backfilled locally. The indexer resumes from genesis on next boot.

### Supabase permissions

On a fresh Supabase project the application role does not own `public` and
cannot create the `_sqlx_migrations` table. From the Supabase SQL Editor
(which runs as `postgres`) grant the application role once:

```sql
grant usage, create on schema public to indexer;
grant all privileges on all tables    in schema public to indexer;
grant all privileges on all sequences in schema public to indexer;
alter default privileges in schema public grant all on tables    to indexer;
alter default privileges in schema public grant all on sequences to indexer;
```

Replace `indexer` with whatever role appears before the dot in your pooler
username (`<role>.<project_ref>`). Supavisor caches role metadata for some
time — give it ~1–2 minutes (or rotate the role password to force a cache
flush) before retrying.

## Running

```sh
cargo run -p dodex-indexer
```

Per-tick log line includes:

- `edges` / `pages` — fetched from GraphQL.
- `ignored` — edges dropped via `ignored_addresses`.
- `inserted` / `skipped` — written vs deduped against `raw_events`.
- `decoded` / `undecoded` — bodies the ABI matched vs not.
- `projected` / `projection_deferred` / `projection_failed` — projector outcomes.
- `cursor` — last `endCursor` after this tick.

Operational SQL to inspect ingest progress:

```sql
select count(*) from raw_events;
select stream_name, cursor, updated_at from indexer_cursors;
select event_type, count(*) from raw_events
  where event_type is not null
  group by event_type
  order by count(*) desc;
-- pending reprojection backlog
select event_type, count(*) from raw_events
  where processed_at is null
    and event_type is not null
    and decoded is not null
  group by event_type
  order by count(*) desc;
select count(*) from oracles;
select count(*) from oracle_event_lists;
select count(*) from markets where last_reconciled_at is not null;
```

## Tests

Unit tests live alongside the implementation in `crates/infrastructure`:

```sh
cargo test -p dodex-infrastructure
```

Integration tests exercise the write/read paths against a real Postgres and
are gated on `TEST_DATABASE_URL`. The repo ships a throw-away harness in
`docker-compose.test.yml` (port 55432, tmpfs storage, fsync off — schema is
created by `sqlx::migrate!` on first connect):

```sh
docker compose -f docker-compose.test.yml up -d --wait
export TEST_DATABASE_URL=postgres://dodex:dodex@localhost:55432/dodex_test
cargo test -p dodex-infrastructure
docker compose -f docker-compose.test.yml down
```

To point at an existing database instead, export `TEST_DATABASE_URL` to its
URL — the suite calls `database::run_migrations` on connect, so the role must
own `public`. Tests use unique per-test prefixes for `msg_id` and addresses
so they can run concurrently against the same database without colliding.
