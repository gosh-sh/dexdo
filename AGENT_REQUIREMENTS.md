# Repository Change Agent Requirements

These rules apply to any AI agent that makes changes in this repository.

## Avoid perfectionism

Only P1/P2 issues.

Match the scope of changes to what was actually requested. Do not, without explicit ask:

- refactor code that already works for a hypothetical readability win;
- add error handling for cases that cannot happen (validated upstream, framework-guaranteed, etc.);
- introduce new abstractions to "make this extensible later";
- rewrite comments that aren't actually wrong;
- add tests for code paths already covered by existing tests.

When in doubt about whether a change is in scope, ask. A 50-line PR that solves the asked problem cleanly beats a 500-line PR that "also fixes a few things along the way".

## Project Documentation Rules

Repository specifications live under [`docs/`](docs/), with implementation technical specs under [`docs/tech-specs/`](docs/tech-specs/).

The public REST API contract is [`docs/api-spec.md`](docs/api-spec.md); do not edit it unless the task explicitly asks to change the public API.

Its machine-readable counterpart [`docs/openapi.yaml`](docs/openapi.yaml) is **generated, never hand-edited**. Whenever a change adds or alters a public route, a query/body parameter, or a response DTO — anything registered in the Salvo OpenAPI document — regenerate it with `cargo run -p dodex-api --bin gen-openapi` and stage the result in the same commit.

README files are entry points only: keep a short service definition, links to canonical specs, config locations/variables, and maintenance commands such as run/test/deploy. Do not put implementation details in README files. Functional requirements belong in `docs/api-spec.md`; implementation details belong in `docs/tech-specs/` (`read-api.md`, `write-api.md`, `indexer.md`, `auth.md`); schema details belong in `docs/tech-specs/data-schema.md`.

## Changelog

Every branch opened as a pull request into `dev` must describe its diff against `dev` in [`CHANGELOG.md`](CHANGELOG.md). No PR is complete without it.

Entries are **date-based, newest first** — add yours under today's date, creating the `## [YYYY-MM-DD]` heading if it is not there yet, grouped under `Breaking Changes` / `Added` / `Changed` / `Fixed` / `Removed`. Dates, not release numbers, are the unit here precisely because nobody knows at merge time which release the change ships in. Do not invent a version heading and do not bump `version` in any `Cargo.toml` — if a release number is ever assigned, a human does it at release time. Entries under a past date are history: do not rewrite them or append to them.

### Write for the reader, not for the author

The reader is a devops engineer or a developer who runs DEX.DO and integrates with it. They did not write the code and will not read it. Describe the surface they can observe, briefly — a few lines, not a commit dump:

- **REST API**: routes, query/body parameters, response DTOs, pagination and filter semantics, error codes. (The contract itself lives in [`docs/api-spec.md`](docs/api-spec.md); the changelog says what moved.)
- **On-chain behaviour**: contract entrypoints, events and their external ids, ABI changes, renamed getters/errors, code-hash re-pins.
- **Indexer behaviour**: what is ingested and what is not, projectors, cursors, backfill and reconciliation semantics.
- **Storage**: Postgres schema, indexes, migrations — and whether a migration has to be run.
- **Operations**: config files and environment variables, `docker-compose*.yml`, `deploy/`, `Makefile` targets, exported metrics, dashboards and alerts an operator would page on.
- **SDK**: the public surface under `sdk/`.

Say what changed and what the reader has to do about it — run a migration, carry a setting over by hand, switch to a new field, redeploy a contract, re-pin a code hash. Name routes, fields, events, tables and options exactly as they appear in the product.

Leave out internal refactors, private renames, test-only changes and implementation detail. If nothing observable changed, there is nothing to write.

## Before every `git commit`

Re-read **every** file under `docs/`, the root [`README.md`](README.md), and the `README.md` of every touched component, then update each one the staged diff invalidates. Default is "check all"; only skip a doc after re-reading it and confirming it is unaffected.

The root `README.md` is the project's high-level entry point — keep its architecture overview, repository layout, configuration / running-locally / test instructions, and the "Documentation" section in sync with the actual code, doc structure, and config files. Stale links or removed files referenced from the README are bugs.

This includes terminology renames (e.g. `OEL` -> `OracleEventList`) and schema/field shape changes - propagate them across all docs, not just the file whose name matches the code change.

If a doc is now obsolete and has no salvageable content, delete it and remove references in the same commit.

Before running the full test suite or DB-backed integration tests, start the disposable test Postgres as described in [`README.md#test-postgres`](README.md#test-postgres).

## Metrics, dashboards, and alerts

The Grafana artifacts under `deploy/grafana/` live outside `docs/`, so the "before every `git commit`" doc sweep does **not** cover them — they must be updated explicitly. When a change adds, renames, or removes an exported metric (in `crates/metrics/`, wired through `services/indexer/`):

- Add or adjust its panel in the dashboard [`deploy/grafana/dodex-indexer-dashboard.json`](deploy/grafana/dodex-indexer-dashboard.json), mirroring the existing panel for the same metric family, in the same commit. The dashboard is JSON — validate it parses before committing.
- If the metric is an error or health signal an operator would page on, add or adjust a rule in [`deploy/grafana/provisioning/alerting/dodex-indexer-alerts.yaml`](deploy/grafana/provisioning/alerting/dodex-indexer-alerts.yaml). A purely informational metric may have a panel and no alert.
- Keep the metric documented in the catalog in `docs/tech-specs/indexer.md` (already required by the doc rules above).
