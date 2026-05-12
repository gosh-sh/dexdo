# dodex-api

Reader-side HTTP service. Salvo server serving market metadata and order-book
snapshots straight from the Postgres read-model — never touches GraphQL, the
on-chain state, or BOC decoding (that is the indexer's job).

Behaviour, request/response shapes, status enum, error mapping:

- public contract — [`docs/api-spec.md`](../../docs/api-spec.md)
- implementation notes — [`docs/tech-specs/market-data-api.md`](../../docs/tech-specs/market-data-api.md)
- read-model schema — [`docs/tech-specs/data-schema.md`](../../docs/tech-specs/data-schema.md)

## Routes

| Method | Path              | Notes                                    |
| ------ | ----------------- | ---------------------------------------- |
| GET    | `/readiness`      | Liveness probe; always returns `200 ok`. |
| GET    | `/api/v1/markets` | Implemented.                             |
| GET    | `/api/v1/depth`   | Implemented.                             |

## Configuration

YAML at `config/api.<env>.yaml`. Override path with `APP_CONFIG=/path/to/file.yaml`.
`serde(deny_unknown_fields)` rejects any unknown key.

```yaml
app:
  env: local
  log_level: info

server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000

database:
  url: postgres://postgres:postgres@localhost:5432/dodex
  max_connections: 10
  min_connections: 1
  connect_timeout_ms: 3000
```

The API is restart-to-reconfigure: every value (database pool, listen
host/port, server timeouts) is captured at startup and not re-read. There is
no SIGUSR1 reload here — restart the service to pick up edits. (The indexer
**does** support SIGUSR1 for a narrow subset of its config; see
[`services/indexer/README.md`](../indexer/README.md).)

## Running

```sh
cargo run -p dodex-api
```

Requires a Postgres database populated by the indexer (the indexer applies
migrations on startup). Without ingest data the listing is empty but the
endpoint itself works.

Smoke check:

```sh
curl -s 'http://localhost:8080/readiness'
curl -s 'http://localhost:8080/api/v1/markets?limit=5' | jq
curl -s 'http://localhost:8080/api/v1/depth?marketAddress=…&symbol=…' | jq
```

## Tests

Unit tests live alongside the implementation in `crates/infrastructure`:

```sh
cargo test -p dodex-infrastructure
```

Integration tests exercise the read path against a real Postgres and are
gated on `TEST_DATABASE_URL`. The repo ships a throw-away harness in
`docker-compose.test.yml`:

```sh
docker compose -f docker-compose.test.yml up -d --wait
export TEST_DATABASE_URL=postgres://dodex:dodex@localhost:55432/dodex_test
cargo test -p dodex-infrastructure
docker compose -f docker-compose.test.yml down
```

To point at an existing database instead, just export `TEST_DATABASE_URL` to
its URL — the suite runs `database::run_migrations` on connect, so the role
must own `public`.