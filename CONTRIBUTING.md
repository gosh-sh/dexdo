# Contributing to DEX.DO

This is the map for **where code and docs go** in this repository, and the rules
for getting a change merged. It is written for **both humans and AI agents** — an
agent with shell access should be able to follow it without guessing.

Two companion documents, referenced rather than duplicated here:

- [`AGENT_REQUIREMENTS.md`](AGENT_REQUIREMENTS.md) — scope discipline and the
  **mandatory pre-commit documentation sweep**. Read it before committing.
- [`docs/README.md`](docs/README.md) — the documentation map and file ownership.

If a rule here and a rule there ever disagree, the more specific one wins; open a
PR fixing the conflict.

## The three kinds of code

Almost everything you add is one of three things. Naming the category is the whole
point of this guide — pick the right one and the rest follows.

| Kind | What it is | Where it lives | In root `cargo build --workspace`? |
| --- | --- | --- | --- |
| **lib** | Reusable code other code imports | [`crates/*`](crates/), [`sdk/`](sdk/) | `crates/*` yes; `sdk/` no (own workspace) |
| **service** | A long-running daemon | [`services/*`](services/) | yes |
| **tool** | A one-shot CLI an end user runs | [`tools/`](tools/) | no (own workspace) |

Plus two non-Rust homes: on-chain contracts in [`contracts/`](contracts/) and
specs in [`docs/`](docs/) (covered below).

## Where code goes

**Reusable code always goes into a lib** — the only question is *which* lib home.
There are two, and both hold libraries other code imports:

- **`crates/*`** — reusable code for the **backend**: the `api` + `indexer`
  services and the Postgres read-model they share. Light dependency graph, lives
  in the root workspace. Internally layered (steps 1–5 below).
- **[`sdk/`](sdk/)** — the reusable **client / write-side library** (`dodex-sdk`):
  the `Dex` facade, DTOs, and the halo2 voucher/proof pipeline that **tools, WASM
  builds, and external integrators** import to drive the chain. Its zk/halo2 graph
  is heavy, so it is its **own workspace**, excluded from the root resolve.
  Its `tests/integration` e2e harness also takes a `dev-dependency` on
  [`crates/infrastructure`](crates/infrastructure) (chain-account BOC/ECC/storage
  decoding). This is dev-only and confined to the test harness — distinct from
  `sdk/`'s existing production dependency on
  [`crates/contracts`](crates/contracts) (the on-chain ABI/wrapper layer), which
  predates this edge and is the only `crates/*` dependency shipped in the
  library itself.

So the first cut is *backend or client/write-side?* — then, within the backend,
the layering decides the crate. Decision order, stop at the first match:

1. **Is it pure domain data or a domain rule, with no I/O?**
   → [`crates/domain`](crates/domain). No database, no network, no chain calls —
   just types, invariants, and errors. `domain` depends on nothing else in-tree.

2. **Is it a use case / orchestration — "do X, then Y, persist Z"?**
   → [`crates/application`](crates/application). Application **owns the port
   traits** (`MarketReadRepository`, `ChainOrderSender`, `AccountRegistry`, …) and
   programs against them. It depends only on `domain`, never on `infrastructure`.

3. **Is it an adapter to the outside world** — Postgres, the TVM runner, the
   GraphQL gateway, crypto, config loading?
   → [`crates/infrastructure`](crates/infrastructure). This is where the port
   traits from `application` get **implemented**.

4. **Does it wrap the on-chain contract handles (`ackinacki_kit`)?**
   → [`crates/chain`](crates/chain) — the write/read facade over the kit.

5. **Is it a tiny cross-cutting concern several crates need** (logging, metrics)?
   → a small standalone crate like [`crates/logging`](crates/logging) or
   [`crates/metrics`](crates/metrics). Keep its dependency footprint minimal so
   crates that don't need the concern don't pay for it.

6. **Is it client / write-side reusable code** — anything tools, WASM, or external
   integrators import to drive the chain (the `Dex` facade, DTOs, the proof /
   halo2 voucher pipeline)?
   → [`sdk/`](sdk/), the `dodex-sdk` library. Reusable code belongs here just as
   much as in `crates/*` — this is simply the **client-side** lib home. It is a
   **separate workspace** because its halo2 graph is heavy and must stay out of the
   root resolve.

7. **Is it a long-running process?** → a new crate under [`services/`](services/),
   added to the root workspace `members`. See *Adding a service*.

8. **Is it a CLI a user runs once to get a result?** → [`tools/`](tools/). See
   *Adding a tool* and [`tools/README.md`](tools/README.md).

### The reuse rule

> If a **second** consumer (another service, another tool, a test) would copy a
> chunk of code, that chunk belongs in a lib — lift it into the right `crates/*`
> or `sdk/` and have both callers import it.

Services and tools are **thin wiring** over the libraries. Business logic that
lives in a `main.rs` is a smell; push it down into `application` (orchestration) or
`domain` (rules) where it can be unit-tested and reused.

### The dependency rule (ports and adapters)

The root crates form a one-directional dependency chain. Do not violate it:

```
domain  ←  application  ←  infrastructure
                 ↑                ↑
                 └──── services / tools wire concrete adapters into use cases
```

- `domain` imports nothing in-tree and performs no I/O.
- `application` imports only `domain`; it defines `trait`s (ports) for everything
  external and never names a concrete adapter.
- `infrastructure` imports `application` + `domain` and implements the ports.
- `services` and `tools` are the **composition root**: they pick concrete adapters
  and inject them into use cases. Nothing depends back "up" the chain.

## Adding a service

1. New crate under `services/<name>/`, added to `members` in the root
   [`Cargo.toml`](Cargo.toml).
2. Config file(s) at `config/<name>.<env>.yaml`; document the env vars and config
   path in the service's `README.md` (entry-point only — see *Documentation*).
3. Wire use cases from `application` to adapters from `infrastructure`; keep the
   binary thin.
4. If it ingests chain events or serves the public API, update the matching
   tech-spec ([`docs/tech-specs/indexer.md`](docs/tech-specs/indexer.md),
   [`read-api.md`](docs/tech-specs/read-api.md),
   [`write-api.md`](docs/tech-specs/write-api.md)).

## Adding a tool

Full instructions live in [`tools/README.md`](tools/README.md). In short: `tools/`
is its **own workspace**, each tool is a crate under `tools/<name>/` that depends
on the libs **by path**, the binary is named `<verb>_<noun>`, and reusable logic
goes into a lib rather than the tool. A tool needing only the lightweight root
crates may instead be a root-workspace `[[bin]]` — the rule keys off the
dependency graph.

The four end-user binaries still in [`sdk/src/bin/`](sdk/src/bin/) are
grandfathered; they move to `tools/` only when next touched.

## Tests

Match the test kind to what you're testing — and keep the heavy ones opt-in so the
default `cargo test` stays fast.

| Kind | Where | Needs | Notes |
| --- | --- | --- | --- |
| **Unit** | inline `#[cfg(test)]` next to the code | nothing | Pure logic in `domain`/`application`. No DB, no network. |
| **Integration (DB)** | `<crate>/tests/*.rs` | test Postgres | Adapter + use-case behavior against a real schema. The bulk of `infrastructure` and `api` coverage. |
| **HTTP** | `services/api/tests/*_http.rs` | test Postgres | Exercise routes end to end through the service. |
| **e2e (chain)** | `*e2e_*` tests, `#[ignore]`-marked | live Shellnet | Real on-chain flow. Skipped by default; CI runs them in a gated job. |
| **SDK integration** | `sdk/tests/integration/` | live Shellnet | One modular test binary (`main.rs` + per-topic modules + `common/` helpers). |

Rules of thumb:

- **Test the library, not the wiring.** Push logic into `domain`/`application` and
  unit-test it there; `services`/`tools` `main.rs` should have little worth
  testing.
- **Don't add a test for a path already covered** (see
  [`AGENT_REQUIREMENTS.md`](AGENT_REQUIREMENTS.md#avoid-perfectionism)).
- **Mock at the HTTP boundary, not above it.** When code makes an outbound HTTP
  call (the GraphQL chain gateway, the TVM node, the giver), the test points the
  **real** client at a local mock HTTP server and asserts on the canned response
  *and* the request the client sent. Don't stub out your own adapter or port to
  fake the call away — that skips the serialization/deserialization and error
  mapping that are the very thing worth testing, and it lets the wire format drift
  unnoticed. Keep the mock dependency-free: a bare `std::net::TcpListener` loop,
  not a new `wiremock`/`mockito` dev-dep. The canonical example is
  [`crates/infrastructure/tests/account_boc.rs`](crates/infrastructure/tests/account_boc.rs)
  (`spawn_mock` → `GraphqlClient` → assert on response + captured request);
  copy its shape. This keeps the suite fast and deterministic — live endpoints
  belong only in the `#[ignore]`d e2e / SDK tests.
- DB-backed tests run their own migrations on connect — bring up the disposable
  Postgres first, per [`README.md#test-postgres`](README.md#test-postgres).
- New on-chain flows get coverage in the SDK integration binary, not a new
  top-level harness.

### What CI gates (run these locally before pushing)

From [`.github/workflows/pr-tests.yml`](.github/workflows/pr-tests.yml):

```sh
cargo +nightly fmt --all --check                                  # rustfmt.toml uses nightly features
cargo clippy --workspace --all-targets --no-deps -- -D warnings   # warnings are errors
cargo nextest run --workspace                                     # unit + DB integration (test Postgres up)
cargo test --workspace --doc                                      # doctests (nextest skips these)
```

If you touched the public API, also regenerate and commit the OpenAPI contract —
the **openapi-drift** job fails otherwise:

```sh
cargo run -p dodex-api --bin gen-openapi -- --out docs/openapi.yaml
```

`sdk/` and `tools/` are separate workspaces — build and test them from their own
directory (`cargo build` / `cargo test` inside `sdk/`, inside `tools/`).

### Make sure CI actually runs your new test

Adding a test file is not enough — CI only runs tests its jobs reach. Check which
case you're in:

- **Unit / DB-integration test in an existing root-workspace crate** (`crates/*`,
  `services/*`) → **nothing to wire.** `cargo nextest run --workspace` and the
  doctest step auto-discover it. This is the common case.
- **New crate in the root workspace** → it's covered the moment it's in `members`
  in the root [`Cargo.toml`](Cargo.toml) (see *Adding a service*). No new crate,
  no coverage.
- **A `#[ignore]`d e2e (chain) test** → the gated job runs **only `dodex-api`**
  (`cargo nextest run -p dodex-api --run-ignored only --test-threads 1`). Put the
  test in `services/api/tests/` as `e2e_*`, or it never runs. If it needs new
  fixtures/secrets (e.g. seed notes), wire them into
  [`.github/workflows/e2e-shellnet.yml`](.github/workflows/e2e-shellnet.yml). New
  on-chain flows are single-threaded and spend test tokens — keep them `#[ignore]`d
  so they stay out of the fast PR job.
- **Any test in a separate workspace (`sdk/`, `tools/`)** → **no CI job runs these
  today** — `--workspace` excludes them, and `sdk/`'s only tests are the live,
  `#[ignore]`d Shellnet integration suite. If you add a test there that *should*
  gate PRs (hermetic, no network — e.g. an HTTP-boundary-mocked test), you must add
  a CI step that runs it from that workspace's directory. A test that doesn't run
  in CI is documentation, not a gate — say so in the PR if you leave it manual.

## Documentation

Where each kind of documentation lives — keep them from drifting into each other:

| Content | Home |
| --- | --- |
| Public REST API contract (**single source of truth**) | [`docs/api-spec.md`](docs/api-spec.md) — do not edit without explicit approval from the contract owner |
| Generated OpenAPI | [`docs/openapi.yaml`](docs/openapi.yaml) — regenerated from the Rust handlers, never hand-edited |
| Implementation specs (one logical component → one file) | [`docs/tech-specs/`](docs/tech-specs/) |
| Postgres schema / indexes / migrations | [`docs/tech-specs/data-schema.md`](docs/tech-specs/data-schema.md) |
| On-chain contract behavior & event routing | [`docs/contract-specs/`](docs/contract-specs/) |
| Operations / self-hosting | [`docs/deployment.md`](docs/deployment.md) |
| User-facing change log (date-based, newest first) | [`CHANGELOG.md`](CHANGELOG.md) |

README files — root and per-component — are **entry points only**: a short
definition, links to the canonical specs, config locations/variables, and
run/test/deploy commands. **No implementation details in a README** — those belong
in `docs/tech-specs/`. This is enforced by
[`AGENT_REQUIREMENTS.md`](AGENT_REQUIREMENTS.md#project-documentation-rules).

A new tool or service ships with its own `README.md` (entry point) and, if it has
non-trivial behavior, an entry in the right tech-spec or contract-spec — not a
prose dump in the README.

### The pre-commit doc sweep is mandatory

Before **every** commit, re-read every file under `docs/`, the root `README.md`,
and the `README.md` of every touched component, and update whatever the diff
invalidates — including terminology renames and schema shape changes. The full
rule (and the rationale) is in
[`AGENT_REQUIREMENTS.md`](AGENT_REQUIREMENTS.md#before-every-git-commit). This is
not optional and not narrowable to "the obviously relevant doc."

## Opening a pull request

1. Branch off `dev` (the default integration branch); PRs target `dev`.
2. Keep the change **scoped to what was asked** — a focused PR beats a sprawling
   one ([`AGENT_REQUIREMENTS.md`](AGENT_REQUIREMENTS.md#avoid-perfectionism)).
3. Green locally: fmt, clippy, tests, doctests, and OpenAPI drift (above).
4. Add a [`CHANGELOG.md`](CHANGELOG.md) entry under today's date if the change is
   user-visible.
5. Run the doc sweep and commit doc updates **in the same commit** as the code.
