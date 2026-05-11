# Data Schema: Backend Notes

Postgres tables that back the DODEX read-model and indexer. Source of truth is the migration set under `/migrations`; this document describes intent and field semantics. Schema changes ship as numbered migration files (`NNNN_*.sql`) and are applied by `sqlx::migrate!` at service startup (`crates/infrastructure/src/database.rs`).

Tables fall into four buckets:

| Bucket | Tables | Owner |
| --- | --- | --- |
| Reference data | `ref_tokens` | Seeded by migrations; read-only at runtime. |
| Indexer infrastructure | `raw_events`, `indexer_cursors` | Indexer ingestion path. |
| Read-model — discovery | `oracles`, `oracle_event_lists`, `oracle_events` | Indexer projectors + OEL reconciler. |
| Read-model — markets | `markets`, `market_outcomes`, `live_orders`, `order_book_snapshots` | Indexer projectors + market reconciler. |

## Reference data

### `ref_tokens`

Static collateral-token catalogue. The indexer joins against it when a `PMPDeployed` event references a `tokenType`; the API surfaces precision and trading-rule constants per outcome through it.

| Column | Type | Notes |
| --- | --- | --- |
| `token_type` | `integer` PK | Numeric token type as the contract uses it (`NACKL=1`, `SHELL=2`, `USDC=3`). |
| `token_code` | `text` UNIQUE | User-facing asset code (`USDC`, etc.). |
| `decimals` | `integer` | On-chain decimal places. |
| `min_notional` | `numeric(78,0)` | Minimum order notional, in raw uint256 units of the token. Scaled to a decimal at API render time. |
| `lot_size` | `numeric(78,0)` | Minimum order quantity increment, raw units. |
| `tick_size_bps` | `numeric(78,0)` | Price tick in basis points (contracts use `TICK_SIZE = 10`). |
| `price_precision` | `integer` | Decimal places for the price field exposed to clients. |
| `quantity_precision` | `integer` | Decimal places for the quantity field exposed to clients. |
| `enabled` | `boolean` | Reserved — not read on the hot path today. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Seeded values: `(1, NACKL, 9, ...)`, `(2, SHELL, 9, ...)`, `(3, USDC, 6, ...)` (`migrations/0001:144-146`). Adding a new collateral token is a migration-time change.

## Indexer infrastructure

### `raw_events`

The append-only event log. Every message edge the indexer pulls from the GraphQL stream lands here, decoded or not, before any projector runs. It is the recovery boundary for the read-model: reprojection replays decoded but unprojected rows here, and downstream tables can always be rebuilt from this one plus a clean schema.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Insertion order; tiebreaker for chain-time ordering. |
| `msg_id` | `text` UNIQUE | Chain-side message id. Prevents duplicate ingestion across overlapping page fetches. |
| `created_at_chain` | `timestamptz` | Chain block timestamp from the GraphQL `created_at` field. Nullable — preserved as-is when the indexer receives a null/unparseable timestamp. |
| `src_address` | `text` (nullable per 0002) | Source contract address (the contract that emitted the event). |
| `dst_address` | `text` (nullable per 0002) | Destination address from the message header. |
| `event_type` | `text` (nullable per 0002) | `"<ContractKind>.<EventName>"`, e.g. `OrderBook.OrderPlaced`. NULL when decoding failed or the body was not an event message. |
| `body_json` | `jsonb` | Raw message body JSON as ingested. |
| `decoded` | `jsonb` (added in 0003) | ABI-decoded event payload. Filled at ingest time if decoding succeeds; reprojection reuses this — bodies are not re-decoded. |
| `processed_at` | `timestamptz` | Stamped by the projector when the row is `Applied` or `Unknown`. NULL = pending; covered by the reprojection sweep. |
| `created_at` | `timestamptz` | Indexer ingestion time (wall-clock). |

Indices:

| Index | Purpose |
| --- | --- |
| `raw_events_event_type_idx` | General `event_type` scans (debug, analytics). |
| `raw_events_event_type_decoded_idx` (partial, `event_type IS NOT NULL`) | Same scope but optimised for decoded rows. |
| `raw_events_created_at_chain_idx` (desc) | Time-window queries. |
| `raw_events_pending_projection_idx` (partial: `processed_at IS NULL AND event_type IS NOT NULL AND decoded IS NOT NULL`) | Drives reprojection (`crates/infrastructure/src/indexer_repo.rs::reproject_pending`). |

### `indexer_cursors`

Resume-points per ingestion stream. The indexer's main fetch loop persists the cursor after every page so a restart does not reprocess the full history.

| Column | Type | Notes |
| --- | --- | --- |
| `stream_name` | `text` PK | Logical stream identifier (e.g. one per filter-set the indexer subscribes to). |
| `cursor` | `text` | Opaque GraphQL cursor returned by shellnet. |
| `last_seen_lt` | `text` | Last observed logical time. Reserved for diagnostics. |
| `updated_at` | `timestamptz` | Last successful page commit. |

## Read-model — discovery

The discovery side of the indexer tracks oracles, their event lists, and the events those lists carry. These tables feed the `event.*` block in `/api/v1/markets` responses.

### `oracles`

One row per oracle service the system knows about. Populated by `RootOracle.OracleDeployed` (and back-filled from `EventList` parent lookups).

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `name` | `text` UNIQUE | Oracle name as registered on chain (e.g. `ElectionOracle`). |
| `address` | `text` UNIQUE | Oracle contract address. Source for `event.oracleAddress`. |
| `deploy_msg_id` | `text` UNIQUE (nullable) | Message id of the deploy event. NULL if the oracle was discovered indirectly. |
| `pubkey` | `text` | Oracle pubkey from the deploy event. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

### `oracle_event_lists`

Each oracle owns a sequence of EventList contracts (`Oracle.OracleEventListDeployed`). Reconciler fanout for event metadata happens at this granularity.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `msg_id` | `text` UNIQUE | Deploy event message id. |
| `oracle_id` | `bigint` FK → `oracles(id)` ON DELETE CASCADE | Parent oracle. |
| `address` | `text` UNIQUE | EventList contract address. |
| `list_index` | `bigint` | Oracle-local index of the event list. |
| `created_at` | `timestamptz` | Bookkeeping. |
| `last_reconcile_failed_at` (added 0011) | `timestamptz` | Stamped when a reconcile attempt fails. Used for backoff and queue ordering. |
| `reconcile_attempts` (added 0011) | `integer` default `0` | Diagnostic counter for permanently broken EventLists. |

Index: `oracle_event_lists_oracle_id_idx` for parent fan-out.

### `oracle_events`

The actual events inside each EventList. Two writers:

- **Projector** writes `event_name`, `oracle_fee`, `deadline`, and the `confirmed_*` columns from `EventAdded` / `EventConfirmed`.
- **OEL reconciler** fills the metadata that lives only in `OracleEventList._events` getter state: `describe` and `trust_addr`.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `eventlist_id` | `bigint` FK → `oracle_event_lists(id)` ON DELETE CASCADE | Parent EventList. |
| `internal_id_in_eventlist` | `numeric(78,0)` | Event id within the EventList. The pair `(eventlist_id, internal_id_in_eventlist)` is UNIQUE. |
| `event_name` | `text` | From `EventAdded`. Surfaces as `event.eventName`. |
| `oracle_fee` | `numeric(78,0)` | From `EventAdded`. |
| `deadline` | `bigint` | Event deadline (unix seconds). |
| `describe` | `text` | Event description — reconciler-only field. NULL until OEL reconciler runs. |
| `count` | `numeric(78,0)` | Reserved metadata field from `_events`. |
| `trust_addr` | `text` | Reconciler-only field. Optional on chain — may stay NULL even after reconciliation (see migration 0012). |
| `outcome_names_jsonb` | `jsonb` default `'{}'::jsonb` | Outcome label map (`outcomeId → name`). |
| `is_deleted` | `boolean` default `false` | Soft-delete flag for events that disappear from the EventList. |
| `last_seen_at` | `timestamptz` | Updated on every projector pass that touches the row. |
| `confirmed_pmp_address` (added 0004) | `text` | Set by `EventConfirmed`. Links an event to the PMP that markets it. |
| `confirmed_at` (added 0004) | `timestamptz` | Stamp time (currently wall-clock; see review note in `docs/review-fixes-2026-05-11.md`). |
| `meta_reconciled_at` (added 0012) | `timestamptz` | Per-row marker — set unconditionally by the OEL reconciler after a successful getter pass, even when `describe`/`trust_addr` come back NULL on chain. Drives the pending-row predicate so legitimately-null fields don't cause infinite re-fetch. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `oracle_events_eventlist_id_idx` | Parent fan-out. |
| `oracle_events_deadline_idx` | Time-window queries. |
| `oracle_events_confirmed_pmp_idx` (partial: `confirmed_pmp_address IS NOT NULL`) | Reverse-lookup from PMP back to event. |
| `oracle_events_pending_meta_idx` (partial: `meta_reconciled_at IS NULL`) | Drives the OEL reconciler's pending-row SELECT. Replaced the original `describe IS NULL`-only index in migration 0012. |

## Read-model — markets

### `markets`

One row per PMP (Prediction Market Pool) contract observed on chain. Discovered by `PMPDeployed`, completed by the market reconciler reading `PMP.getDetails()`, transitioned through lifecycle events (`TimingsSet`, `PoolsFrozen`, `Resolved`, `PMPCancelled`, `EventCancelled`). Hidden from the public API until `last_reconciled_at` is non-null.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `pmp_address` | `text` UNIQUE | The PMP contract address. Exposed as `marketAddress`. |
| `market_id` | `text` (nullable per 0005) | Market identifier from `getDetails()`. NULL pre-reconcile. |
| `name` | `text` (nullable per 0005) | Market display name from `getDetails()`. Surfaces as `marketName`. |
| `token_type` | `integer` FK → `ref_tokens(token_type)` | Quote-asset token type. |
| `token_code` | `text` | Quote-asset code (denormalised from `ref_tokens` for read speed). |
| `event_id` | `numeric(78,0)` | Oracle event id this market resolves against. |
| `oracle_list_hash` | `numeric(78,0)` (nullable per 0005) | EventList hash used in OrderBook derivation. NULL pre-reconcile. |
| `orderbook_address` | `text` (nullable) | The deterministic OrderBook address. Written by the reconciler **only when `frozen_at IS NOT NULL`** (migration 0013, H1 fix) — until then this column stays NULL and the API reports `orderBookAddress: null`. |
| `approved` | `boolean` default `false` | Approval flag from `getDetails()`; flipped to `true` on `TimingsSet`. |
| `is_cancelled` | `boolean` default `false` | On-chain cancellation flag from `getDetails()`. Either this or `cancelled_at` being set is enough to flip the derived status to `CANCELLED`. |
| `stake_start` / `stake_end` / `result_start` / `result_end` | `bigint` (nullable) | Lifecycle timings (unix seconds). Written only by `TimingsSet`; reconciler does **not** touch these (H2 fix). NULL on all four = PENDING. |
| `num_outcomes` | `integer` default `0` | Outcome count from `getDetails()`. |
| `oracle_event_lists_json` (added 0005) | `jsonb` | Auxiliary data from `PMPDeployed` for outcome-resolution. |
| `oracle_fee_json` (added 0005) | `jsonb` | Same. |
| `last_reconciled_at` (added 0005) | `timestamptz` | Stamped by the market reconciler after a successful pass. The public API filters on `last_reconciled_at IS NOT NULL` — markets without this are invisible to clients. |
| `frozen_at` (added 0006) | `bigint` | `PoolsFrozen` block timestamp. Required for any post-freeze status (TRADING / RESOLVING / EXPIRED / RESOLVED). |
| `resolved_at` (added 0006) | `bigint` | `PMP.Resolved` block timestamp. |
| `resolved_outcome_id` (added 0006) | `integer` | Winning outcome id. |
| `cancelled_at` (added 0006) | `bigint` | Block timestamp of `PMP.PMPCancelled` or `PMP.EventCancelled`. May also be back-filled to `now()` by the reconciler if the chain flag flipped before the event was replayed. |
| `cancel_reason` (added 0006) | `text` | `'PMP_CANCELLED'` or `'EVENT_CANCELLED'`. Required when `cancelled_at` is set; the API fails closed (HTTP 503) when CANCELLED is derived without a valid reason. |
| `last_reconcile_failed_at` (added 0011) | `timestamptz` | Backoff bookkeeping for the market reconciler. |
| `reconcile_attempts` (added 0011) | `integer` default `0` | Diagnostic counter. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `markets_market_id_idx` | Lookup by `market_id`. |
| `markets_status_idx` (`approved, is_cancelled`) | Coarse status filters. |
| `markets_pending_reconcile_idx` (partial: `last_reconciled_at IS NULL`) | Drives the market reconciler's pending-row SELECT. |
| `markets_terminal_idx` (partial: `resolved_at IS NOT NULL OR cancelled_at IS NOT NULL`) | Terminal-status filters. |

### `market_outcomes`

One row per outcome of each market. Source for outcome listings and the per-outcome trading-rule constants the API publishes. Populated by the reconciler after `getDetails()` resolves outcome names + per-outcome precision metadata.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `market_id_fk` | `bigint` FK → `markets(id)` ON DELETE CASCADE | Parent market. |
| `pmp_address` | `text` | Denormalised from `markets.pmp_address` for fast `(pmp_address, outcome_id)` joins. |
| `outcome_id` | `integer` | Stable outcome id used in trading. The pair `(pmp_address, outcome_id)` is UNIQUE. |
| `outcome_name` | `text` | Outcome display name. |
| `symbol` | `text` UNIQUE | The outcome-token symbol (`<marketName>-<OUTCOME_NAME>`). |
| `price_precision` | `integer` | Decimal places for prices. Used at API render time to scale raw uint256 prices. |
| `quantity_precision` | `integer` | Decimal places for quantities. Same. |
| `tick_size` | `text` | Minimum price increment as a decimal string. |
| `step_size` | `text` | Minimum quantity increment as a decimal string. |
| `min_notional` | `text` | Minimum order notional as a decimal string. |
| `max_batch_size` | `integer` | Max orders per batch request for this outcome. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Index: `market_outcomes_market_id_fk_idx` for parent fan-out. Symbol is globally unique by construction.

### `live_orders`

Per-order read-model that backs `/api/v1/depth`. One row per chain-side order, mutated in place as `OrderPlaced` / `OrderFilled` / `OrderCancelled` events arrive. Never deleted — FILLED / CANCELLED rows stay for sequence-number monotonicity (the depth handler reads `max(last_event_lt)` over **all** rows for the `(orderbook, outcome)` pair).

| Column | Type | Notes |
| --- | --- | --- |
| `orderbook_address` | `text` (PK part 1) | OrderBook contract address. |
| `order_id` | `numeric(78,0)` (PK part 2) | Chain-side order id. The pair `(orderbook_address, order_id)` is the primary key. |
| `outcome_id` | `integer` | Which outcome this order is on. |
| `is_buy` | `boolean` | Side. `true` = bid, `false` = ask. |
| `price` | `numeric(78,0)` | Order price as the contract emitted it (raw uint256). Scaled to a decimal at API render time. |
| `amount_remaining` | `numeric(78,0)` | Quantity still open. Set on `OrderPlaced`, decremented on `OrderFilled`, zeroed on `OrderCancelled`. |
| `client_order_id` | `text` | Optional client-supplied id. |
| `status` | `text` CHECK `IN ('OPEN', 'FILLED', 'CANCELLED')` | Order lifecycle. Depth aggregation filters on `status = 'OPEN' AND amount_remaining > 0`. |
| `last_event_lt` | `bigint` | Chain block timestamp of the most recent event that touched this order. Monotonic via `greatest(existing, new)` on every write — protects against out-of-order delivery. Feeds `lastUpdateId` in depth responses. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Index: `live_orders_open_book_idx` — partial, `(orderbook_address, outcome_id, is_buy, price desc) WHERE status = 'OPEN'`. Sized for the depth query: top-N levels per side per outcome.

### `order_book_snapshots`

Reserved table for cached depth snapshots. Not used by the current depth handler — `/api/v1/depth` aggregates `live_orders` on every request. Kept in the schema for a future cache-warming path; safe to ignore until then.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | |
| `symbol` | `text` UNIQUE | Outcome symbol. |
| `orderbook_address` | `text` | |
| `last_update_id` | `bigint` | |
| `bids_jsonb` / `asks_jsonb` | `jsonb` default `'[]'::jsonb` | |
| `updated_at` | `timestamptz` | |

## System tables

`_sqlx_migrations` is created and maintained by `sqlx::migrate!`. It records which migration files have been applied. Do not touch it in application code.

## Schema evolution

Every schema change ships as a new numbered migration file. Conventions:

- Use `if not exists` / `if exists` on DDL so re-runs are idempotent.
- For new columns, prefer `add column if not exists` with a sensible default — never break startup on an empty database.
- Partial indices are preferred over full ones for "pending row" predicates; they shrink with reconciliation progress.
- Add a header comment on every migration explaining *why* the change is needed and which code path requires it. Migrations are read by reviewers and operators as much as the code is.

The full migration set (`migrations/0001_*.sql` … `migrations/0013_*.sql`) is the canonical reference; this document summarises intent but does not replace it.
