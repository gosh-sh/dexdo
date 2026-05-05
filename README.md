# Dodex Backend

REST API and chain indexer for prediction markets on the Acki Nacki blockchain. The
backend exposes a Binance-style spot trading contract (`/api/v1/markets`, `/depth`,
`/account`, `/order`, `/batchOrders`, `/openOrders`, `/allOrders`) over a read-model
materialized from on-chain events. Currently delivered: `/api/v1/markets` (with
lifecycle status, timings, oracle event metadata, terminal info, and cursor
pagination); `/depth` is wired but its read-model is not built yet.

## Architecture

The system is split into two independent processes that share a Supabase Postgres
read-model. Data flow:

```text
Acki Nacki GraphQL  →  indexer  →  Supabase Postgres (read-model)  →  api  →  REST clients
```

- **`services/api`** (Salvo) — read-only HTTP server. Never touches GraphQL or BOC
  decoding; serves market metadata and order-book snapshots straight from Postgres.
- **`services/indexer`** — ingests `blockchain.events` from Acki Nacki GraphQL,
  decodes message bodies against vendored ABIs (TVM ABI v2.4 via `tvm_abi` /
  `tvm_types` from the [tvm-sdk](https://github.com/tvmlabs/tvm-sdk)), and projects
  decoded events into the read-model. Per-page atomicity with savepoint isolation
  per projector.
- **Supabase Postgres** — single source of truth for the API. Schema lives in
  `migrations/`; the indexer applies migrations automatically on startup via
  `sqlx::migrate!`.

Splitting `api` and `indexer` keeps user-facing latency independent of chain
ingestion, makes either side independently scalable, and lets us replay
projectors against `raw_events` without re-fetching from the chain.

## Repository layout

```text
.
├── services/
│   ├── api/          Salvo HTTP server (read-only)
│   └── indexer/      Chain ingestion + decoder + projectors  (see services/indexer/README.md)
├── crates/
│   ├── domain/       Value objects, entities, domain errors
│   ├── application/  Use cases, ports to infrastructure
│   └── infrastructure/  sqlx repositories, GraphQL client, ABI decoder, projectors,
│                        config loader, SIGUSR1 reload
├── contracts/
│   ├── *.sol         On-chain contracts (PMP, Oracle, OrderBook, …)
│   └── abi/dex/      Vendored ABI v2.4 JSONs consumed by the decoder
├── config/           YAML config files (per-service, per-environment)
├── migrations/       SQL migrations (numbered NNNN_*.sql, auto-applied by indexer)
└── docs/             Architecture plan, API spec, gap analysis
```

## Tech stack

- **Rust** (edition 2024), `cargo workspace`.
- **Salvo** for HTTP, **sqlx** (Postgres + rustls) for the database.
- **reqwest** for the GraphQL client.
- **tvm_abi** + **tvm_types** (low-level TVM crates from
  [tvmlabs/tvm-sdk](https://github.com/tvmlabs/tvm-sdk)) for decoding event BOCs.
- **Supabase Postgres** as the read-model store; migrations applied on boot.

## Configuration

YAML per service per environment, e.g. `config/api.local.yaml`,
`config/indexer.local.yaml`. The schema is enforced via `serde(deny_unknown_fields)`
so that mismatched sections fail fast at load time. Both services support
`SIGUSR1` to reload config without restart; on reload, external clients (HTTP,
GraphQL, Postgres pool) are rebuilt only if their parameters changed.

The default config path is `config/<service>.local.yaml`; override with
`APP_CONFIG=/path/to/file.yaml`.

## Running locally

Both processes need a Postgres connection. The indexer applies migrations on
startup, so point both at the same database. For local development a plain
docker-postgres is enough; for stage we use Supabase via the connection pooler.

```sh
# indexer (writes to DB, pulls from GraphQL)
cargo run -p dodex-indexer

# api (reads from DB)
cargo run -p dodex-api
```

If the indexer fails with `permission denied for schema public` on first run,
grant the application role usage/create on `public` once from a Supabase admin
session — see [services/indexer/README.md](services/indexer/README.md#supabase-permissions)
for the exact SQL.

## Tests and formatting

```sh
cargo test --workspace --lib
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Deployment

Stage runs via `docker compose` with an override file:

```sh
docker compose -f docker-compose.yml -f docker-compose.stage.yml up -d --build
```

Host preparation (apt + docker + repo clone + config templating) is automated
in `deploy/ansible/`; see `deploy/ansible/playbook.yml`.

## Documentation

- [docs/api-spec.md](docs/api-spec.md) — public REST contract (markets, depth,
  account, orders, batchOrders, openOrders, allOrders) with HMAC auth.
- [docs/tech-spec.md](docs/tech-spec.md), [docs/GRAPHQL.md](docs/GRAPHQL.md),
  [docs/dex-events-routing.md](docs/dex-events-routing.md) — auxiliary specs.
- [services/api/README.md](services/api/README.md),
  [services/indexer/README.md](services/indexer/README.md) — per-service
  internals.

## License

See [LICENSE.md](LICENSE.md).
