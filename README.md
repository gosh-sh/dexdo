# Dodex Backend

## Architecture

Two independent processes that share a Postgres read-model:

```text
Acki Nacki GraphQL  →  indexer  →  Postgres (read-model)  →  api  →  REST clients
```

- **`services/api`** — HTTP server. Serves market metadata and order-book snapshots from Postgres, and gates private endpoints behind HMAC authentication.
- **`services/indexer`** — ingests chain events, decodes them, and projects them into the read-model.
- **Postgres** — single source of truth for the API. The indexer applies migrations on startup.

Splitting `api` and `indexer` keeps user-facing latency independent of chain ingestion and lets either side scale independently.

## Repository layout

```text
.
├── services/         Service binaries (api, indexer)
├── crates/           Shared library crates (domain, application, infrastructure)
├── contracts/        On-chain Solidity contracts and ABIs
├── config/           YAML config files (per-service, per-environment)
├── migrations/       SQL migrations
└── docs/             api-spec.md (public REST contract), tech-specs/, contract-specs/
```

Per-component internals are documented in each service's `README.md`.

## Configuration

YAML per service per environment, e.g. `config/api.local.yaml`. The default path is `config/<service>.local.yaml`; override with `APP_CONFIG=/path/to/file.yaml`.

## Running locally

Both processes need a Postgres connection. Point both at the same database; the indexer applies migrations on startup.

```sh
cargo run -p dodex-indexer
cargo run -p dodex-api
```

## Tests and formatting

### Test Postgres

Integration tests need a real Postgres. A disposable test database is shipped in `docker-compose.test.yml` on port `55432`:

```sh
docker compose -f docker-compose.test.yml up -d --wait
export TEST_DATABASE_URL=postgres://dodex:dodex@localhost:55432/dodex_test
cargo test
docker compose -f docker-compose.test.yml down
```

The committed `.env` already contains the same `TEST_DATABASE_URL`. Export it yourself when pointing tests at another database.

```sh
cargo test --workspace --lib
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Deployment

```sh
docker compose -f docker-compose.yml -f docker-compose.stage.yml up -d --build
```

## Documentation

- [docs/api-spec.md](docs/api-spec.md) — functional REST API requirements.
- [docs/tech-specs/](docs/tech-specs/) — implementation technical specs.
- [docs/tech-specs/data-schema.md](docs/tech-specs/data-schema.md) — Postgres schema semantics.
- [docs/contract-specs/](docs/contract-specs/) — on-chain contracts and event routing.
- [services/api/README.md](services/api/README.md), [services/indexer/README.md](services/indexer/README.md) — service entry points with spec links, config, and maintenance commands.

## For contributors and AI agents

[`AGENT_REQUIREMENTS.md`](AGENT_REQUIREMENTS.md) is the entry point for anyone (human or AI) making changes here. It defines the documentation contract, including the mandatory pre-commit sweep over `docs/`, component READMEs, and this root `README.md`.

## License

See [LICENSE.md](LICENSE.md).
