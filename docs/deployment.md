# Self-hosting DODEX: `indexer` + `api`

How to run the two DODEX backend services on your own server and wire them to
your own Acki Nacki GraphQL endpoint and your own Postgres (self-managed or
Supabase).

This guide uses **Docker Compose** — the vehicle the repository already ships
(`docker/api.Dockerfile`, `docker/indexer.Dockerfile`, `docker-compose.yml`,
and the `docker-compose.stage.yml` override). For the service-level config
reference see [`services/api/README.md`](../services/api/README.md) and
[`services/indexer/README.md`](../services/indexer/README.md); for the config
schema itself see `crates/infrastructure/src/config.rs`.

## What connects to what

```
                 ┌──────────────────────────────┐
   reads BOC ───▶│  Acki Nacki node / Block     │◀── submits external
   GraphQL       │  Manager (your own GraphQL)  │    messages (gateway)
                 └──────────────────────────────┘
                     ▲                    ▲
                     │ graphql.endpoint   │ graphql.endpoint
                     │                    │ chain.gateway_endpoint
              ┌──────┴──────┐      ┌──────┴──────┐
              │   indexer   │      │     api     │  :8080 REST
              └──────┬──────┘      └──────┬──────┘
        writes read- │                    │ reads read-model,
        model        ▼                    ▼ POSTs trades to chain
                 ┌──────────────────────────────┐
                 │  Postgres (self-hosted or    │
                 │  Supabase)                   │
                 └──────────────────────────────┘
```

- **`indexer`** polls the Acki Nacki GraphQL `blockchain_events` stream, decodes
  DEX events, and writes the Postgres read-model. It applies SQL migrations on
  startup. It has no HTTP port — it is a background worker.
- **`api`** serves the public REST API on `:8080` from the Postgres read-model,
  reads PrivateNote BOCs on demand from GraphQL (for `/api/v1/account` and
  `/account/balances`), and send external messages to the blockchain gateway for the
  trading path.

Both services read GraphQL; only the `api` writes to the chain gateway.

> **GraphQL must be Acki Nacki–compatible.** The "GraphQL endpoint" is an Acki
> Nacki node's GraphQL API (the Block Manager). The indexer queries the standard
> `blockchain_events` stream and reads account BOCs — it is **not** a generic
> GraphQL server you can swap for an arbitrary schema.

`services/market-manager` is a separate, optional service and is out of scope here.

## Prerequisites

- A server with Docker Engine and the Docker Compose plugin.
- A reachable **Acki Nacki GraphQL endpoint** (your own Block Manager, a provider,
  or `https://shellnet.ackinacki.org/graphql`).
- A **Postgres 14+** database — either self-managed or a Supabase project — that
  both services can reach.
- This repository checked out on the server (the Compose build context is the
  repo root; the Dockerfiles compile the Rust binaries from source).

## Step 1 — Your GraphQL / chain endpoint

Decide the two endpoint values both services will use:

| Config field | Used by | Example (public shellnet) | Shape |
| --- | --- | --- | --- |
| `graphql.endpoint` | indexer + api | `https://shellnet.ackinacki.org/graphql` | full HTTP(S) URL ending in `/graphql` |
| `chain.gateway_endpoint` | api only | `shellnet.ackinacki.org` | bare host (no scheme), as in `config/api.local.yaml` |

If you run your own node, substitute your node's host/URL. Keep the **shapes**
exactly as shown above — `graphql.endpoint` is a full URL, `chain.gateway_endpoint`
is the bare host the SDK's `Dex::from_endpoints` expects.

## Step 2 — Postgres or Supabase

### Self-managed Postgres

Create a database and an application role:

```sql
CREATE DATABASE dodex;
CREATE ROLE dodex WITH LOGIN PASSWORD 'change-me';
GRANT ALL PRIVILEGES ON DATABASE dodex TO dodex;
```

The connection string then looks like:

```
postgres://dodex:change-me@db-host:5432/dodex
```

You do **not** need to apply migrations by hand — the indexer runs
`sqlx::migrate!` from [`migrations/`](../migrations/) on startup (and the api
does too when `auth.seed_accounts` is on). The role must be allowed to create
objects in `public`.

### Supabase

Use the **connection pooler** string from your Supabase project (Project
Settings → Database → Connection pooling). It has the form:

```
postgresql://<role>.<project-ref>:<password>@aws-...pooler.supabase.com:5432/postgres
```

On a fresh project, grant the pooler role permissions on `public` **before the
first run** (run this in the Supabase SQL editor; replace `indexer` with the
role name embedded in your pooler username):

```sql
grant usage, create on schema public to indexer;
grant all privileges on all tables    in schema public to indexer;
grant all privileges on all sequences in schema public to indexer;
alter default privileges in schema public grant all on tables    to indexer;
alter default privileges in schema public grant all on sequences to indexer;
```

> **URL-encode special characters in the password.** The password lives inline
> in the connection URL, so reserved characters must be percent-encoded —
> `$` → `%24`, `@` → `%40`, `:` → `%3A`, `/` → `%2F`. A raw `$` or `@` in the
> password will otherwise be misparsed and the connection will fail.

## Step 3 — Write your config files

Config is plain YAML selected by the `APP_CONFIG` environment variable. **There
is no environment-variable interpolation** — every value, including secrets,
lives in the YAML file itself. The model is therefore: keep a local, untracked
config file with real credentials and point `APP_CONFIG` at it (exactly how the
committed-but-gitignored `config/*.stage.supabase.yaml` files work).

> **Keep credentials out of git.** Name your files so they match an ignored
> pattern, or add them to [`.gitignore`](../.gitignore). For example, add:
> ```
> config/*.prod.yaml
> ```
> Never commit a config file that contains a real database password or KEK.

### Generate a KEK

`auth.kek_hex` is the 32-byte (64 hex chars) key that encrypts `api_secret` and
`pn_seckey` at rest. Generate a fresh one per environment — **do not reuse the
shared dev value from `config/api.local.yaml`**:

```sh
openssl rand -hex 32
```

### `config/api.prod.yaml`

```yaml
app:
  env: prod
  log_level: info

server:
  host: 0.0.0.0
  port: 8080
  # Must exceed every chain.*_timeout_ms AND graphql.request_timeout_ms below,
  # or startup validation fails. 35s = 30s chain + ~5s slack.
  request_timeout_ms: 35000

database:
  url: postgres://dodex:change-me@db-host:5432/dodex
  max_connections: 10
  min_connections: 1
  connect_timeout_ms: 3000

auth:
  kek_hex: "<output of `openssl rand -hex 32`>"
  default_recv_window_ms: 5000
  max_recv_window_ms: 60000      # spec ceiling; cannot exceed 60000
  # On first boot, applies migrations and inserts seed accounts. Turn OFF once
  # the schema exists and you manage accounts yourself; with it off the api
  # stays read-only against the schema and relies on the indexer for migrations.
  seed_accounts: true

chain:
  gateway_endpoint: shellnet.ackinacki.org   # your Acki Nacki node host
  place_order_timeout_ms: 30000
  cancel_order_timeout_ms: 30000
  place_batch_timeout_ms: 30000
  cancel_batch_timeout_ms: 30000
  split_full_set_timeout_ms: 30000

graphql:
  endpoint: https://shellnet.ackinacki.org/graphql   # your Acki Nacki GraphQL
  request_timeout_ms: 10000
```

### `config/indexer.prod.yaml`

```yaml
app:
  env: prod
  log_level: info

database:
  # Point at the SAME database the api uses.
  url: postgres://dodex:change-me@db-host:5432/dodex
  max_connections: 10
  min_connections: 1
  connect_timeout_ms: 3000

graphql:
  endpoint: https://shellnet.ackinacki.org/graphql   # your Acki Nacki GraphQL
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

The api and indexer **must share one database**: the indexer writes the
read-model the api serves.

#### Validation rules enforced at startup

Both services validate their config on load and refuse to start on violation.
The `auth`, `server`, and `chain` sections exist only in the api config, so the
rules over them are checked by the api alone — the indexer config has no `auth`
section and never reads a KEK. The non-obvious ones:

Shared (both services):

- Config structs use `deny_unknown_fields` — a typo'd or stray key is a hard
  parse error, not a silent ignore.
- `database.url` non-empty, `max_connections > 0`,
  `max_connections >= min_connections`, and pool/timeout values `> 0`.
- `graphql.endpoint` non-empty and `graphql.request_timeout_ms > 0`.

api only:

- `auth.kek_hex` must be exactly 32 bytes of hex (64 chars).
- `auth.max_recv_window_ms` must be `<= 60000`; `default_recv_window_ms <= max_recv_window_ms`.
- `chain.gateway_endpoint` non-empty and every `chain.*_timeout_ms > 0`.
- `server.request_timeout_ms` must be **strictly greater than** every
  `chain.*_timeout_ms` and than `graphql.request_timeout_ms` — otherwise the
  HTTP timeout could fire while a chain submission or BOC read is still in
  flight.

## Step 4 — Compose override, build, and run

The base `docker-compose.yml` mounts `./config` into each container read-only
(`/app/config`) and defaults `APP_CONFIG` to the `*.local.yaml` files. Add an
override that points `APP_CONFIG` at your own files — mirroring how
`docker-compose.stage.yml` selects the Supabase configs.

Create `docker-compose.prod.yml`:

```yaml
services:
  api:
    environment:
      APP_CONFIG: /app/config/api.prod.yaml
      RUST_LOG: info

  indexer:
    environment:
      APP_CONFIG: /app/config/indexer.prod.yaml
      RUST_LOG: info
```

Build the images and start both services:

```sh
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --build
```

Because `./config` is bind-mounted read-only, your `*.prod.yaml` files are read
at container start — editing them and restarting picks up changes without a
rebuild. Rebuild (`--build`) only when the Rust source changes.

## Step 5 — Verify

```sh
# api readiness (returns "ok" once the process is accepting traffic;
# note this probe does NOT check the database connection)
curl -s http://localhost:8080/readiness

# a real read path — exercises Postgres
curl -s 'http://localhost:8080/api/v1/markets?limit=5' | jq

# indexer is making progress (look for the resumed-from-cursor line and
# steadily advancing event ingestion)
docker compose -f docker-compose.yml -f docker-compose.prod.yml logs -f indexer
```

Until the indexer has ingested chain events into the read-model, market-data
endpoints return empty results — that is expected on a cold database.

## Operations

### Migrations

Applied automatically on startup: the indexer always runs them; the api runs
them only when `auth.seed_accounts: true`. `sqlx::migrate!` takes an advisory
lock, so the two racing on a fresh database is safe.

### Changing config

- **api** is restart-to-reconfigure — none of its live request paths read config
  at runtime (pool, bind address, and timeouts are fixed at startup). Restart
  the container after editing `api.prod.yaml`.
- **indexer** hot-reloads a subset of its config on `SIGUSR1` (its main fetch
  loop). Changes to the database pool, GraphQL endpoint used by the reconciler
  tasks, and other startup-pinned values still require a restart — when in
  doubt, restart. See [`services/indexer/README.md`](../services/indexer/README.md).

```sh
# restart a service to apply config changes
docker compose -f docker-compose.yml -f docker-compose.prod.yml restart api

# send SIGUSR1 to the indexer for its hot-reloadable subset
docker compose -f docker-compose.yml -f docker-compose.prod.yml kill -s SIGUSR1 indexer
```

### Logs

```sh
docker compose -f docker-compose.yml -f docker-compose.prod.yml logs -f api indexer
```

Log verbosity is controlled by `RUST_LOG` (set in the override) and
`app.log_level` in config.

### Upgrades

Pull the new code and rebuild:

```sh
git pull
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --build
```

New migrations in `migrations/` apply automatically on the next start.

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| Process exits immediately with a config error | A validation rule from Step 3 failed — read the error; common cases are `request_timeout_ms` not exceeding a chain/graphql timeout, a bad `kek_hex` length, or an unknown/typo'd YAML key. |
| `database.url must not be empty` / connection refused | Wrong or unreachable `database.url`; for Supabase, verify the pooler host/port and that the password is percent-encoded. |
| Permission denied on `public` (Supabase) | The grant block in Step 2 was not run for the pooler role. |
| api `/readiness` is `ok` but `/markets` is empty | Normal on a cold DB — wait for the indexer to ingest events; check indexer logs for progress and GraphQL connectivity. |
| Indexer cannot reach GraphQL | `graphql.endpoint` wrong/unreachable, or it is not an Acki Nacki–compatible endpoint exposing the `blockchain_events` stream. |
