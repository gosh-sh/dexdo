# Self-hosting DEX.DO: `indexer` + `api`

How to run the two DEX.DO backend services on your own server and wire them to
your own Acki Nacki GraphQL endpoint and your own Postgres (self-managed or
Supabase).

This guide uses **Docker Compose** — the vehicle the repository already ships
(`docker/api.Dockerfile`, `docker/indexer.Dockerfile`, `docker-compose.yml`,
and the `docker-compose.stage.yml` override). For the service-level config
reference see [`services/api/README.md`](../services/api/README.md) and
[`services/indexer/README.md`](../services/indexer/README.md); for the config
schema itself see `crates/infrastructure/src/config.rs`.

To deploy with **Ansible** instead of running Compose by hand — one playbook
that provisions the host, renders the config, and brings the stack up — see
[`deploy/ansible/README.md`](../deploy/ansible/README.md).

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

| Config field | Used by | Example (public Shellnet) | Shape |
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
  # Seeding only writes DB rows — the PrivateNote contracts those accounts
  # point at must be deployed and funded on-chain first. See
  # docs/seed-private-notes.md.
  seed_accounts: true
  seed_accounts_path: ./config/seed_notes_list.json  # required when seed_accounts is on

chain:
  gateway_endpoint: shellnet.ackinacki.org   # your Acki Nacki node host
  place_order_timeout_ms: 30000
  cancel_order_timeout_ms: 30000
  place_batch_timeout_ms: 30000
  cancel_batch_timeout_ms: 30000
  split_full_set_timeout_ms: 30000
  max_batch_size: 10   # batch cap for /batchOrders; must not exceed the chain's MAX_BATCH_SIZE

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
  reprojection_batch_size: 500
  oracle_event_list_reconciliation_interval_ms: 60000
  dapp_id: "<dexdo-dapp-id>"   # scopes ingestion to this dapp; omit to disable
  ignored_addresses:
    - "0:1111111111111111111111111111111111111111111111111111111111111111"
  ignored_event_types:
    - "OrderBook.Queued"
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

indexer only:

- `indexer.dapp_id` (optional): when set, scopes ingestion to the DEXDO dapp
  whose `src_dapp_id` matches — foreign chain events are dropped before decode.
  Edges with no `src_dapp_id` are kept. Omit the key (or leave it commented) to
  disable scoping. An empty string is rejected at startup — it would otherwise
  deserialize to `Some("")` and drop every edge with a real `src_dapp_id`.
- `indexer.ignored_event_types` may list only known droppable no-op types
  (`OrderBook.Queued` / `FullyFilled` / `Rejected` / `CallbackBounced` and
  `PMP.StakeAccepted` / `PMP.MergeProcessed`). Each
  entry is matched by its external `dst` before decode (no decode cost). The
  startup guard refuses anything else — metric-critical types
  (`OrderBook.OrderPlaced`, `OrderBook.PartialFill`, counted from `raw_events`
  for the OTLP metrics), state-changing types, and typos — so a bad list
  refuses startup rather than silently dropping nothing or corrupting the read
  model.

## Step 4 — Compose override, build, and run

The base `docker-compose.yml` mounts `./config` into each container read-only
(`/app/config`), bind-mounts a per-service host log directory
(`./logs/api`, `./logs/indexer` → `/app/logs`) with `LOG_DIR=/app/logs` set,
and defaults `APP_CONFIG` to the `*.local.yaml` files. Add an override that
points `APP_CONFIG` at your own files — mirroring how
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
curl -s 'http://localhost:8080/api/v1/prediction/markets?limit=5' | jq

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

### `raw_events` retention (optional)

`raw_events` is the append-only event log and by far the largest table — every
ingested message edge lands there, and nothing prunes it on its own. The live
projection loop only needs rows with `processed_at IS NULL`; already-projected
rows are kept solely for reprojection and on-call heals. Left unbounded it grows
without limit, so on a long-lived database schedule a retention job.

[`deploy/sql/prune_raw_events.sql`](../deploy/sql/prune_raw_events.sql) installs a
[pg_cron](https://github.com/citusdata/pg_cron) job that deletes **processed**
rows older than a retention window, in small batches (pending rows are never
touched). Run the file once as a privileged role — on Supabase, the SQL editor
(which runs as `postgres`); pg_cron must be enabled in the `postgres` database.
The pooler application role (the `indexer` user) is not a superuser and can
neither `CREATE EXTENSION` nor schedule jobs.

It schedules a job named `prune-raw-events` to run daily at 03:00 **UTC** with a
3-day window. Rows are deleted only once they age past the window, so the job is
a no-op until the database holds more than the window's worth of history.

Change the window — re-running `cron.schedule` with the same job name updates it
in place (here, 14 days):

```sql
select cron.schedule('prune-raw-events', '0 3 * * *',
  $$ call public.prune_raw_events(interval '14 days', 10000) $$);
```

Inspect recent runs, or disable the job:

```sql
select status, return_message, start_time, end_time
  from cron.job_run_details
 where jobid = (select jobid from cron.job where jobname = 'prune-raw-events')
 order by start_time desc limit 10;

select cron.unschedule('prune-raw-events');
```

After the first large purge, dead tuples are reclaimed for reuse by autovacuum;
to return disk to the OS, run `vacuum full raw_events` once during a quiet window
(it takes an `ACCESS EXCLUSIVE` lock — never put it in cron). See the file header
for the full operations notes.

### API credentials for clients

The seeder stores no `api_secret` — each one is derived from `auth.kek_hex` and
the note's slot index (see
[docs/seed-private-notes.md](seed-private-notes.md#api-credentials-are-derived-not-in-the-file)).
To recover the `api_key` / `api_secret` to hand a client, re-derive them from
this environment's KEK:

```sh
cargo run -p dodex-api --bin dump_creds -- --kek <auth.kek_hex> --count <N>
```

It prints the pairs for the first `N` slots in cleartext — run it on a trusted
host and never log or commit the output.

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

Each service writes to **both** stdout and a host-mounted directory. The base
`docker-compose.yml` bind-mounts `./logs/api` and `./logs/indexer` (on the host)
to `/app/logs` (in each container) and sets `LOG_DIR=/app/logs`. With `LOG_DIR`
set, the service writes daily-rotated, human-readable files named
`<service>.log.<YYYY-MM-DD>` into that directory, keeping at most `LOG_MAX_FILES`
of them (default 14).

The indexer additionally writes a second daily-rotated file,
`indexer.noise.log.<YYYY-MM-DD>` (same `LOG_MAX_FILES` retention), carrying the
high-volume, low-value "projector has no handler for event type" repeats. The
first sighting of each unseen type still goes to stdout and `indexer.log`; only
the repeats are diverted here, so this file stays quiet unless a deployed
contract is steadily emitting an event the indexer does not yet handle. Other
services build the noise appender too, so an empty `<service>.noise.log.<date>`
is created, but only the indexer ever writes to it.

```sh
# tail the live stdout stream (unchanged)
docker compose -f docker-compose.yml -f docker-compose.prod.yml logs -f api indexer

# the persisted files on the host (survive container removal / redeploy)
tail -f logs/api/api.log.*
tail -f logs/indexer/indexer.noise.log.*   # diverted "no handler" repeats
ls -1 logs/indexer/
```

Notes:

- The containers run as `root`, so files under `logs/` are root-owned — use
  `sudo` to read/rotate them as a non-root user.
- `LOG_DIR` and `LOG_MAX_FILES` are environment variables (there is no YAML
  config key). Unset `LOG_DIR` to disable file logging and keep stdout only.
- Verbosity is still controlled by `RUST_LOG` (set in the override) and
  `app.log_level` in config; the same filter applies to stdout and files.

### Metrics & Grafana

The **indexer** exports OpenTelemetry metrics over OTLP — but **only when an
endpoint env var is set**: `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` (or the generic
`OTEL_EXPORTER_OTLP_ENDPOINT`). With neither set, no meter provider is created and
nothing is collected; the service runs unaffected. (The api does not export
metrics.) Set it in your Compose override next to `APP_CONFIG`:

```yaml
  indexer:
    environment:
      APP_CONFIG: /app/config/indexer.prod.yaml
      OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-collector:4317
```

The indexer refreshes its DB-derived metric caches every 15s and the OTLP reader
pushes every 30s, under `service.name=dodex-indexer`. To see them in Grafana,
route the OTLP stream into Prometheus (an OpenTelemetry Collector with a
`prometheus` / `prometheusremotewrite` exporter) and point Grafana at that
Prometheus. The full metric catalog — what each gauge and counter measures — is
in [`docs/tech-specs/indexer.md`](tech-specs/indexer.md#metrics).

The repository ships a ready dashboard and alert rules under `deploy/grafana/`:

| File | What it is | How to use |
| --- | --- | --- |
| [`deploy/grafana/dodex-indexer-dashboard.json`](../deploy/grafana/dodex-indexer-dashboard.json) | Grafana dashboard covering every indexer metric — ingestion (`raw_events` counters), projection pipeline (backlog/lag/cursor age/fallbacks), DB pool, and inference markets (state, order depth, reconcile failures, price/sweep staleness) | Grafana → Dashboards → Import → Upload JSON; pick your Prometheus when prompted |
| [`deploy/grafana/provisioning/alerting/dodex-indexer-alerts.yaml`](../deploy/grafana/provisioning/alerting/dodex-indexer-alerts.yaml) | 12 Grafana-managed alert rules (projection/cursor lag, decode errors, inference markets `failing`, reference-price & sweep staleness), warning→critical | Copy to Grafana's `/etc/grafana/provisioning/alerting/` and restart Grafana |

Two setup notes, also documented in the files themselves:

- **Counter suffix.** The OTel→Prometheus exporter appends `_total` to monotonic
  counters by default (`add_metric_suffixes: true`). The dashboard exposes a
  `counter_suffix` template variable (default `_total`; switch to `(none)` if your
  collector disables it); the alert rules match `…(_total)?` so they fire either
  way. Gauge metrics get no suffix.
- **Alert data source UID.** Provisioned alert rules reference the Prometheus data
  source by UID. Before installing, replace the placeholder with yours (found
  under Connections → Data sources):
  ```sh
  sed -i 's/REPLACE_WITH_PROMETHEUS_DS_UID/<your-uid>/g' \
    deploy/grafana/provisioning/alerting/dodex-indexer-alerts.yaml
  ```

Alert thresholds (lag/age cutoffs, `failing > 0`, decode-error rate) mirror the
dashboard's panel thresholds and are conservative starting points — retune them
and the `for:` durations against your real traffic and SLOs.

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
