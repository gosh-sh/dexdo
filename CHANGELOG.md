# Changelog

All notable changes to DODEX are recorded here. Entries are date-based, newest first.

## [2026-05-29]

### Added

- `sdk/`: new `dodex-sdk` crate — the write-side DEX facade over `ackinacki-kit` (private notes, order book, PMP, oracle/market) plus the halo2 voucher proof pipeline. Kept as its own workspace and `exclude`d from the root build, since the halo2 pipeline pulls private SSH-only git sources that CI hosts have no key for; build it directly from `sdk/`.

## [2026-05-27]

### Added

- `services/market-manager/`: new market-manager service with Dockerfile, stage configs, and event-list seed (`config/events.stage.json`) — wires DEX market lifecycle off-chain.
- `openapi/openapi.yaml` and `openapi/index.html`: published OpenAPI spec rendered on GitHub Pages. Added `services/api/src/bin/gen-openapi.rs` generator binary, `openapi/generate.sh`, and `.github/workflows/{openapi,pages}.yml` to regenerate and deploy on push.
- `crates/chain/`: new chain-client crate carved out of `infrastructure/chain_sender.rs` (client, DTO, error, test helpers).
- `makerComission` and `takerComission` fields on the `GET /api/v1/markets` response. Signed `DECIMAL` strings (e.g. `"0.00045"`); a negative `makerComission` is a maker rebate credited rather than debited.

### Changed

- Trimmed `services/market-manager/Cargo.toml` and `Dockerfile` to fix the stage image build (#32).
- Regenerated OpenAPI after BE-DEX method sync; moved `openapi/index.html` → `docs/index.html` for Pages serving.

## [2026-05-26]

### Added

- `GET /api/v1/account` balances API (#27): collateral + outcome-token balances, plus `-2013` error for accounts with no deployed PrivateNote contract. Adds `pn_state_reader`, `tvm_hash` helpers, and the `PrivateNote.sol` source.
- `DELETE /api/v1/batchOrders` cancel-batch endpoint (#26): atomic batch cancellation by `orderIds`, capped at 5 per request. Adds `tests/cancel_batch_orders_http.rs` and `tests/resolve_for_cancel.rs` coverage.

### Changed

- `docs/api-spec.md`: documented `GET /api/v1/account` outcome balances and `DELETE /api/v1/batchOrders` response shape; aligned cancel errors with new `-2013`.
- `AGENT_REQUIREMENTS.md`: strengthened the pre-commit spec-sweep rule (re-read every doc under `docs/` and touched READMEs — no narrowing of "relevant").

## [2026-05-22]

### Added

- `POST /api/v1/batchOrders` (#25): atomic batch order creation, per-outcome `maxBatchSize`, intra-batch dedup of `newOrderClientId`. New tests: `create_batch_orders_http.rs`, `e2e_batch_orders.rs`.
- `GET /api/v1/orders` all-orders endpoint (#23): paginates orders across all markets for the authenticated account, sorted by stable chain-order key (descending). Added `tests/orders.rs` (replaces the older open-orders coverage) and `docs/migrations/orders-cancel-remainder-cutover.md`.

### Changed

- Renamed `openOrders` filter behavior and trimmed stale `MARKET` TIF claims in `docs/api-spec.md`.
- `docs/tech-specs/{read-api,data-schema,indexer}.md`: updated to match the all-orders projection and new cursor semantics.

## [2026-05-20]

### Added

- `DELETE /api/v1/order` cancel-order endpoint (#24): single-order cancel with `PENDING_CANCEL` intermediate status; ephemeral-market testkit under `services/api/tests/common/` (`deploy_market.rs`, `e2e_setup.rs`, `test_pns.rs`) and new `e2e_cancel_order.rs`.

### Changed

- `docs/api-spec.md` and `docs/tech-specs/write-api.md`: documented cancel acceptance/finality semantics.

## [2026-05-19]

### Changed

- `contracts/`: regenerated `OracleEventList.sol`, `OrderBook.sol`, `PMP.sol`, `PrivateNote.sol`, `RootPN.sol`, `modifiers/{errors,modifiers}.sol` from the latest DEX source (#22).

## [2026-05-18]

### Added

- `POST /api/v1/order` create-order endpoint (#21) with `PENDING_NEW` acceptance semantics, fail-closed `AppState` hoop, and quantity-validation tightening. Added `tests/resolve_for_new_order.rs`.

### Changed

- `docs/api-spec.md` and `docs/tech-specs/write-api.md`: documented order placement, `MARKET` buy semantics on `quoteAsset`, and `request_timeout > chain timeout` invariant.
- Renamed write-side spec file; restored `api-spec.md` after rebase churn.

## [2026-05-15]

### Added

- Open-orders pipeline behind `GET /api/v1/openOrders` (#20): projectors, postgres repo extensions, and `tests/open_orders.rs` (later subsumed by the all-orders endpoint).
- `migrations/0001_initial.sql` replacing `0001_init_read_model.sql` — initial read-model schema reset.

### Changed

- `docs/tech-specs/{data-schema,indexer,read-api}.md`: documented open-orders projection and schema.

## [2026-05-12]

### Added

- `POST /api/v1/order` mocked implementation with auth/permission wiring (#15) and `docs/tech-specs/auth.md` end-to-end smoke coverage.
- `GET /api/v1/depth` order-book endpoint (#11) — initial DEX read API skeleton, ABI bundle under `contracts/abi/dex/`, root `Cargo.toml` workspace, `config/{api,indexer}.local.yaml`, and `LICENSE.md`.
- `docs/README.md` — documentation map with file ownership.
- `CHANGELOG.md` (this file).

### Changed

- Restructured `docs/tech-specs/` (#18):
  - `market-data-api.md` → `read-api.md` (scope: all read endpoints).
  - `market-data-indexer.md` → `indexer.md`.
  - `trading-api/write-api.md` → `write-api.md` (top-level).
- Updated cross-references in `services/api/README.md`, `services/indexer/README.md`, `AGENT_REQUIREMENTS.md`, and inline test comments.
- Rewrote root `README.md` for DODEX (previous content was carried over from a different project).
- Centralized permission check and split authN/authZ in `docs/tech-specs/auth.md` to match code (#16, #17).

### Removed

- `docs/tech-specs/trading-api/` directory (empty `read-api.md` removed; `write-api.md` promoted to top-level).
- `internal-docs/` directory (was gitignored; local-only).
- `AGENTS.md` legacy stub.

## [2026-05-04]

### Changed

- `docs/api-spec.md`: extended response payloads with UI-facing fields; added `docs/tech-spec.md`; removed `docs/technical-spec-market-data.md` (#10).

## [2026-04-27]

### Changed

- `docs/api-spec.md` rewritten and aligned with `docs/dex-events-routing.md` (#8); added rendered HTML diagrams `dex-contracts-external-flows.html` and `dex-contracts-system.html`.

## [2026-04-23]

### Changed

- `contracts/`: synced to the latest DEX contracts (#7) — `Nullifier.sol`, `Oracle.sol`, `OracleEventList.sol`, `OrderBook.sol`, `PMP.sol`, `PrivateNote.sol`, `RootOracle.sol`, `RootPN.sol`, `libraries/DexLib.sol`, `modifiers/{errors,modifiers,replayprotection}.sol`.

## [2026-04-20]

### Added

- `docs/GRAPHQL.md` — GraphQL gateway notes (#4).
- `docs/dex-events-routing.md` — event-routing reference.

### Changed

- Updated `docs/dex-contracts-object-diagram.drawio`.

## [2026-04-17]

### Added

- Initial Solidity contracts under `contracts/` (#2): `Nullifier`, `Oracle`, `OracleEventList`, `OrderBook`, `PMP`, `PrivateNote`, `RootOracle`, `RootPN`, plus `libraries/DexLib.sol` and shared `modifiers/`.

### Changed

- Reworked `docs/api-spec.md` to match contract shapes.

## [2026-04-15]

### Added

- `docs/api-spec.md` — first draft of the REST API specification.

## [2026-02-05]

### Added

- Initial commit.
