# Deploying DEX.DO with Ansible

An Ansible role + playbook to deploy the `api` + `indexer` services to your own
host. It checks the source out on the target host and builds the images there
with `docker compose up --build` — nothing is pulled from a registry. For the
manual / from-scratch Compose path and the meaning of every config field, see
[`docs/deployment.md`](../../docs/deployment.md).

## How it works

```
deploy-dexdo.yml → role dexdo   on the host: git checkout <ref> → render config +
                                compose → docker compose up --build
```

Ansible checks the requested ref out on the host and `docker compose --build`
compiles the images from it, so the Rust + halo2 build runs **on the host**.

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
  defaults/main.yml               non-secret vars (env, repo/version, endpoints, knobs) — override per env in inventory
  tasks/main.yml                  dirs, git checkout, render templates, docker compose up --build
  handlers/main.yml               restart stack on config change
  templates/                      env-agnostic — env name comes from {{ dexdo_env }}
    compose.yml.j2                build-based compose (build context = the host checkout) -> <deploy_dir>/compose.yml
    api.yaml.j2                   rendered to <deploy_dir>/config/api.<env>.yaml
    indexer.yaml.j2               rendered to <deploy_dir>/config/indexer.<env>.yaml
```

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
3. **Non-secret vars** — review `roles/dexdo/defaults/main.yml`: `dexdo_repo_url`
   (the checkout source) plus the Acki Nacki endpoints. Override any of these per
   environment in that env's `hosts.yml`.
4. **Host prerequisites** — Docker Engine + Compose plugin, `git`, and an SSH
   user that can `sudo` and run `docker` (the playbook uses `become: true`). The
   host also needs read access to `dexdo_repo_url` (a deploy key, or a token in
   the URL for a private repo) since it clones and builds the source itself.

## Run

From this directory:

```sh
# deploy a specific git ref (default is `dev` if omitted)
ansible-playbook deploy-dexdo.yml -i inventories/<env>/hosts.yml --ask-vault-pass -e dexdo_version=<branch|tag|sha>

# dry run
ansible-playbook deploy-dexdo.yml -i inventories/<env>/hosts.yml --ask-vault-pass --check --diff
```

(Drop `--ask-vault-pass` if the env vault is left unencrypted.)

## Verify

```sh
curl -s http://<host>:<dexdo_api_port>/readiness        # dexdo_api_port default 8080
curl -s 'http://<host>:<dexdo_api_port>/api/v1/markets?limit=5' | jq
```

Market-data endpoints are empty until the indexer has ingested chain events —
normal on a cold database.
