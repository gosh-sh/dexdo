# dodex-indexer

Writer-side service. It ingests Acki Nacki chain data, decodes DEX events, and
maintains the Postgres read-model served by `services/api`.

## Specifications

- Indexer implementation: [docs/tech-specs/indexer.md](../../docs/tech-specs/indexer.md).
- Data schema: [docs/tech-specs/data-schema.md](../../docs/tech-specs/data-schema.md).
- API consumer of the read-model: [docs/tech-specs/read-api.md](../../docs/tech-specs/read-api.md).
- Public API requirements served from the read-model: [docs/api-spec.md](../../docs/api-spec.md).
- Contract/event references: [docs/contract-specs/dex-events-routing.md](../../docs/contract-specs/dex-events-routing.md) and [docs/contract-specs/](../../docs/contract-specs/).

Implementation details belong in the tech specs above, not in this README.

## Configuration

Config file: `config/indexer.<env>.yaml`. Local default:
`config/indexer.local.yaml`. Override with `APP_CONFIG=/path/to/file.yaml`.

Config sections:

- `app`: environment name and log level.
- `database`: Postgres URL and pool settings.
- `graphql`: gateway endpoint, page size, request timeout.
- `indexer`: polling/reconciliation/reprojection intervals and ignored addresses.

## Database

The indexer applies SQL migrations from `migrations/` on startup. Column and
table semantics live in [docs/tech-specs/data-schema.md](../../docs/tech-specs/data-schema.md).

On a fresh Supabase project, grant the application role permissions on `public`
before the first run:

```sql
grant usage, create on schema public to indexer;
grant all privileges on all tables    in schema public to indexer;
grant all privileges on all sequences in schema public to indexer;
alter default privileges in schema public grant all on tables    to indexer;
alter default privileges in schema public grant all on sequences to indexer;
```

Replace `indexer` with the role name from the pooler username.

## Running

```sh
cargo run -p dodex-indexer
```

Reload config in a running process:

```sh
kill -USR1 <pid>
```

Restart the process for changes that are documented as startup-pinned in the
indexer tech spec.

## Tests

Unit tests:

```sh
cargo test -p dodex-infrastructure --lib
```

DB-backed indexer/infrastructure tests:

```sh
docker compose -f docker-compose.test.yml up -d --wait
export TEST_DATABASE_URL=postgres://dodex:dodex@localhost:55432/dodex_test
cargo test -p dodex-infrastructure --tests
docker compose -f docker-compose.test.yml down
```

The test database role must own `public` because the test suite runs migrations
on connect.

## Deployment

Use the repository-level deployment process from [README.md](../../README.md).
