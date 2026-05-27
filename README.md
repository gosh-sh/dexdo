# DODEX

Backend for DODEX — a decentralized exchange on the Acki Nacki chain. Two Rust services share a Postgres read-model:

- `services/api` — HTTP service serving the public REST API.
- `services/indexer` — chain-event ingestor that builds the Postgres read-model.

On-chain DODEX contracts live under `contracts/`.

## Documentation

- [docs/api-spec.md](docs/api-spec.md) — public REST API contract.
- [docs/openapi.yaml](docs/openapi.yaml) — OpenAPI 3.1 contract, generated from the Rust handlers. See [openapi/README.md](openapi/README.md) for the regen workflow and the GitHub Pages deployment.
- [docs/README.md](docs/README.md) — documentation map and file ownership.
- [AGENT_REQUIREMENTS.md](AGENT_REQUIREMENTS.md) — rules for any agent making repository changes.

## Repository layout

```
crates/
  domain/            # domain types
  application/       # use cases
  infrastructure/    # adapters (Postgres, TVM runner, GraphQL gateway)
services/
  api/               # REST API service
  indexer/           # chain-event indexer
contracts/           # on-chain DODEX contracts (TVM)
docs/                # specs and plans (see docs/README.md)
migrations/          # SQL migrations applied by sqlx::migrate! at startup
config/              # service config files (api.<env>.yaml, indexer.<env>.yaml)
scripts/             # operational scripts
tests/               # repo-level integration fixtures (REST .rest files, e2e)
```

## Configuration

Per-service config files live under `config/`:

- `config/api.<env>.yaml` — consumed by `services/api`.
- `config/indexer.<env>.yaml` — consumed by `services/indexer`.

Local defaults: `config/api.local.yaml`, `config/indexer.local.yaml`. Override at runtime with `APP_CONFIG=/path/to/file.yaml`.

Secrets and environment-specific values live in `.env`:

```sh
cp .env.example .env
```

The `.env` file is gitignored; the committed `.env.example` is the template.

## Build

```sh
cargo build --workspace
```

## Lint and format

```sh
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets --no-deps -- -D warnings
```

`rustfmt.toml` uses unstable features, so `fmt` needs nightly. `clippy` runs on the default toolchain.

## Test Postgres

The full test suite needs a disposable Postgres. Bring it up with the test compose file:

```sh
docker compose -f docker-compose.test.yml up -d --wait
```

`TEST_DATABASE_URL` is read from `.env` (copied from `.env.example` on first checkout). The test database role must own `public` because the test suite runs migrations on connect.

Tear it down with:

```sh
docker compose -f docker-compose.test.yml down -v
```

## Tests

Unit tests (no database required):

```sh
cargo test --workspace --lib
```

DB-backed integration tests (test Postgres up first, see above):

```sh
cargo test --workspace --tests
```

Or with [cargo-nextest](https://nexte.st) (faster — runs integration test binaries in parallel, matches what CI uses):

```sh
cargo nextest run --workspace
```

Per-service narrower runs are described in [services/api/README.md](services/api/README.md) and [services/indexer/README.md](services/indexer/README.md).

## Running locally

After `cargo build --workspace`:

```sh
cargo run -p dodex-api
cargo run -p dodex-indexer
```

The API needs Postgres running and the indexer feeding it; see the service READMEs for the bring-up sequence.

## License

See [LICENSE.md](LICENSE.md).
