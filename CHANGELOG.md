# Changelog

All notable changes to DEX.DO are recorded here. Entries are date-based, newest first.

## [2026-08-26]

### Breaking Changes

- **A market's model is one field now, not an object.** `/api/v1/inference/markets` replaces `model: { producer, name, version, ref }` with **`modelRefName`** — a string carrying the model's name exactly as its order book reports it, e.g. `"Qwen2.5-32B-Instruct"`, or the model hash when the book has no name. Treat it as an opaque label: no structure is guaranteed and none is parsed. The same rename lands on `resolvesFrom.model` → **`resolvesFrom.modelRefName`** in `/api/v1/prediction/markets`, which already carried this value under the other name.

  The split was a guess at structure the names never guaranteed: it required exactly three `--`-separated parts and produced three nulls for everything else, and the model registry has been re-seeded with names that are not in that shape at all.
- **`?producer=` is gone from `/api/v1/inference/markets`.** It filtered on a field the response no longer carries, backed by a column that would have been NULL for every newly registered model. Callers that used it should filter client-side on `modelRefName`. It is also no longer one of the parameters that conflicts with a single-market `?inferenceOrderBookAddress=` lookup.

- **A deal is deployed from the seller's note now, not by an external message** (contracts 4.0.36). `PrivateNote.deployDeal(nonce, modelName, modelHash, pricePerTick, maxTicks, gasReserve)` replaces the off-chain `TokenContract` deploy. The constructor takes a new `depositIdentifierHash` argument and requires `msg.sender` to be the canonical `PrivateNote` for it, so a seller-key-signed external deploy is refused outright. The deal therefore lives in the note's dApp instead of one of its own, and `PrivateNote`'s constructor takes a new `tokenContractCode` cell. Re-pinned code hashes: `PrivateNote → f7a49601`, `TokenContract → 4378e271` (depth 17 → 18), `RootModel → 87c63e32`, `InferenceOrderBook → 91f0af87`.
- **`RootPN.setTokenContractCode` must run before any note is deployed.** A note minted without it bakes an empty TokenContract code cell, and its `deployDeal` puts a codeless account at a well-formed address. `RootPN.onCodeUpgrade` restores six codes, so every `updateCode` wipes `_inferenceOrderBookCode`, `_tokenContractCode` and the previous-generation note pin: re-run `setInferenceOrderBookCode`, `setTokenContractCode` and `setPrevPrivateNoteCode` after each upgrade.
- **A BUY order now costs the note 0.025 SHELL**, burned as ECC[2] at placement (`PrivateNote.placeInferenceBuy`), mirroring what posting a SELL offer already cost the seller. Taken from the note's own SHELL, not from its recorded balance, so a note holding none cannot place a buy.

### Added

- `RootPN.setPrevPrivateNoteCode` / `getPrevPrivateNoteCode`: the root serves notes of one previous code generation, so a `PrivateNote` upgrade no longer strands the balances of notes already deployed — `withdrawTokens` accepts a note of either generation.
- The indexer ingests the 15 `TokenContract.*` settlement routes (external ids 709, 710, 720–725, 727–733). [`inference_deals`](docs/tech-specs/data-schema.md#inference_deals) and [`inference_ticks`](docs/tech-specs/data-schema.md#inference_ticks) are populated from live capture instead of only from retained rows. No config change is needed; expect more `raw_events` volume wherever inference deals are active, since a deal emits ten-plus events per match plus one per claimed tick.

### Removed

- `inference_markets.producer`, `.model_name` and `.model_version` — the parsed name parts behind the retired `model` object. **Migration `0005_drop_inference_model_name_parts.sql` must be run.** Nothing is lost that cannot be recovered: they were derived from `model_ref`, which stays and is re-read from the book's `getModelName()` getter on every reconcile.

### Changed

- [`inference_deals`](docs/tech-specs/data-schema.md#inference_deals) describes a deal's **current** match, not its whole history. `cleanupUnopened` no longer destroys the deal on a buyer no-show, so one `TokenContract` address serves several matches: The wind-down is silent on chain, so the cycle is ended by the next SELL offer naming the deal (a funded deal cannot post one) and by `TokenContract.StreamFunded` as a backstop; either resets the per-cycle columns (`buyer_note`, `deposit`, `funded_at_chain`, `price_per_tick`, `opened_at_chain`, `settled_at_chain`, `close_kind`, `clean_settlement`, `disputed_at_chain`, `trusted_ticks`, `claimed_ticks`, `finalized_ticks`) and deletes the deal's `inference_ticks` rows, and `buyer_note` is newest-wins rather than first-write-wins. `orderbook_address` and `seller_note` are unchanged by a new cycle. The history of matches is [`inference_trades`](docs/tech-specs/data-schema.md#inference_trades).
- A deal's gas is an ECC[2] reserve that each entrypoint burns a measured charge from, instead of a one-time native deposit sized by a published formula. `fundDeal`, `fundBuyerBond` and `fundDeployShell` send under flag 1 rather than 17, so the value stays SHELL instead of converting to native on arrival; a buyer's calls into a deal carry their own charge on the message.
- `RootPN.withdrawTokens` is gated on the sender being a note of either code generation (was: the current generation only). Parameters are unchanged.

## [2026-07-25]

### Changed

- inference-market **4.0.28** — PR627 coherence pass: make code, ABI, and canon describe one seller-bond model (owner-approved via dexdo-cli-private PR522/PR627).
  - **Seller-bond terminology, no dual path.** Renamed the seller-collateral entrypoints and surface — `fundProbeCommission → fundSellerBond`, `postProbeCommission → postSellerBond`, event `ProbeCommissionFunded → SellerBondFunded`, getter `getProbe → getSellerBond` (`bondFunded`/`bondHeld`/`bondRequired`), errors `ERR_PROBE_*_FUNDED → ERR_BOND_*_FUNDED` — and removed the dead `SELLER_PROBE_COMMISSION_BPS` constant. No compatibility alias (4.0.28 is not yet deployed). The bond is `2P` and the platform fee stays a separate buyer commission.
  - **Note-lock fully removed.** Deleted the inert inference note-lock from `PrivateNote` (`streamLock`/`streamUnlock`/`streamDisputeLock`/`streamDisputeUnlock`/`getStreamLocks`/`forceClearStreamLocks`, the `_streamLocks`/`_disputeLocks` state and gate, the `IStreamNote` interface, `ERR_STREAM_LOCKED`, and `STREAM_LOCK_MAX`). The per-TC mirror bond is the seller's only at-risk mechanism; notes are never frozen by an inference stream or dispute.
  - **`PRICE_STEP = 1e9` (1 SHELL)** confirmed canonical on limit SELL / limit BUY / subscription (market BUY exempt); canon (`SELLER_BOND`, §3.1.2 burn `P` vs `P`, §8 refund-to-buyer) aligned to the model.
  - Full re-pin to the coherent head: `PrivateNote → 98179ac7`, `TokenContract → 2f6159b7`, `RootModel → 9b09eb90`, `SuperRoot → d35073ec`, `InferenceOrderBook → d61d91f0`, `ModelRegistry.IOB_CODE_HASH → f93508a1`, `RootPN → 25789d96`. Local pins.
- inference-market **4.0.28** — three coordinated changes to the private-inference contracts:
  - **Subscriptions: unused cycle budget refunds to the buyer** (no longer forfeited to sellers). Removed the per-seller forfeit accounting from `InferenceOrderBook` (the forfeit-pool / cycle-funded / cycle-seller maps, the cycle-forfeited and forfeit-claimed events, and the forfeit-claim entrypoint) and the matching `PrivateNote` relay. Sellers are still paid per delivered tick in their `TokenContract`; the matcher only throttles the weekly spend and returns the remainder to the buyer on cycle rollover, early full-fill, cancel, and expiry — no relationship graph is stored.
  - **Seller collateral: a symmetric mirror bond held in the `TokenContract`** (spec §4.2). The seller posts a 2-tick bond (`fundProbeCommission` now funds `2P`, not a small commission) that mirrors the buyer's at-risk deposit `D`. On a dispute that reaches timeout with no concession, the disputed `D` is burned AND an equal `D` of the bond is burned (the seller gets nothing from the disputed ticks); the bond returns in full on a clean close, a concession, an abandon, or a seller no-show. Note-locking is removed — both sides' at-risk value lives inside the TC, so `PrivateNote` streams are never frozen; a new `abandonDispute` lets the buyer settle a dispute to the standard split.
  - **Oracle (#588): a normal PMP cancellation now releases the `OracleEventList` event count.** `PMP.cancelEvent` (and the onBounce / rejectEvent cleanup) call `_releaseOracleCounts` exactly once via a `_countReleased` latch, and `OracleEventList.cancelEvent` guards against underflow — so a normally-cancelled confirmed event decrements its count to zero and can later be deleted.
  - Full contract-stack re-pin: `InferenceOrderBook → c308b838`, `TokenContract → c50e36e8`, `RootModel → 0fa1ef35`, `SuperRoot → 35258fbb`, `ModelRegistry.IOB_CODE_HASH → c308b838`, `PrivateNote → 69948118`, `RootPN → 662f14ce`, `PMP → c5da4a1c`, `OracleEventList → d2278623`. Local pins.

## [2026-07-24]

### Changed

- Vendored contracts → inference-market **4.0.28**: `InferenceOrderBook` hardening (issues #558–#567) — true fill-or-kill (per-order simulation), unknown flag-bit rejection, `ticks >= 2` subscriptions serialized through the match queue with the current-cycle forfeit settled on cancel, bounded expired-GTD cleanup, a terminal result for `cancelOrder`, POST_ONLY tested against executable liquidity, and a minimum price step of 1 SHELL (order prices must be a whole multiple of `1_000_000_000`). Re-pinned `ModelRegistry.IOB_CODE_HASH → 19014ccc`; local pins.

## [2026-06-10]

### Added

- OrderBook protocol-fee collection: `OrderBook` reports its accumulated taker-fee share to `RootPN.collectProtocolFee` at shutdown, and the root owner withdraws it via `RootPN.withdrawProtocolFees` (with a `getProtocolFee` getter). `RootPN` tracks `_protocolFees` per token type; the backing real ECC already sits in RootPN reserves. New events `ProtocolFeeCollected` (external id `155`) and `ProtocolFeeWithdrawn` (`156`); `RootPN` ABI and [docs/contract-specs/dex-events-routing.md](docs/contract-specs/dex-events-routing.md) updated. The indexer decodes both new events into `raw_events`; they have no projector and are stored as `Unknown`.

## [2026-06-05]

### Added

- `GET /api/v1/oracles`: public oracle-discovery endpoint returning oracles, their event lists, and available events for market creation. Supports `oracleAddress`, `eventId`, `deadlineBefore`, `cursor`, and clamped `limit` filters; pagination is by oracle, ordered by oracle name, then event-list index, deadline, and event id. Response includes list descriptions, per-event oracle fee, trusted address, and sorted outcome labels. Added domain/application DTOs, `GetOraclesUseCase`, Postgres two-phase listing, API route/OpenAPI output, and DB-backed + HTTP coverage.
- Oracle event-list description indexing: `oracle_event_lists.description text not null` is populated from `Oracle.OracleEventListDeployed`, and `OracleEventList` contracts/ABI now carry a deploy-time description plus `setDescription` / `DescriptionUpdated`.

### Changed

- OracleEventList reconciliation now persists `outcomeNames` into `oracle_events.outcome_names_jsonb` alongside `describe` and `trust_addr`; `/api/v1/oracles` hides unreconciled events so `events[].outcomes` is not empty because metadata has not been fetched yet.
- `docs/api-spec.md`, `docs/openapi.yaml`, and `docs/tech-specs/{read-api,indexer,data-schema}.md` document the oracles endpoint, availability/filter semantics, schema changes, and current indexer limitations for post-deploy list-description updates.

## [2026-06-04]

### Added

- Indexer metrics `orders_created_event_cnt` and `order_partially_filled_event_cnt`, exported over OpenTelemetry/OTLP. Both are observable counters derived from `raw_events` totals (`OrderBook.OrderPlaced` / `OrderBook.PartialFill`), refreshed every 15s and pushed every 30s; collection is gated on the `OTEL_EXPORTER_OTLP_*` env (no-op when unset). New `dodex-metrics` crate encapsulates the OTLP setup. See [docs/tech-specs/indexer.md](docs/tech-specs/indexer.md#metrics).

## [2026-05-29]

### Added

- `sdk/`: new `dodex-sdk` crate — the write-side DEX facade over `ackinacki-kit` (private notes, order book, PMP, oracle/market) plus the halo2 voucher proof pipeline. Kept as its own workspace and `exclude`d from the root build, since the halo2 pipeline pulls private SSH-only git sources that CI hosts have no key for; build it directly from `sdk/`.

## [2026-05-28]

### Added

- `POST /api/v1/buyFullSet`: trader-facing endpoint backing the chain `PrivateNote.splitFullSet`. Permitted on `AWAITING_FREEZE` (first successful call activates the OrderBook) and `TRADING`. New `BuyFullSetUseCase`, `chain.split_full_set_timeout_ms` config, and `docs/tech-specs/write-api.md` section. `crates/chain` promotes `Dex::split_full_set` out of `test-helpers` into the prod path.

## [2026-05-27]

### Added

- `openapi/openapi.yaml` and `openapi/index.html`: published OpenAPI spec rendered on GitHub Pages. Added `services/api/src/bin/gen-openapi.rs` generator binary, `openapi/generate.sh`, and `.github/workflows/{openapi,pages}.yml` to regenerate and deploy on push.
- `crates/chain/`: new chain-client crate carved out of `infrastructure/chain_sender.rs` (client, DTO, error, test helpers).
- `makerComission` and `takerComission` fields on the `GET /api/v1/markets` response. Signed `DECIMAL` strings (e.g. `"0.00045"`); a negative `makerComission` is a maker rebate credited rather than debited.

### Changed

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
- `GET /api/v1/depth` order-book endpoint (#11) — initial DEX read API skeleton, ABI bundle under `contracts/dex/`, root `Cargo.toml` workspace, `config/{api,indexer}.local.yaml`, and `LICENSE.md`.
- `docs/README.md` — documentation map with file ownership.
- `CHANGELOG.md` (this file).

### Changed

- Restructured `docs/tech-specs/` (#18):
  - `market-data-api.md` → `read-api.md` (scope: all read endpoints).
  - `market-data-indexer.md` → `indexer.md`.
  - `trading-api/write-api.md` → `write-api.md` (top-level).
- Updated cross-references in `services/api/README.md`, `services/indexer/README.md`, `AGENT_REQUIREMENTS.md`, and inline test comments.
- Rewrote root `README.md` for DEX.DO (previous content was carried over from a different project).
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
