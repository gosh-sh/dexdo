# dodex-api

HTTP service for the DODEX REST API. It serves the read side of the system from
Postgres and hosts authenticated private API routes.

## Specifications

- Functional REST requirements: [docs/api-spec.md](../../docs/api-spec.md).
- Authentication implementation: [docs/tech-specs/auth.md](../../docs/tech-specs/auth.md).
- Read API implementation (all `GET` endpoints): [docs/tech-specs/read-api.md](../../docs/tech-specs/read-api.md).
- Write API implementation (order placement / cancellation / batching / position writes): [docs/tech-specs/write-api.md](../../docs/tech-specs/write-api.md). Covers `POST /api/v1/order`, `DELETE /api/v1/order`, `POST /api/v1/batchOrders`, `DELETE /api/v1/batchOrders`, and `POST /api/v1/buyFullSet` today; `DELETE /api/v1/openOrders` is a stub section inside that doc.
- Data schema: [docs/tech-specs/data-schema.md](../../docs/tech-specs/data-schema.md).

Implementation details belong in the tech specs above, not in this README.

## Configuration

Config file: `config/api.<env>.yaml`. Local default: `config/api.local.yaml`.
Override with `APP_CONFIG=/path/to/file.yaml`.

Config sections:

- `app`: environment name and log level.
- `server`: host, port, request timeout.
- `database`: Postgres URL and pool settings.
- `auth`: HMAC recvWindow limits and local seed-account toggle.
- `chain`: Acki Nacki gateway endpoint (`gateway_endpoint`) the trading
  path POSTs external messages to, plus `place_order_timeout_ms`,
  `cancel_order_timeout_ms`, `place_batch_timeout_ms`,
  `cancel_batch_timeout_ms`, and `split_full_set_timeout_ms` bounding
  the per-call wait for each chain entry point. Local config defaults
  to `shellnet.ackinacki.org`; stage/prod ship their own.
- `graphql`: gateway URL for on-demand PrivateNote BOC reads. `endpoint`
  (HTTP URL) and `request_timeout_ms` (per-request budget for BOC fetch).
  `page_size` defaults to 100 and may be omitted; it is used by the
  indexer's paginated fetches but not by the API tier.

The `auth.kek_hex` field is the 32-byte master key used to encrypt
`api_secret` and `pn_seckey` at rest. `config/api.local.yaml` ships a
shared dev value; stage and prod configs carry their own KEKs assembled
by CI from the secret store.

Local-only env vars (`TEST_DATABASE_URL`) live in `.env`. The file is
gitignored; copy from the committed template on first checkout:

```sh
cp .env.example .env
```

CI sets the same variables directly in its workflow env block.

## Running

```sh
cargo run -p dodex-api
```

The API needs a Postgres database. For market-data responses, run the indexer
against the same database first.

Smoke checks:

```sh
curl -s 'http://localhost:8080/readiness'
curl -s 'http://localhost:8080/api/v1/markets?limit=5' | jq
```

## Tests

Unit tests:

```sh
cargo test --workspace --lib
```

DB-backed API tests:

```sh
docker compose -f docker-compose.test.yml up -d --wait
cargo test -p dodex-api --tests -- --test-threads=1
docker compose -f docker-compose.test.yml down
```

Every test binary loads `.env` via `dotenvy::dotenv()` at setup, so no
manual `export` is required after the first-checkout `cp .env.example .env`.

The test database role must own `public` because the test suite runs migrations
on connect.

## Deployment

Self-hosting the service (Docker Compose, own GraphQL + Postgres/Supabase) is
covered in [docs/deployment.md](../../docs/deployment.md); the repository-level
entry point is [README.md](../../README.md).
