# DEX.DO

Backend for DEX.DO — a decentralized exchange on the Acki Nacki chain. Two Rust services share a Postgres read-model:

- `services/api` — HTTP service serving the public REST API.
- `services/indexer` — chain-event ingestor that builds the Postgres read-model.

On-chain contracts live under `contracts/`: the DEX.DO core in `contracts/dex/`, and the AI-inference registry it integrates with in `contracts/airegistry/`.

## Onboarding (Shellnet)

DEX.DO is **agent-driven**: an AI coding agent with shell access to this repo runs the whole flow — onboarding, registration, market data, trading, and withdrawal — from one-line prompts, one per skill (see the [Quickstart](#quickstart--prompts-by-skill) below).

**Prerequisites**

- An AI agent with shell + repo access (Claude Code, Cursor, …).
- macOS or Linux with `git`, `curl`, `jq` — the skill installs Rust (`cargo`) and `tvm-cli` itself if they're missing.
- Network access to Shellnet (`shellnet.ackinacki.org`) and its public giver.

## Quickstart — prompts by skill

The whole lifecycle is driven by pasting one-line prompts to the agent. Each step maps
to one skill under [`.claude/skills/`](.claude/skills/); run them top to bottom (skip
any you've already done). Copy a prompt, fill in the `<…>` bits, paste.

| # | Skill | Paste this prompt |
|---|---|---|
| 1 | [`dexdo-deploy-multisig-shellnet`](.claude/skills/dexdo-deploy-multisig-shellnet/SKILL.md) — deploy + fund a multisig | > Deploy and fund a multisig wallet for me on DEXDO Shellnet. |
| 2 | [`dexdo-deposit-shellnet`](.claude/skills/dexdo-deposit-shellnet/SKILL.md) — fund the wallet + create PrivateNotes | > Deposit onto DEXDO Shellnet using my multisig (here is the seed phrase / keypair) — create three gas-funded PrivateNotes ready to trade. |
| 3 | [`dexdo-register-account`](.claude/skills/dexdo-register-account/SKILL.md) — get API credentials (delegates the note key — opt-in) | > Register my PrivateNotes with the DEXDO API and save the credentials. |
| 4 | [`dexdo-market-data`](.claude/skills/dexdo-market-data/SKILL.md) — read markets / book / price / my orders / my stakes | > Show me the active DEXDO markets I can stake or trade. <br> > Show the order book and price for `<symbol>`. <br> > Show my open orders / order history / stakes. |
| 5 | [`dexdo-trading`](.claude/skills/dexdo-trading/SKILL.md) — stake / place orders / full split | > Stake `<amount>` NACKL on `<outcome>` in `<market>`. <br> > Place a limit `<BUY/SELL>` of `<qty>` `<symbol>` at `<price>`. <br> > Buy a full set in `<market>` for `<amount>` NACKL. |
| 6 | [`dexdo-total-withdraw`](.claude/skills/dexdo-total-withdraw/SKILL.md) — close everything + sweep to multisig | > Close all my DEXDO positions and withdraw everything back to my multisig. |

Notes:

- **Already have a multisig?** Skip step 1 — step 2 takes it as input (seed phrase or keypair).
- **Steps 1–2 are on-chain only**; the note isn't usable via the API until **step 3** (registration is a deliberate security step — it delegates the note's key to the backend; see the warning below).
- **Staking & the order book live in different market phases** — the trading skill (step 5) routes STAKING-phase stakes through the on-chain SDK and TRADING-phase orders through the REST API automatically.
- **Before a network restart**, run step 6 to get funds back to the multisig (rewards not withdrawn in time can be lost).

**Output of steps 1–2** — a multisig wallet you control and three funded PrivateNotes (SHELL / NACKL / USDC).

Everything is written under the workspace directory **`$WORKSPACE`** (default `~/dexdo-workspace`; export `WORKSPACE=/your/path` before running to change it):

```
$WORKSPACE/
├── multisig/   wallet — Multisig.keys.json, Multisig.seed (SECRET) + Multisig.abi.json, Multisig.tvc
├── notes/      per-PrivateNote files, token ∈ {shell, nackl, usdc} (SECRET, mode 0600):
│                 <token>.account.json  — ready POST /api/v1/accounts body
│                                         (pnAddress / pnPubkeyHex / pnSeckeyHex / pnDihHex)
│                 pn_state.<token>.json — onboarding resume state, holds the note key (not API-loadable)
│                 <token>.creds.json    — registration credential from POST /api/v1/accounts
│                                         (apiKey + apiSecret); written only after you register the note
└── giver/      GiverV3.abi.json — Shellnet funding artifact
```

The `notes/` files hold two kinds of secret used by the trading / CLI skills:

- **the note's on-chain key** (`pnSeckeyHex`, in `account.json` / `pn_state.json`) — signs on-chain ops (stake, place-order, withdraw) directly;
- **the backend API credential** (`apiKey` + `apiSecret`, in `creds.json`) — obtained when you register the note (`POST /api/v1/accounts`); REST calls are then signed `HMAC-SHA256(request, apiSecret)` under the `X-DODEX-APIKEY` header, and the `apiSecret` is returned **only once** at registration.

All of these are written **mode 0600** and stored in plaintext under `$WORKSPACE` — back them up, **never commit them**, and prefer an OS keychain over the workspace on an untrusted/multi-user host.

Use the PNs two ways:

- **directly with the SDK libraries** (`dodex-sdk` / `ackinacki-kit`) — sign on-chain operations with each note's key from its file;
- **loaded into a running API service** — POST each `notes/<token>.account.json` to `/api/v1/accounts`.

🚨 **Important:**
Loading a PN into any API service delegates that PrivateNote's private key to the backend. 

The hosted testing backend (`https://dodex-dev.ackinacki.org`) is for Shellnet testing only, **provides no security guarantees for delegated PN keys**, and will not be offered on Mainnet.

Files under `multisig/` and `notes/` carry secret keys — back them up, never commit them.

## Documentation

- [docs/api-spec.md](docs/api-spec.md) — public REST API contract.
- [docs/openapi.yaml](docs/openapi.yaml) — OpenAPI 3.1 contract, generated from the Rust handlers. See [openapi/README.md](openapi/README.md) for the regen workflow and the GitHub Pages deployment.
- [docs/README.md](docs/README.md) — documentation map and file ownership.
- [docs/shellnet-testing.md](docs/shellnet-testing.md) — tester guide for trying DEX.DO on Shellnet with the hosted testing backend or a self-hosted backend.
- [docs/deployment.md](docs/deployment.md) — self-hosting the `indexer` and `api` on your own server, wired to your own Acki Nacki GraphQL endpoint and your own Postgres / Supabase.
- [CONTRIBUTING.md](CONTRIBUTING.md) — where code and docs go (libs / services / tools), test and documentation rules, PR checklist. For humans and agents.
- [AGENT_REQUIREMENTS.md](AGENT_REQUIREMENTS.md) — rules for any agent making repository changes.

## Repository layout

```
crates/
  domain/            # domain types
  application/       # use cases
  infrastructure/    # adapters (Postgres, TVM runner, GraphQL gateway)
  contracts/         # dodex-contracts: Rust wrappers for the on-chain contracts,
                     # built on ackinacki-kit traits; ABIs read from contracts/<group>/
services/
  api/               # REST API service
  indexer/           # chain-event indexer
contracts/           # on-chain contracts (TVM), .sol + .abi.json side by side
  dex/               #   DEX.DO core: PrivateNote, OrderBook, PMP, Oracle, RootPN, Nullifier
  airegistry/        #   AI-inference registry (integrates with dex): SuperRoot/RootModel,
                     #   ManifestMetadata, TokenContract, InferenceOrderBook, InferenceOracle
sdk/                 # dodex-sdk: write-side DEX facade + halo2 voucher pipeline
                     # (separate workspace, excluded from the root build)
tools/               # end-user CLIs (separate workspace; see tools/README.md)
docs/                # specs and plans (see docs/README.md)
migrations/          # SQL migrations applied by sqlx::migrate! at startup
config/              # service config files (api.<env>.yaml, indexer.<env>.yaml)
deploy/              # deployment tooling
scripts/             # operational scripts
tests/               # repo-level integration fixtures (REST .rest files, e2e)
```

`sdk/` is its own Cargo workspace and is **not** part of `cargo build --workspace` — its halo2 proof pipeline pulls a heavy, distinct zk/halo2 dependency graph. Build it from `sdk/` directly (`cargo build` there).

## Configuration

Per-service config files live under `config/`:

- `config/api.<env>.yaml` — consumed by `services/api`.
- `config/indexer.<env>.yaml` — consumed by `services/indexer`.

Local defaults: `config/api.local.yaml`, `config/indexer.local.yaml`. Override at runtime with `APP_CONFIG=/path/to/file.yaml`.

Notable indexer config keys under `indexer:`:

- `ignored_addresses` — source addresses dropped before `raw_events` insert and projection.
- `dapp_id` — scopes ingestion to the DEXDO dapp; foreign events dropped before decode. See [docs/tech-specs/indexer.md](docs/tech-specs/indexer.md#scope-filter-indexerdapp_id).
- `ignored_event_types` — event types dropped before decode, matched by `dst`. See [docs/tech-specs/indexer.md](docs/tech-specs/indexer.md#no-op-filter-indexerignored_event_types).

Logging is environment-driven: `RUST_LOG` sets verbosity, and `LOG_DIR`
(optional) makes each service also write rotated log files into a directory —
the Compose deployment bind-mounts these to `./logs/<service>`. See
[docs/deployment.md](docs/deployment.md#logs) and
[docs/tech-specs/indexer.md](docs/tech-specs/indexer.md#noise-log).

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
