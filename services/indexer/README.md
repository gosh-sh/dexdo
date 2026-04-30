# dodex-indexer

The indexer is the writer side of the Dodex backend. It pulls the global event
stream from Acki Nacki GraphQL, decodes message bodies against vendored ABIs,
and projects decoded events into the Supabase read-model that `services/api`
serves to clients.

## Pipeline

Each tick of the indexer loop runs the following stages, all wired in
`services/indexer/src/main.rs` with primitives in `crates/infrastructure/`:

1. **Fetch** — `GraphqlClient::fetch_events(first, after)` issues a relay-style
   `blockchain.events` query (`crates/infrastructure/src/graphql.rs`). It paginates
   until `hasNextPage = false` or `MAX_PAGES_PER_TICK = 100` is hit.
2. **Persist raw** — `IndexerRepository::persist_page` inserts every edge into
   `raw_events` with `on conflict (msg_id) do nothing` and upserts the cursor in
   `indexer_cursors` in the same Postgres transaction. Page-atomic: a crash in
   the middle of a page replays the whole page on restart, deduped by `msg_id`.
3. **Decode** — `Decoder` (see `crates/infrastructure/src/decoder.rs`) tries to
   decode `body` (base64 BOC) against the eight dex ABIs vendored in
   `contracts/abi/dex/`. On match, `event_type` is set to `<Contract>.<EventName>`
   and `decoded` jsonb is populated. Unknown-id events stay with both columns
   `NULL`, so they can be re-decoded later if the ABI set is extended.
4. **Project** — `projectors::project_event` dispatches by `event_type` and
   updates the read-model tables. Each projector runs inside a **savepoint**
   (`Transaction::begin()` from `sqlx::Acquire`); if a projector errors, the
   savepoint rolls back, the raw row stays committed, and the rest of the page
   keeps going.
5. **Sleep** — wait `indexer.polling_interval_ms` and tick again.

Cursor is persisted in `indexer_cursors` under `stream_name = "blockchain_events"`
and reloaded on startup via `IndexerRepository::load_cursor`, so restarts resume
where the previous run left off.

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

Currently the index covers 42 events across the eight dex contracts (PMP,
PrivateNote, OrderBook, Oracle, OracleEventList, RootOracle, RootPN, Nullifier).
System / non-dex messages observed in the chain do not match any id and are
silently skipped (counted as `undecoded` in the per-tick log).

## Projectors

Lives in `crates/infrastructure/src/projectors.rs`. Single dispatch entry
`project_event(tx, decoded, node) -> ProjectionOutcome`. Outcomes:

- `Applied` — read-model write succeeded.
- `Deferred` — a parent record is missing (e.g. `OracleEventListDeployed`
  arrives before the corresponding `OracleDeployed`). Logged at warn, raw row
  still persists, will be picked up on the next pass.
- `Unknown` — `event_type` is not in the whitelist yet.

Implemented today:

| Event                              | Target table          | Notes                                                                                  |
| ---------------------------------- | --------------------- | -------------------------------------------------------------------------------------- |
| `RootOracle.OracleDeployed`        | `oracles`             | upsert by `address`; `name`/`pubkey`/`deploy_msg_id` use `coalesce` to keep known data |
| `Oracle.OracleEventListDeployed`   | `oracle_event_lists`  | upsert by `msg_id`; `oracle_id` resolved via `oracles.address = node.src`              |

TODO:
 - `EventAdded` / `EventConfirmed` → `oracle_events`;
 - `PrivateNote.PMPDeployed` plus `PMP.getDetails()` reconciliation → `markets` / `market_outcomes`;
 - `OrderBook.*` events plus periodic `_state` snapshots → `order_book_snapshots`.

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

features:
  stub_mode: true
```

Send `SIGUSR1` to the running process to reload the config; the GraphQL client
and Postgres pool are rebuilt only if their respective parameters changed.

## Running

```sh
cargo run -p dodex-indexer
```

Per-tick log line includes:

- `edges` / `pages` — fetched from GraphQL.
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
select count(*) from oracles;
select count(*) from oracle_event_lists;
```

## Crate layout the indexer depends on

```text
crates/infrastructure/src/
├── config.rs        IndexerConfig (YAML schema, serde validation)
├── signal.rs        SIGUSR1 reload loop
├── database.rs      sqlx Pg pool + sqlx::migrate!
├── graphql.rs       reqwest + relay pagination over blockchain.events
├── decoder.rs       tvm_abi-based BOC decoder, ABI v2.4
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
  timestamps.
- `config::tests` — schema separation between `ApiConfig` and `IndexerConfig`.
