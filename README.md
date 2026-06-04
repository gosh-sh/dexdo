# DEX.DO

Backend for DEX.DO — a decentralized exchange on the Acki Nacki chain. Two Rust services share a Postgres read-model:

- `services/api` — HTTP service serving the public REST API.
- `services/indexer` — chain-event ingestor that builds the Postgres read-model.

On-chain DEX.DO contracts live under `contracts/`.

## Documentation

- [docs/api-spec.md](docs/api-spec.md) — public REST API contract.
- [docs/openapi.yaml](docs/openapi.yaml) — OpenAPI 3.1 contract, generated from the Rust handlers. See [openapi/README.md](openapi/README.md) for the regen workflow and the GitHub Pages deployment.
- [docs/README.md](docs/README.md) — documentation map and file ownership.
- [docs/deployment.md](docs/deployment.md) — self-hosting the `indexer` and `api` on your own server, wired to your own Acki Nacki GraphQL endpoint and your own Postgres / Supabase.
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
contracts/           # on-chain DEX.DO contracts (TVM)
sdk/                 # dodex-sdk: write-side DEX facade + halo2 voucher pipeline
                     # (separate workspace, excluded from the root build)
docs/                # specs and plans (see docs/README.md)
migrations/          # SQL migrations applied by sqlx::migrate! at startup
config/              # service config files (api.<env>.yaml, indexer.<env>.yaml)
scripts/             # operational scripts
tests/               # repo-level integration fixtures (REST .rest files, e2e)
```

`sdk/` is its own Cargo workspace and is **not** part of `cargo build --workspace` — its halo2 proof pipeline pulls a heavy, distinct zk/halo2 dependency graph. Build it from `sdk/` directly (`cargo build` there).

## Configuration

Per-service config files live under `config/`:

- `config/api.<env>.yaml` — consumed by `services/api`.
- `config/indexer.<env>.yaml` — consumed by `services/indexer`.

Local defaults: `config/api.local.yaml`, `config/indexer.local.yaml`. Override at runtime with `APP_CONFIG=/path/to/file.yaml`.

Logging is environment-driven: `RUST_LOG` sets verbosity, and `LOG_DIR`
(optional) makes each service also write rotated log files into a directory —
the Compose deployment bind-mounts these to `./logs/<service>`. See
[docs/deployment.md](docs/deployment.md#logs).

Metrics are OpenTelemetry/OTLP: the indexer exports event counters when
`OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` (or `OTEL_EXPORTER_OTLP_ENDPOINT`) is set,
and collects nothing when unset. See
[docs/tech-specs/indexer.md](docs/tech-specs/indexer.md#metrics).

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

## Deployment

To run the services on your own server — wired to your own Acki Nacki GraphQL
endpoint and your own Postgres or Supabase — follow
[docs/deployment.md](docs/deployment.md). It uses the shipped Dockerfiles and a
Compose override (the same mechanism as `docker-compose.stage.yml`).

## License

DEX.DO is licensed under the **GNU Affero General Public License v3.0** — see [LICENSE.md](LICENSE.md) for the full text and [NOTICE.md](NOTICE.md) for copyright and the note on the Acki Nacki Block Manager runtime dependency (which is published separately under its own license).
