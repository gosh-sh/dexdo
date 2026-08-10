# Deploying DEX.DO with Ansible

An Ansible role + playbook to deploy the `api` + `indexer` services to your own
host. It renders the config + compose file and pulls the pre-built images from
the registry — the host builds nothing. For the manual / from-scratch Compose
path and the meaning of every config field, see
[`docs/deployment.md`](../../docs/deployment.md).

## How it works

```
deploy-dexdo.yml → role dexdo   on the host: render config + compose →
                                docker compose up --pull always
```

Images are built and pushed separately (`dexdo-api` / `dexdo-indexer`), and the
deploy pulls them by tag — so the host only needs Docker + registry access, not
a source checkout or a compiler.

## Layout

```
ansible.cfg                       defaults
deploy-dexdo.yml                  applies the dexdo role to the `dexdo` group
inventories/
  vault.example.yml               secrets template -> copy into each env's group_vars/all/vault.yml
  stage.example/
    hosts.yml                     example env inventory (copy the dir to <env>/)
  <env>/                          your env — gitignored
    hosts.yml                     host(s) + dexdo_env + per-env overrides
    group_vars/all/vault.yml      encrypted secrets for THIS env only
roles/dexdo/
  defaults/main.yml               non-secret vars (env, registry/image tag, endpoints, knobs) — override per env in inventory
  tasks/main.yml                  dirs, render templates, docker compose up --pull always
  handlers/main.yml               restart stack on config change
  templates/                      env-agnostic — env name comes from {{ dexdo_env }}
    compose.yml.j2                image-based compose (pulls from registry) -> <deploy_dir>/compose.yml
    api.yaml.j2                   rendered to <deploy_dir>/config/api.<env>.yaml
    indexer.yaml.j2               rendered to <deploy_dir>/config/indexer.<env>.yaml
    logrotate.j2                  rotation script for the logrotate sidecar -> <deploy_dir>/logrotate.sh
```

## Logging

The api + indexer write their logs to files under `dexdo_logs_dir/<service>/`
(bind-mounted to `/app/logs`). That dir defaults to `<deploy_dir>/logs`; point it
at a separate disk/mount per env to keep logs off the system disk or surviving a
redeploy. The services themselves
rotate **daily** (`<service>.log.<date>` + `<service>.noise.log.<date>`
via the `dodex-logging` crate, pruned to `LOG_MAX_FILES` days), so rotation is
handled out of the box.

Daily rotation bounds age but not size. If you also need a **size** cap, the
role ships an **optional `logrotate` sidecar** (off by default —
`dexdo_logrotate_enabled`) that rotates the dated files by size with
`copytruncate` (mandatory, since the services keep the files open), driven by
busybox cron, in place; the app keeps appending to the current day's file.

Knobs (in `roles/dexdo/defaults/main.yml`, overridable per env):

- `dexdo_logs_dir` (default `<deploy_dir>/logs`) — host dir the logs are written
  to; point it at a data mount to keep logs off the system disk.
- `dexdo_logrotate_enabled` (default `false`) — the app already rotates daily;
  turn on only when you also want a size cap on top.
- `dexdo_logrotate_image` — image providing `logrotate` + `crond` + bash.
- `dexdo_log_rotate_size` (default `2G`) / `dexdo_log_rotate_amount`
  (default `10`) — rotate at this size, keep this many compressed copies.
- `dexdo_log_rotate_spec` (default `"*/5 *"`) — cron "minute hour" for the run
  (every 5 minutes).

For the app-side `LOG_DIR` / `LOG_MAX_FILES` knobs see
[docs/deployment.md](../../docs/deployment.md#logs).

## Environments

Each environment is its own directory under `inventories/` — an inventory
(`hosts.yml`) plus an adjacent `group_vars/`. Ansible loads `group_vars`
relative to the inventory file, so **each env has its own vault**
(`inventories/<env>/group_vars/all/vault.yml`) and they never share secrets.

The environment name is a single variable, `dexdo_env`, set in that env's
`hosts.yml`. It drives the rendered config file names (`api.<env>.yaml`,
`indexer.<env>.yaml`), the `app.env` field, and `APP_CONFIG` — nothing in the
templates hardcodes an environment.

Defaults for every non-secret value live in `roles/dexdo/defaults/main.yml`
(lowest precedence). To diverge for an environment, set the value under the
`dexdo` group in that env's `hosts.yml` (it wins over the role default). Adding
an environment = copy `inventories/stage.example/` to `inventories/<env>/`,
change `dexdo_env`, drop in the env's vault, and override what differs.

## Database

`dexdo_db_local` chooses where Postgres comes from:

- **`false` (default) — connect to an existing database.** Set `dexdo_db_host`,
  `dexdo_db_port`, `dexdo_db_name`, `dexdo_db_user` (in the env inventory) and
  `vault_dexdo_db_password` (vault) to point at it (managed/Supabase/etc.). No
  `postgres` service is rendered into the compose file.
- **`true` — run a bundled Postgres 16** as the `postgres` service, with data on
  the named docker volume `dodex_pgdata` (persists across redeploys). api and
  indexer wait for it to be healthy and connect over the compose network at
  `postgres:5432`; keep `dexdo_db_host: postgres`.

Either way the connection string is built from `dexdo_db_user` / `dexdo_db_name`
/ `dexdo_db_host` / `dexdo_db_port` + `vault_dexdo_db_password`, and migrations
run automatically on startup (`sqlx::migrate!`).

### Migration checksums and schema recreation

`sqlx::migrate!` records a checksum per applied migration file in
`_sqlx_migrations`. Editing an already-applied migration file (as opposed to
adding a new one) changes its checksum, so any database that already recorded
the old checksum refuses to start — `sqlx::migrate!` errors out rather than
reapplying it. When a release folds a schema change into an existing
migration file instead of shipping a new one, **the target database must be
dropped and recreated before deploying that release.** This destroys all
indexed data; the indexer rebuilds the read-model from chain with no operator
backfill step, but market-data endpoints will read empty until ingestion
catches back up (see [Verify](#verify)).

### Rollout order: stop the stack, then wipe, then deploy

Stop the running stack **before** wiping the database, and only deploy the
new release once the database is empty. Wiping while the old stack is still
up lets the old indexer reapply *its* (unedited) migration file into the now-empty
schema; the new indexer's edited version of that file then fails its checksum
check against what the old indexer just recorded, and refuses to start.

There is a second, data-correctness reason to keep this order. In the window
between the wipe and the old stack actually stopping, the old projector can
still write rows for a column a schema fold expects a reconciler sweep to
backfill progressively — for example `inference_orders.token_contract`. A
row that reaches a terminal status (`FILLED` / `CANCELLED`) before the sweep
visits it keeps that column `NULL` permanently: the sweep only probes `OPEN`
rows, and the `/api/v1/inference/orders` read-side gate only vouches for
`OPEN` (`LIVE`) rows too, so a historical NULL like this is invisible to any
later online repair. Following stop → wipe → deploy makes that window
unreachable.

### The indexer applies migrations; start it before or with the API

`sqlx::migrate!` runs unconditionally from the indexer's `main.rs` at every
startup. The API runs it too, but **only** when `auth.seed_accounts=true`
(`services/api/src/lib.rs:2445-2459`); with the flag off — the normal
deployed configuration — the API stays a read-only client of whatever schema
already exists. The rendered compose file starts `api` and `indexer`
concurrently with no `depends_on` between them, so on a freshly recreated
database the API container can come up first and query tables the migration
hasn't created yet. This fails closed: affected requests return an internal
error, never a wrong answer, and the condition self-heals with no operator
action once the indexer finishes applying migrations. Deploying or starting
the indexer before or together with the API avoids the transient outright.

## Metrics

The indexer can export OTLP metrics. A rendered `.env` next to the compose file
(loaded by the indexer via `env_file`) carries `OTEL_EXPORTER_OTLP_ENDPOINT`,
`OTEL_EXPORTER_OTLP_METRICS_PROTOCOL`, and `OTEL_SERVICE_NAME`. Set
`dexdo_otel_endpoint` (per env) to your OTLP collector to enable them; left empty
(the default) the indexer collects nothing.

## One-time setup (per environment)

1. **Inventory** — `cp -r inventories/stage.example inventories/<env>`, then edit
   `inventories/<env>/hosts.yml`: set `dexdo_env`, the host(s), and SSH user.
2. **Secrets** — `mkdir -p inventories/<env>/group_vars/all` and
   `cp inventories/vault.example.yml inventories/<env>/group_vars/all/vault.yml`,
   fill in a `db_password` (e.g. `openssl rand -hex 24`) and a fresh `kek_hex`
   (`openssl rand -hex 32`), then
   `ansible-vault encrypt inventories/<env>/group_vars/all/vault.yml`. This vault
   is loaded only for `<env>`.
3. **Non-secret vars** — review `roles/dexdo/defaults/main.yml`: `dexdo_registry`
   (where the images are pulled from) plus the Acki Nacki endpoints. Override any
   of these per environment in that env's `hosts.yml`.
4. **Host prerequisites** — Docker Engine + Compose plugin and an SSH user that
   can `sudo` and run `docker` (the playbook uses `become: true`). The host must
   be authenticated to `dexdo_registry` (`docker login`, out of band) so it can
   pull the images. No `git` or compiler needed — nothing is built on the host.

## Run

From this directory:

```sh
# deploy a specific image tag (default is `latest-dev` if omitted)
ansible-playbook deploy-dexdo.yml -i inventories/<env>/hosts.yml --ask-vault-pass -e dexdo_image_tag=<tag|sha>

# dry run
ansible-playbook deploy-dexdo.yml -i inventories/<env>/hosts.yml --ask-vault-pass --check --diff
```

(Drop `--ask-vault-pass` if the env vault is left unencrypted.)

## Verify

```sh
curl -s http://<host>:<dexdo_api_port>/readiness        # dexdo_api_port default 8080
curl -s 'http://<host>:<dexdo_api_port>/api/v1/prediction/markets?limit=5' | jq
```

Market-data endpoints are empty until the indexer has ingested chain events —
normal on a cold database.

### When `/api/v1/inference/orders` refuses to answer

This endpoint fails closed rather than risk a false "not in use" for a
`tokenContract` query: it returns `-1500` / HTTP 503 (retry) instead of an
empty page whenever it cannot vouch for its coverage of the book. Two
operational conditions cause this, both distinct from ordinary indexer
backlog:

- **An unprojected message under the book's address.** The gate treats any
  `raw_events` row for that book's `src_address` with `processed_at IS NULL`
  as incomplete coverage. Most of the time this is ordinary projection
  backlog and clears on its own as the projection loop catches up — no
  operator action needed. It does **not** clear when the row is stored
  undecoded (`event_type` / `decoded` both NULL): `persist_page` never
  re-decodes a stored row and projection permanently skips it. Because
  every event an `InferenceOrderBook` contract can emit is already in the
  indexer's loaded ABI, a permanently-undecoded row under a book's
  `src_address` can only mean the deployed contract emitted something the
  loaded ABI doesn't recognize — i.e. the ABI is stale. Monitor with:

  ```sql
  select src_address, count(*) from raw_events where processed_at is null group by 1;
  ```

  A count that persists across repeated checks (rather than draining) points
  at the undecoded case; corroborate with the exported `indexer_decode_errors`
  / `indexer_decode_ambiguous_collisions` OTLP counters. Recovery: sync the
  ABI to the deployed contract version, then stop → wipe → redeploy per
  [Rollout order](#rollout-order-stop-the-stack-then-wipe-then-deploy) above
  — the same procedure the schema fold already requires.
- **A stale capture cursor.** The gate also requires the capture loop's last
  poll to be recent — within `CAPTURE_FRESHNESS_SECS` (30s) of the request —
  because `at_head` alone only records that the *last* poll saw no next
  page, not that the loop is still polling. A wedged or crashed capture loop
  therefore turns every book's TokenContract-filtered live-SELL queries into
  a 503 once the last poll ages past 30s, independent of whether the loop is
  actually behind on data.
