# dodex-indexer

The indexer is the writer side of the Dodex backend. It pulls the global event
stream from Acki Nacki GraphQL, decodes message bodies against vendored ABIs,
and projects decoded events into the Supabase read-model that `services/api`
serves to clients. A separate market reconciler runs in parallel and fills in
fields that are not carried by events (timings, on-chain state) by calling
on-chain getters off-line via a local TVM runner.

## Pipeline

Each tick of the main indexer loop runs the following stages, all wired in
`services/indexer/src/main.rs` with primitives in `crates/infrastructure/`:

1. **Fetch** — `GraphqlClient::fetch_events(first, after)` issues a relay-style
   `blockchain.events` query (`crates/infrastructure/src/graphql.rs`). It paginates
   until `hasNextPage = false` or `MAX_PAGES_PER_TICK = 100` is hit.
2. **Filter** — edges with `node.src` in `indexer.ignored_addresses` are dropped
   before persistence. The cursor still advances on the original page.
3. **Persist raw** — `IndexerRepository::persist_page` inserts every edge into
   `raw_events` with `on conflict (msg_id) do nothing` and upserts the cursor in
   `indexer_cursors` in the same Postgres transaction. Page-atomic: a crash in
   the middle of a page replays the whole page on restart, deduped by `msg_id`.
4. **Decode** — `Decoder` (see `crates/infrastructure/src/decoder.rs`) tries to
   decode `body` (base64 BOC) against the eight dex ABIs vendored in
   `contracts/abi/dex/`. On match, `event_type` is set to `<Contract>.<EventName>`
   and `decoded` jsonb is populated. Unknown-id events stay with both columns
   `NULL`, so they can be re-decoded later if the ABI set is extended.
5. **Project** — `projectors::project_event` dispatches by `event_type` and
   updates the read-model tables. Each projector runs inside a **savepoint**
   (`Transaction::begin()` from `sqlx::Acquire`); if a projector errors, the
   savepoint rolls back, the raw row stays committed, and the rest of the page
   keeps going. On `Applied` / `Unknown` the row's `processed_at` is stamped;
   on `Deferred` / projector error it stays `null` so the reprojection loop
   picks it up later (see below).
6. **Sleep** — wait `indexer.polling_interval_ms` and tick again.

Cursor is persisted in `indexer_cursors` under `stream_name = "blockchain_events"`
and reloaded on startup via `IndexerRepository::load_cursor`, so restarts resume
where the previous run left off.

## Market reconciler

`MarketReconciler` (`crates/infrastructure/src/reconciler.rs`) runs as an
independent task spawned from `services/indexer/src/main.rs` on the
`indexer.reconciliation_interval_ms` cadence (default 60 s). It scans `markets`
for rows with `last_reconciled_at IS NULL`, fetches the corresponding account
BOC via GraphQL, and runs PMP off-line getters (`getDetails`,
`getOrderBookAddress`) through `tvm_runner`. The result fills `market_id`,
`name`, `oracle_list_hash`, timings (when available), `orderbook_address`, and
flips `last_reconciled_at`. It owns its own `GraphqlClient` and `Decoder`
clones, so a config-reload that swaps the main-loop client does not disturb
mid-run reconciliation.

The reconciler is also the only place that observes `PMP.getDetails().isCancelled`.
When it flips `markets.is_cancelled = true` and `markets.cancelled_at` is still
NULL (cancellation event was missed or has not been replayed), it stamps
`cancelled_at = extract(epoch from now())::bigint` so the API can fill
`terminal.at`. Coalesce-style: an event-derived (chain) timestamp is never
overwritten, and the discovery timestamp is never moved forward on a second
pass. The read-side mirrors this in `derive_status` and the SQL `STATUS_CASE`,
so either signal flips the market to `CANCELLED`.

## OracleEventList reconciler

`OracleEventListReconciler` (`crates/infrastructure/src/oracle_event_list_reconciler.rs`)
fills metadata that lives in OEL contract state but is **not** carried by the
`EventAdded` event — most importantly `oracle_events.describe`, which the API
exposes as `event.description` (docs/api-spec.md §Event), plus `trust_addr`.
Spawned from `services/indexer/src/main.rs` on the
`indexer.oracle_event_list_reconciliation_interval_ms` cadence (default 60 s).

Each sweep selects up to 16 OELs that have at least one child `oracle_events`
row still missing reconciler-only metadata — `describe IS NULL OR trust_addr
IS NULL` — ordered by failure recency then `oel.id` (backed by partial index
`oracle_events_pending_meta_idx` from migration `0010_*`, which replaced the
narrower `describe`-only index from `0008_*`). For each OEL it fetches the
account BOC, runs the `_events` getter via `tvm_runner`, walks the returned
`map(uint256, tuple)`, and updates each child row with `coalesce` semantics
so already-recorded values are never overwritten. The same
`describe IS NULL OR trust_addr IS NULL` predicate guards the UPDATE so the
write is idempotent — once both fields are populated the row drops out of the
partial index and is not visited again.

## Decoder

`Decoder::new()` loads ABIs through `include_str!` from `contracts/abi/dex/`
once at startup and builds a `HashMap<event_id (u32), (contract_kind,
event_name)>`. Decoding a body:

```text
read_single_root_boc(bytes)
    → SliceData::load_cell
    → Function::decode_output_id   (extracts the 32-bit event id)
    → lookup contract + event in the index
    → Contract::decode_output(slice, internal=true, allow_partial=true)
    → Detokenizer::detokenize_to_json_value
```

ABIs are vendored on purpose: builds are deterministic and do not depend on a
sibling clone of the contracts repo. Bumping the ABI version means dropping new
JSONs into `contracts/abi/dex/` and recompiling. The underlying TVM crates are
pinned via tag in the workspace `Cargo.toml`.

The index covers 42 events across the eight dex contracts (PMP, PrivateNote,
OrderBook, Oracle, OracleEventList, RootOracle, RootPN, Nullifier). System /
non-dex messages observed in the chain do not match any id and are silently
skipped (counted as `undecoded` in the per-tick log).

## Projectors

Lives in `crates/infrastructure/src/projectors.rs`. Single dispatch entry
`project_event(tx, decoded, node) -> ProjectionOutcome`. Outcomes:

- `Applied` — read-model write succeeded; `raw_events.processed_at` is stamped.
- `Deferred` — a parent record is missing (e.g. `OracleEventListDeployed`
  arrives before the corresponding `OracleDeployed`, or a PMP lifecycle event
  fires before `PMPDeployed`). Logged at warn, raw row still persists with
  `processed_at = null`, and the reprojection loop replays it on its next
  sweep — see *Deferred-projection retry* below.
- `Unknown` — `event_type` is not in the whitelist yet; `processed_at` is
  stamped so it will not be replayed.

Implemented today:

| Event                            | Target table         | Notes                                                                                  |
| -------------------------------- | -------------------- | -------------------------------------------------------------------------------------- |
| `RootOracle.OracleDeployed`      | `oracles`            | upsert by `address`; `name`/`pubkey`/`deploy_msg_id` use `coalesce` to keep known data |
| `Oracle.OracleEventListDeployed` | `oracle_event_lists` | upsert by `msg_id`; `oracle_id` resolved via `oracles.address = node.src`              |
| `OracleEventList.EventAdded`     | `oracle_events`      | upsert by `(eventlist_id, internal_id_in_eventlist)`; clears `is_deleted`              |
| `OracleEventList.EventConfirmed` | `oracle_events`      | sets `confirmed_pmp_address` + `confirmed_at`                                          |
| `PrivateNote.PMPDeployed`        | `markets`            | upsert by `pmp_address`; resolves `token_code` via `ref_tokens`; `created_at` from `node.created_at` (not overwritten on conflict) |
| `PMP.TimingsSet`                 | `markets`            | writes stake/result timings, sets `approved = true`                                    |
| `PMP.PoolsFrozen`                | `markets`            | writes `frozen_at` from `node.created_at`                                              |
| `PMP.Resolved`                   | `markets`            | writes `resolved_at` + `resolved_outcome_id`                                           |
| `PMP.EventCancelled`             | `markets`            | writes `cancelled_at` + `cancel_reason = 'EVENT_CANCELLED'`                            |
| `PMP.PMPCancelled`               | `markets`            | writes `cancelled_at` + `cancel_reason = 'PMP_CANCELLED'`                              |

TODO:

- `OrderBook.*` (`OrderPlaced`, `OrderCancelled`, `OrderFilled`, …) → a
  `live_orders` table aggregated into `order_book_snapshots` for `/depth`.

## Deferred-projection retry

`IndexerRepository::run_reprojection_loop` runs as an independent task spawned
from `services/indexer/src/main.rs` on the
`indexer.reprojection_interval_ms` cadence (default 30 s). It scans
`raw_events` for rows where `processed_at is null and event_type is not null
and decoded is not null`, ordered by `created_at_chain asc, id asc` so that an
out-of-order parent that just arrived gets its first chance before its
children retry. The query is backed by the partial index
`raw_events_pending_projection_idx` (migration `0007_*`).

For each row the loop reconstructs a `DecodedEvent` from the stored `decoded`
jsonb (no re-decoding of bodies) plus an `EventNode` from
`msg_id`/`src_address`/`dst_address`/`created_at_chain`, runs `project_event`
in a savepoint, and:

- on `Applied` / `Unknown` — stamps `processed_at = now()`;
- on `Deferred` / projector error — leaves `processed_at` null for another
  pass.

Projectors are idempotent (upserts), so replaying a row that has already been
applied via the main loop or a previous sweep is safe but a no-op. The batch
size is bounded by `indexer.reprojection_batch_size` (default 500). When the
backlog is empty the sweep logs at `debug` only.

## Database and migrations

The indexer applies SQL migrations from `migrations/` automatically at startup
through `sqlx::migrate!`. Migrations are numbered `NNNN_*.sql`, idempotent
(`create table if not exists`, `alter ... drop not null`), and safe to run
against an already-initialised database.

Today's migrations:

- `0001_init_read_model.sql` — full read-model schema and `ref_tokens` seed.
- `0002_raw_events_nullable.sql` — relaxes `NOT NULL` on `raw_events.src_address`,
  `dst_address`, `event_type` since these are not always known at ingestion.
- `0003_raw_events_decoded.sql` — adds `raw_events.decoded jsonb` and a partial
  index on `event_type` for projector-side scans.
- `0004_oracle_events_confirmed.sql` — adds `confirmed_pmp_address`/`confirmed_at`
  to `oracle_events` and an index for the API-side join.
- `0005_markets_relax_required.sql` — drops `NOT NULL` on `markets.market_id`,
  `name`, `oracle_list_hash`; adds `last_reconciled_at`, `oracle_event_lists_json`,
  `oracle_fee_json` for the reconciler.
- `0006_markets_lifecycle.sql` — adds `frozen_at`, `resolved_at`,
  `resolved_outcome_id`, `cancelled_at`, `cancel_reason` for the nine-phase
  market lifecycle and a partial terminal index.
- `0007_raw_events_pending_idx.sql` — adds
  `raw_events_pending_projection_idx` (partial, on
  `(created_at_chain, id) where processed_at is null and event_type is not
  null and decoded is not null`) backing the deferred-projection retry loop.
- `0008_oracle_events_describe_idx.sql` — adds
  `oracle_events_describe_pending_idx` (partial, on `eventlist_id where
  describe is null`) backing the OracleEventList reconciler.

### Supabase permissions

On a fresh Supabase project the application role does not own `public` and
cannot create the `_sqlx_migrations` table. From the Supabase SQL Editor (which
runs as `postgres`) grant the application role once:

```sql
grant usage, create on schema public to indexer;
grant all privileges on all tables    in schema public to indexer;
grant all privileges on all sequences in schema public to indexer;
alter default privileges in schema public grant all on tables    to indexer;
alter default privileges in schema public grant all on sequences to indexer;
```

Replace `indexer` with whatever role appears before the dot in your pooler
username (`<role>.<project_ref>`). After granting, Supavisor caches role
metadata for some time — give it ~1–2 minutes (or rotate the role password to
force a cache flush) before retrying.

## Configuration

YAML at `config/indexer.<env>.yaml`. Override the path with
`APP_CONFIG=/path/to/file.yaml`. Schema (enforced via `serde(deny_unknown_fields)`):

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
events are dropped before `raw_events` insert and projector dispatch — only
addresses that emit confirmed noise (system / null-route).

Send `SIGUSR1` to the running process to reload the config (handled by
`signal::run_config_reload_loop` in `crates/infrastructure/src/signal.rs`,
which re-parses the YAML and swaps the `Arc<RwLock<IndexerConfig>>` shared
state). What the new values actually affect is narrower than a full restart:

| Knob                                              | On `SIGUSR1`                                                                                                |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `graphql.endpoint`                                | Picked up by the **main loop only** — its GraphQL client is rebuilt when endpoint/timeout differ from the live values (`services/indexer/src/main.rs:100-118`). |
| `graphql.request_timeout_ms`                      | Same — main loop only.                                                                                      |
| `graphql.page_size`                               | Read fresh each tick of the main loop.                                                                      |
| `indexer.polling_interval_ms`                     | Read fresh each tick of the main loop.                                                                      |
| `indexer.ignored_addresses`                       | Read fresh each tick of the main loop.                                                                      |
| `indexer.reconciliation_interval_ms`              | **Pinned at startup** for `MarketReconciler`. Restart to change.                                            |
| `indexer.reprojection_interval_ms` / `_batch_size`| **Pinned at startup** for the reprojection loop. Restart to change.                                         |
| `indexer.oracle_event_list_reconciliation_interval_ms` | **Pinned at startup** for `OracleEventListReconciler`. Restart to change.                              |
| `graphql.*` for the **reconciler / OEL reconciler** GraphQL clients | **Pinned at startup**. They keep their own `GraphqlClient` instance, frozen at the value the process saw on boot — restart to swap their endpoint or timeout. |
| `database.*` (URL, pool sizes, connect timeout)   | **Pinned at startup**. The `sqlx::PgPool` is built once before the main loop and shared by every task; SIGUSR1 does not rebuild it. Restart to change DB connection params. |

In short: SIGUSR1 only retunes the **main fetch loop** (endpoint, timeout,
page size, polling cadence, ignore list). Anything that affects the
reconciler tasks, the reprojection loop, or the database pool requires a
process restart.

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

To inspect ingest progress in Supabase:

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

## Crate layout the indexer depends on

```text
crates/infrastructure/src/
├── config.rs        IndexerConfig (YAML schema, serde validation)
├── signal.rs        SIGUSR1 reload loop
├── database.rs      sqlx Pg pool + sqlx::migrate!
├── graphql.rs       reqwest + relay pagination over blockchain.events
├── decoder.rs       tvm_abi-based BOC decoder, ABI v2.4
├── tvm_runner.rs    off-chain TVM getter execution against account BOC
├── reconciler.rs    market reconciler (PMP getDetails / getOrderBookAddress)
├── indexer_repo.rs  raw_events / indexer_cursors persistence + projector dispatch
└── projectors.rs    Event → read-model writers, savepoint-isolated
```

## Tests

Unit-level, all in `crates/infrastructure`:

- `decoder::tests` — ABI loading and event-id index build, base64 error path,
  unknown-id passthrough on a real shellnet body.
- `graphql::tests` — JSON deserialisation of `EventsPage`, error envelopes,
  nullable node fields.
- `indexer_repo::tests` — `parse_unix_seconds` for int / float / string
  timestamps; `pending_row_to_inputs` field mapping for the reprojection
  loop (full payload, missing `event_type`, NaN/Inf timestamps, nullable
  src/dst).
- `projectors::tests` — `uint256_hex_to_decimal` parsing and rejection.
- `reconciler::tests` — power-of-ten decimal rendering for tick / step sizes.
- `oracle_event_list_reconciler::tests` — `_events` getter response parsing:
  describe / trustAddr extraction, empty-string vs null normalisation,
  invalid-hex key rejection.
- `tvm_runner::tests` — invalid account BOC and unknown function rejection.
- `config::tests` — schema separation between `ApiConfig` and `IndexerConfig`.

End-to-end coverage of the reprojection loop lives in
`crates/infrastructure/tests/reprojection.rs`. It is gated on
`TEST_DATABASE_URL`; when the variable is unset every test prints a skip
notice and returns early, so `cargo test` still passes without a database.
Tests use unique per-test prefixes for `msg_id` and addresses so they can
run concurrently against the same database without colliding.

The repo ships a throw-away Postgres for this in `docker-compose.test.yml`
(port 55432, tmpfs storage, fsync off — schema is created by
`sqlx::migrate!` on first connect):

```sh
docker compose -f docker-compose.test.yml up -d --wait
export TEST_DATABASE_URL=postgres://dodex:dodex@localhost:55432/dodex_test
cargo test -p dodex-infrastructure --test reprojection -- --nocapture
docker compose -f docker-compose.test.yml down
```

If you prefer to point at an existing database, just export
`TEST_DATABASE_URL` to its URL — the suite calls `database::run_migrations`
on connect, so the role must own the `public` schema.

Scenarios covered:
- `Applied` outcome stamps `processed_at` and writes the read-model row.
- A `Deferred` `OracleEventListDeployed` keeps `processed_at = null` until
  its parent `OracleDeployed` materialises, then is applied on the next
  sweep.
- Rows that already carry `processed_at` are not picked up — neither the
  timestamp nor the read-model is touched.
- `Unknown` event types still receive `processed_at` so the retry queue
  drains.

A second integration suite, `crates/infrastructure/tests/markets_status.rs`,
exercises the read path (`PostgresReadModelRepository`) and pins the contract
that a market with `is_cancelled = true` and `cancelled_at = null` (the
reconciler-only path, when the cancellation event is missed or hasn't been
replayed) is still surfaced as `CANCELLED` to the API and matches the
`status=CANCELLED` listing filter. It also asserts the reconciler's
"stamp `cancelled_at` on first observation, never overwrite" idempotency.

A third suite, `crates/infrastructure/tests/oel_reconciler.rs`, pins the
DB-write contract of the OracleEventList reconciler: the SQL emitted by
`apply_event_metadata` fills `describe` / `trust_addr` when null, never
overwrites already-populated values, and partially fills the missing field
when only one of the two is set.
