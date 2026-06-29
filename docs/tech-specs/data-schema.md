# Data Schema Technical Specification

Postgres tables that back the DEX.DO read-model and indexer. Source of truth is the migration set under `/migrations`; this document describes intent and field semantics. Schema changes ship as numbered migration files (`NNNN_*.sql`) and are applied by `sqlx::migrate!` at service startup (`crates/infrastructure/src/database.rs`).

Tables fall into five buckets:

| Bucket | Tables | Owner |
| --- | --- | --- |
| Reference data | `ref_tokens` | Seeded by migrations; read-only at runtime. |
| Indexer infrastructure | `raw_events`, `indexer_cursors` | Indexer ingestion path. |
| Read-model — discovery | `oracles`, `oracle_event_lists`, `oracle_events` | Indexer projectors + OracleEventList reconciler. |
| Read-model — markets | `markets`, `market_outcomes`, `live_orders`, `order_book_snapshots` | Indexer projectors + market reconciler. |
| Read-model — inference markets | `inference_markets`, `inference_orders` | Indexer projectors + inference reconciler. |
| Read-model — inference deals | `inference_deals`, `inference_ticks` | Inference SETTLEMENT projector (writer). Intended to back the forthcoming rewards service (reader). |
| Authentication and credentials | `accounts`, `api_keys` | Operator-provisioned; read on every signed request by the auth middleware. |

## Glossary

**Read-model** — Postgres tables prepared for API reads. They are derived from chain events and contract state so the API can answer requests without decoding the blockchain state on every call.

**Projector** — code that handles one decoded chain event and writes the corresponding read-model change. For example, the `OrderBook.OrderPlaced` event creates or refreshes a row in `live_orders`.

**Reconciler** — background indexer task that periodically reads contract state through getters and fills fields that events alone do not provide. The market reconciler reads PMP state (`getDetails`, `getOrderBookAddress`) and updates `markets` / `market_outcomes`. The OracleEventList reconciler reads `_events` from each EventList contract and fills missing event metadata in `oracle_events`, such as `describe` and `trust_addr`.

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

Seeded values: `(1, NACKL, 9, ...)`, `(2, SHELL, 9, ...)`, `(3, USDC, 6, ...)`. Adding a new collateral token is a migration-time change.

## Indexer infrastructure

### `raw_events`

The append-only event log. Every message edge the indexer pulls from the GraphQL stream lands here, decoded or not, before any projector runs. It is the recovery boundary for the read-model: reprojection replays decoded but unprojected rows here, and downstream tables can always be rebuilt from this one plus a clean schema.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Insertion order. Not used for ordering — that's `chain_order` below. |
| `msg_id` | `text` UNIQUE | Chain-side message id. Prevents duplicate ingestion across overlapping page fetches. |
| `chain_order` | `text` NOT NULL | Global lex-sortable chain order from the GraphQL gateway's `msg_chain_order`. The strict-monotonic projection key — `created_at_chain` collides within one second and drifts across shards, so any reproject sweep that ordered on time could apply `OrderFilled` before its parent `OrderPlaced`. Required on every row; edges arriving without it are dropped at ingest. |
| `created_at_chain` | `timestamptz` | Chain block timestamp from the GraphQL `created_at` field. Kept for diagnostics/analytics only — not load-bearing for ordering. Nullable, preserved as-is. |
| `src_address` | `text` | Source contract address (the contract that emitted the event). |
| `dst_address` | `text` | Destination address from the message header. |
| `event_type` | `text` | `"<ContractKind>.<EventName>"`, e.g. `OrderBook.OrderPlaced`. NULL when decoding failed or the body was not an event message. |
| `body_json` | `jsonb` | Raw message body JSON as ingested. |
| `decoded` | `jsonb` | ABI-decoded event payload. Filled at ingest time if decoding succeeds; reprojection reuses this — bodies are not re-decoded. |
| `processed_at` | `timestamptz` | Stamped by the projector when the row is `Applied` or `Unknown`. NULL = pending; covered by the reprojection sweep. |
| `created_at` | `timestamptz` | Indexer ingestion time (wall-clock). |

Indices:

| Index | Purpose |
| --- | --- |
| `raw_events_event_type_idx` | General `event_type` scans (debug, analytics). |
| `raw_events_event_type_decoded_idx` (partial, `event_type IS NOT NULL`) | Same scope but optimised for decoded rows. |
| `raw_events_created_at_chain_idx` (desc) | Time-window queries (analytics only). |
| `raw_events_chain_order_idx` | Backs the projection loop's `ORDER BY chain_order ASC`. |
| `raw_events_pending_chain_order_idx` (partial: `processed_at IS NULL AND event_type IS NOT NULL AND decoded IS NOT NULL`) | Backs the projection loop's keyset scan (`crates/infrastructure/src/indexer_repo.rs::reproject_pending_from`). Added by migration `0004`; replaces the former `raw_events_pending_projection_idx` which was keyed on `(created_at_chain, id)`. |
| `raw_events_pending_src_idx` (partial: `processed_at IS NULL AND event_type IS NOT NULL AND decoded IS NOT NULL`) | Indexed on `src_address`. Allows the inference reconciler's sweep catch-up gate to probe "are there any pending events for this book?" as an index probe on `src_address = orderbook_address`, rather than a full-table scan. Added by migration `0005`. |

### `indexer_cursors`

Resume-points per ingestion stream. The indexer's main fetch loop persists the cursor after every page so a restart does not reprocess the full history.

| Column | Type | Notes |
| --- | --- | --- |
| `stream_name` | `text` PK | Logical stream identifier (e.g. one per filter-set the indexer subscribes to). |
| `cursor` | `text` | Opaque cursor returned by GraphQL server. |
| `updated_at` | `timestamptz` | Last successful page commit. |
| `at_head` | `boolean` NOT NULL default `false` | Set to `true` by the capture loop after a drain that returned `has_next_page=false` (the cursor is caught up to the chain tip); reset to `false` whenever more pages follow. Read by the inference reconciler as the `at_head` sweep catch-up gate: phantom-cancel sweeps must not fire while the gateway still has older pages ahead of the cursor. Added by migration `0006`. |

## Read-model — discovery

The discovery side of the indexer tracks oracles, their event lists, and the events those lists carry. These tables feed the `event.*` block in `/api/v1/prediction/markets` responses.

### `oracles`

One row per oracle service the system knows about. Populated by the `RootOracle.OracleDeployed` event and back-filled from `EventList` parent lookups.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `name` | `text` UNIQUE | Oracle name as registered on chain (e.g. `ElectionOracle`). |
| `address` | `text` UNIQUE | Oracle contract address. |
| `deploy_msg_id` | `text` UNIQUE (nullable) | Message id of the deploy event. NULL if the oracle was discovered indirectly. |
| `pubkey` | `text` | Oracle pubkey from the deploy event. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

### `oracle_event_lists`

Each oracle owns a sequence of EventList contracts created by the `Oracle.OracleEventListDeployed` event. The indexer's OracleEventList reconciler processes one EventList at a time: it reads that contract's `_events` getter and updates the related `oracle_events` rows with metadata such as `describe` and `trust_addr`.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `msg_id` | `text` UNIQUE | Deploy event message id. |
| `oracle_id` | `bigint` FK → `oracles(id)` ON DELETE CASCADE | Parent oracle. |
| `address` | `text` UNIQUE | EventList contract address. |
| `list_index` | `bigint` | Oracle-local index of the event list. |
| `description` | `text` `NOT NULL` | Human-readable list description from the `OracleEventListDeployed` event (always carried; projector reads it strictly). May be an empty string, never null. Surfaced as `/api/v1/oracles` `eventLists[].description`. |
| `created_at` | `timestamptz` | Bookkeeping. |
| `last_reconcile_failed_at` | `timestamptz` | Stamped when a reconcile attempt fails. Used for backoff and queue ordering. |
| `reconcile_attempts` | `integer` default `0` | Diagnostic counter for permanently broken EventLists. |

Index: `oracle_event_lists_oracle_id_idx` speeds up loading all EventList rows for one oracle.

### `oracle_events`

The actual events inside each EventList. Two writers:

- **Projector** writes `event_name`, `oracle_fee`, `deadline`, and the `confirmed_*` columns from the `EventAdded` and `EventConfirmed` events.
- **OracleEventList reconciler** fills the metadata that lives only in getter state: `describe` and `trust_addr` from `_events`, plus — for numeric **range events** — `range_ob_address` and `range_bounds_jsonb` from `OracleEventList.getRangeData(eventId)`. The `EventAdded` event is identical for plain and range events and carries neither field, so the inference linkage is reconciler-sourced.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `eventlist_id` | `bigint` FK → `oracle_event_lists(id)` ON DELETE CASCADE | Parent EventList. |
| `internal_id_in_eventlist` | `numeric(78,0)` | Event id within the EventList. The pair `(eventlist_id, internal_id_in_eventlist)` is UNIQUE. |
| `event_name` | `text` | From the `EventAdded` event. Surfaces as `event.eventName`. |
| `oracle_fee` | `numeric(78,0)` | From the `EventAdded` event. |
| `deadline` | `bigint` | Event deadline (unix seconds). |
| `describe` | `text` | Event description — reconciler-only field. NULL until OracleEventList reconciler runs. |
| `count` | `numeric(78,0)` | Reserved metadata field from `_events`. |
| `trust_addr` | `text` | Reconciler-only field. Optional on chain — may stay NULL even after reconciliation. |
| `outcome_names_jsonb` | `jsonb` default `'{}'::jsonb` | Outcome label map (`outcomeId → name`). |
| `range_ob_address` | `text` (nullable) | For a numeric **range event**: the `InferenceOrderBook` whose weekly-median price resolves the outcome (`OracleEventList._rangeData[eventId].ob`, spec §6.2). NULL for plain events. Reconciler-only. The reverse lookup (markets resolving from a given inference book) backs the `resolvesFrom` filter on `/api/v1/prediction/markets`. |
| `range_bounds_jsonb` | `jsonb` (nullable) | For a range event: the strictly-increasing numeric upper bounds (`n` bounds → `n+1` outcomes), as a JSON array of decimal strings. NULL for plain events. Reconciler-only. The human labels for those ranges are already in `outcome_names_jsonb`, so the API does not re-expose the raw bounds. |
| `is_deleted` | `boolean` default `false` | Soft-delete flag for events that disappear from the EventList. |
| `last_seen_at` | `timestamptz` | Updated on every projector pass that touches the row. |
| `confirmed_pmp_address` | `text` | Set by the `EventConfirmed` event. Links an event to the PMP that markets it. |
| `confirmed_at` | `timestamptz` | Stamp time recorded when the projector observes the confirmation event. |
| `meta_reconciled_at` | `timestamptz` | Per-row marker — set unconditionally by the OracleEventList reconciler after a successful getter pass, even when `describe`/`trust_addr` come back NULL on chain. Drives the pending-row predicate so legitimately-null fields don't cause infinite re-fetch. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `oracle_events_eventlist_id_idx` | Speeds up loading all event rows for one EventList. |
| `oracle_events_deadline_idx` | Time-window queries. |
| `oracle_events_confirmed_pmp_idx` (partial: `confirmed_pmp_address IS NOT NULL`) | Reverse-lookup from PMP back to event. |
| `oracle_events_pending_meta_idx` (partial: `meta_reconciled_at IS NULL`) | Drives the OracleEventList reconciler's pending-row SELECT. |
| `oracle_events_range_ob_idx` (partial: `range_ob_address IS NOT NULL`) | Reverse lookup from an inference order book to the range events (and thus prediction markets) that resolve from it. Backs the `resolvesFrom` filter on `/api/v1/prediction/markets`. |

## Read-model — markets

### `markets`

One row per PMP (Prediction Market Pool) contract observed on chain. Discovered by the `PMPDeployed` event, completed by the market reconciler reading `PMP.getDetails()`, and transitioned by the `TimingsSet` event, the `PoolsFrozen` event, the `Resolved` event, the `PMPRejected` event, and the `EventCancelled` event. Hidden from the public API until `last_reconciled_at` is non-null.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `pmp_address` | `text` UNIQUE | The PMP contract address. Exposed as `predictionMarketAddress`. |
| `market_id` | `text` | Market identifier from `getDetails()`. NULL pre-reconcile. |
| `name` | `text` | Market display name from `getDetails()`. Surfaces as `marketName`. |
| `token_type` | `integer` FK → `ref_tokens(token_type)` | Quote-asset token type. |
| `token_code` | `text` | Quote-asset code (denormalised from `ref_tokens` for read speed). |
| `event_id` | `numeric(78,0)` | Oracle event id this market resolves against. |
| `oracle_list_hash` | `numeric(78,0)` | EventList hash used in OrderBook derivation. NULL pre-reconcile. |
| `orderbook_address` | `text` | The deterministic OrderBook address returned by `PMP.getOrderBookAddress()`. Written by the market reconciler on the first successful pass, including pre-`PoolsFrozen` rows. Nullable only during the pre-reconcile window; the CHECK predicate `last_reconciled_at IS NULL OR orderbook_address IS NOT NULL` enforces that every market visible to the API has a non-null `predictionOrderBookAddress`. A partial UNIQUE index on `orderbook_address WHERE orderbook_address IS NOT NULL` pins the contract-side per-market invariant — `/api/v1/prediction/orders` joins `live_orders` to `markets` on this column and relies on the at-most-one-row guarantee. |
| `approved` | `boolean` default `false` | Approval flag from `getDetails()`; flipped to `true` by the `TimingsSet` event. |
| `is_cancelled` | `boolean` default `false` | On-chain cancellation flag from `getDetails()`. Either this or `cancelled_at` being set is enough to flip the derived status to `CANCELLED`. |
| `stake_start` / `stake_end` / `result_start` / `result_end` | `bigint` (nullable) | Lifecycle timings (unix seconds). Written only by the `TimingsSet` event; reconciler does **not** touch these (H2 fix). NULL on all four = PENDING. |
| `num_outcomes` | `integer` default `0` | Outcome count from `getDetails()`. |
| `oracle_event_lists_json` | `jsonb` | Auxiliary data from the `PMPDeployed` event for outcome-resolution. |
| `oracle_fee_json` | `jsonb` | Same. |
| `last_reconciled_at` | `timestamptz` | Stamped by the market reconciler after a successful pass. The public API filters on `last_reconciled_at IS NOT NULL` — markets without this are invisible to clients. |
| `frozen_at` | `bigint` | Block timestamp of the `PoolsFrozen` event. Required for any post-freeze status (TRADING / RESOLVING / EXPIRED / RESOLVED). |
| `resolved_at` | `bigint` | Block timestamp of the `PMP.Resolved` event. |
| `resolved_outcome_id` | `integer` | Winning outcome id. |
| `cancelled_at` | `bigint` | Block timestamp of the `PMP.PMPRejected` or `PMP.EventCancelled` event. May also be back-filled to `now()` by the reconciler if the chain flag flipped before the event was replayed. |
| `cancel_reason` | `text` | `'PMP_REJECTED_BY_ORACLE'` or `'EVENT_CANCELLED'`. Required when `cancelled_at` is set; the API fails closed (HTTP 503) when CANCELLED is derived without a valid reason. |
| `last_reconcile_failed_at` | `timestamptz` | Backoff bookkeeping for the market reconciler. |
| `reconcile_attempts` | `integer` default `0` | Diagnostic counter. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `markets_market_id_idx` | Lookup by `market_id`. |
| `markets_status_idx` (`approved, is_cancelled`) | Coarse status filters. |
| `markets_pending_reconcile_idx` (partial: `last_reconciled_at IS NULL`) | Drives the market reconciler's pending-row SELECT. |
| `markets_terminal_idx` (partial: `resolved_at IS NOT NULL OR cancelled_at IS NOT NULL`) | Terminal-status filters. |
| `markets_orderbook_address_unique` (partial UNIQUE: `orderbook_address IS NOT NULL`) | Pins the per-market invariant; relied on by `/api/v1/prediction/orders`'s all-markets join. |

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
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Index: `market_outcomes_market_id_fk_idx` speeds up loading all outcome rows for one market. Symbol is globally unique by construction.

### `live_orders`

Per-order read model backing `/api/v1/prediction/depth` and account-scoped
`GET /api/v1/prediction/orders`. One row per chain-side order, mutated in place as
`OrderPlaced`, `OrderFilled`, and `OrderCancelled` events arrive. Rows are never
deleted — `FILLED` / `CANCELLED` entries remain so that history queries and the depth
handler (`max(last_chain_order)` across **all** rows for the `(orderbook, outcome)` pair)
both see them.

| Column | Type | Notes |
| --- | --- | --- |
| `orderbook_address` | `text` (PK part 1) | OrderBook contract address. |
| `order_id` | `numeric(78,0)` (PK part 2) | Chain-side order id. The pair `(orderbook_address, order_id)` is the primary key. |
| `outcome_id` | `integer` | Which outcome this order is on. |
| `is_buy` | `boolean` | Side. `true` = bid, `false` = ask. |
| `price` | `numeric(78,0)` | Order price as the contract emitted it — raw uint256 in **basis points** (probability × `FULL_PERCENT` = 10 000). Decoded to a decimal at API render time (÷ `FULL_PERCENT`, formatted at `price_precision`). |
| `amount_initial` | `numeric(78,0)` | Original order quantity from `OrderBook.OrderPlaced`, in **raw token atoms** (× `10^decimals`). Used with `amount_remaining` to render `origQty` / `executedQty` (decoded ÷ `10^decimals`, formatted at `quantity_precision`) in account order endpoints. |
| `amount_remaining` | `numeric(78,0)` | Quantity not yet filled. Set by the `OrderPlaced` event and decremented by the `OrderFilled` event. `OrderCancelled` preserves the current value as the cancelled remainder so `/api/v1/prediction/orders.executedQty` can be derived as `amount_initial - amount_remaining`; depth ignores the row because `status != 'OPEN'`. See the [orders cancel-remainder cutover note](../migrations/orders-cancel-remainder-cutover.md) for data-bearing deployment guidance. |
| `client_order_id` | `text` | Optional client-supplied id. |
| `owner_pn_address` | `text` | Trading PrivateNote address that owns the order. Initially NULL from `OrderBook.OrderPlaced`; attached by `PrivateNote.OrderPlacedConfirmed` using the event source address. NULL rows can still contribute to public depth, but cannot appear in account-scoped order responses. |
| `status` | `text` CHECK `IN ('OPEN', 'FILLED', 'CANCELLED')` | Order lifecycle. Depth aggregation filters on `status = 'OPEN' AND amount_remaining > 0`. The CHECK is extended to include `'REJECTED'` by the contracts-side follow-up documented in [read-api.md §REJECTED — future work](read-api.md#rejected--future-work); until then no row carries that value. |
| `last_chain_order` | `text` NOT NULL | Chain-order key (`msg_chain_order` from the gateway) of the most recent OrderBook event that touched this order. Lex-monotonic via `greatest(existing, new)` on OrderBook writes. Feeds `lastUpdateId` in depth responses as a STRING. |
| `chain_created_at` | `timestamptz` | On-chain block time of the originating `OrderBook.OrderPlaced`. Drives `time` in `/api/v1/prediction/orders`. Display-only. NULL for pre-migration rows. |
| `chain_updated_at` | `timestamptz` | On-chain block time of the most recent order book event that affected the order — OrderPlaced, OrderFilled, OrderCancelled. Advanced via greatest(...). Drives updateTime in /api/v1/prediction/orders. Display-only. NULL for pre-migration rows. |
| `placed_chain_order` | `text not null` | `msg_chain_order` of the `OrderPlaced` event that created the row. First-write-wins (`coalesce` on conflict). Sole sort key + cursor for `/api/v1/prediction/orders` (DESC). |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Index: `live_orders_open_book_idx` — partial index on `(orderbook_address, outcome_id, is_buy, price DESC)` with predicate `status = 'OPEN'`. Sized for the depth query: top-N price levels per side and outcome.

Index: `live_orders_owner_idx` — partial index on `(owner_pn_address, placed_chain_order DESC)`
with predicate `owner_pn_address IS NOT NULL AND chain_created_at IS NOT NULL`.

Serves as the seek path for the cursor-based `/api/v1/prediction/orders` query (DESC by chain-order): a single-
column lexicographic range scan over `placed_chain_order`. The partial predicate confines the index
to owner-attributed rows whose timestamps are renderable. Status filtering (`OPEN` vs `FILLED` vs
`CANCELLED` vs the future `REJECTED`) is intentionally a heap-side predicate so that one index
covers both the default "all statuses" query and any CSV-driven subset; per-owner row counts are
small enough that this is cheaper than maintaining a wider composite index.

The `chain_updated_at IS NOT NULL` condition stays in the SQL query as a heap filter, keeping the
index independent of a display-only timestamp column that advances on every `OrderFilled` event.

Cancel projection preserves [`amount_remaining`](#live_orders), so
`executedQty = amount_initial - amount_remaining` holds across cancellation.
Data-bearing cutover guidance lives in
[`orders-cancel-remainder-cutover.md`](../migrations/orders-cancel-remainder-cutover.md).

### `trades`

Append-only public trade tape backing `GET /api/v1/prediction/trades`. One row per maker↔taker
match, written by the `OrderBook.OrderFilled` projector on the **taker-side** event only
(`isTaker = true`) — the maker-side event mutates `live_orders` but writes no `trades`
row, so a match is recorded exactly once. Rows are immutable once written, except a
first-write-wins fill of a `NULL` `chain_time` on replay; never deleted. Write-side
derivation in [indexer.md](indexer.md#projection--public-trades); read side in
[read-api.md](read-api.md#apiv1predictiontrades).

| Column | Type | Notes |
| --- | --- | --- |
| `trade_id` | `text` PK | The taker-side `OrderFilled` event's chain-order key (`msg_chain_order` from the gateway, copied from [`raw_events.chain_order`](#raw_events)). Globally unique per match and lex-sortable — the sole sort key and identity for `/api/v1/prediction/trades` (DESC). The identical value is specified to surface as `orderUpdate`'s `t` field for the same fill ([api-spec.md](../api-spec.md#prediction-trades)). |
| `orderbook_address` | `text` NOT NULL | OrderBook contract address. With `outcome_id`, scopes the tape to one market outcome. |
| `outcome_id` | `integer` NOT NULL | Which outcome the match is on. |
| `price` | `numeric(78,0)` NOT NULL | Match (clearing) price from `OrderFilled.clearingPrice` — raw uint256 **basis points** (probability × `FULL_PERCENT` = 10 000). Decoded ÷ `FULL_PERCENT`, formatted at `price_precision`, at API render. |
| `qty` | `numeric(78,0)` NOT NULL | Matched quantity from `OrderFilled.filledAmount` — raw **token atoms** (× `10^decimals`). Decoded ÷ `10^decimals`, formatted at `quantity_precision`. `quoteQty` is derived at render as `price * qty / FULL_PERCENT` (the contract's integer-division notional), not stored. |
| `is_buyer_maker` | `boolean` NOT NULL | Trade direction, derived from the taker order's side: taker selling ⇒ the buyer is the maker ⇒ `true`; taker buying ⇒ `false`. Surfaces as `isBuyerMaker`. |
| `chain_time` | `timestamptz` | On-chain block time of the taker `OrderFilled` event ([`raw_events.created_at_chain`](#raw_events)). Drives `time` (Unix ms) in trade responses. NULL when the gateway omitted `created_at`; such rows are filtered out of the read query, matching `live_orders` / `/api/v1/prediction/orders`. |
| `created_at` | `timestamptz` | Bookkeeping (indexer ingestion wall-clock). |

Index: `trades_tape_idx` — `(orderbook_address, outcome_id, trade_id DESC)`. Backs the newest-first per-outcome read (`ORDER BY trade_id DESC LIMIT $limit`) as an index range scan. `trades` is insert-only; a replayed insert conflicts on `trade_id` and only coalesces a `NULL` `chain_time` (first-write-wins), so reprojection from `raw_events` is idempotent.

Recovery notes for on-call:

- **Row hidden by `chain_time IS NULL`** (gateway delivered the fill without `created_at`): fix the `trades` row directly — `UPDATE trades SET chain_time = to_timestamp(...) WHERE trade_id = ...`, sourcing the timestamp from the repaired `raw_events.created_at_chain`. Do **not** clear the event's `processed_at` while its order is still live: replay re-runs the whole `OrderFilled` projection, and the `live_orders` fill arm is not replay-idempotent (`filledAmount` would be subtracted again — see `reproject_pending`'s doc). A replay-based heal via the conflict-arm coalesce is safe only when the order row is already terminal, or during a wholesale reprojection that rebuilds `live_orders` from scratch.
- **Tape 503s for one outcome** (`MarketInconsistent` from an undecodable `price`/`qty`): the table is append-only and the read is newest-first, so one corrupt row inside the newest `limit` rows fails every request for that `(orderbook_address, outcome_id)` until it is fixed. The failing `trade_id`/axis/raw value is logged by the read path; verify against the originating `raw_events` row, then correct or delete the corrupt `trades` row manually.

### `order_book_snapshots`

Reserved table for cached depth snapshots. Not used by the current depth handler — `/api/v1/prediction/depth` aggregates `live_orders` on every request. Kept in the schema for a future cache-warming path; safe to ignore until then.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | |
| `symbol` | `text` UNIQUE | Outcome symbol. |
| `orderbook_address` | `text` | |
| `last_update_id` | `bigint` | |
| `bids_jsonb` / `asks_jsonb` | `jsonb` default `'[]'::jsonb` | |
| `updated_at` | `timestamptz` | |

## Read-model — inference markets

The inference side tracks the per-model order books of the private-inference market (`contracts/airegistry/InferenceOrderBook.sol` — one book per model) and the resting orders inside them. These tables back `/api/v1/inference/markets` (list, plus single-market via `?inferenceOrderBookAddress=`) and `/api/v1/inference/depth` (order book). Inference-settled **prediction** markets add no table of their own — `/api/v1/prediction/markets?resolvesFrom=` reuses [`markets`](#prediction-markets) joined to the range-event columns on [`oracle_events`](#oracle_events) (`range_ob_address`). As on the prediction-market side, a row is hidden from the public API until the inference reconciler stamps `last_reconciled_at`.

Both tables are created by migration `0005_inference_orderbook.sql`.

### `inference_markets`

One row per `InferenceOrderBook` contract observed on chain — equivalently, one tradable model. The book's address is derived from the model identity alone (`DexLib.computeInferenceOrderBookAddress(code, modelHash)` — one book per `_modelHash`, the tick-size component was dropped), so the address and `model_hash` are 1:1. Discovered from the first order event on an unknown book address (see [indexer.md](indexer.md#projection--inference-order-events)), completed by the inference reconciler reading `InferenceOrderBook.getParams()` and `getWeeklyMedianPrice()`.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal FK target. |
| `orderbook_address` | `text` UNIQUE | The InferenceOrderBook contract address. Exposed as `inferenceOrderBookAddress` — the public market id. |
| `model_hash` | `numeric(78,0)` UNIQUE | On-chain model identity (`_modelHash` static), from `getParams()`. The only model identifier the order book itself carries. NULL only during the pre-reconcile window; the visibility gate guarantees it is set on every market the API returns. |
| `model_ref` | `text` (nullable) | 🚧 Human-readable model id `producer--model--version`. Reconciler-filled from the model's `ManifestMetadata` manifest (the order book carries only the hash). NULL when the manifest is not yet indexed or carries no model id — the API then surfaces the model by hash alone. Resolution from the model registry is deferred. |
| `producer` / `model_name` / `version` | `text` (nullable) | 🚧 Parsed components of `model_ref`, for the `model.{producer,name,version}` render. Filled together with `model_ref`; NULL until `model_ref` is resolved. |
| `manifest_address` | `text` (nullable) | 🚧 The model's `ManifestMetadata` contract address — the reconcile source for `model_ref`. NULL until linked. |
| `root_model_address` | `text` (nullable) | 🚧 The model's `RootModel` address. Diagnostic / reconcile aid. NULL until linked. |
| `owner_pubkey` | `numeric(78,0)` (nullable) | 🚧 Model-owner pubkey (`RootModel` / `ManifestMetadata.getOwnerPubkey()`). NULL until resolved. Note: `buyerPubkey` is present on-chain but is intentionally not stored — there is no per-order ownership column on `inference_orders`. |
| `platform_fee_bps` | `integer` | Platform fee in basis points (`getParams().platformFeeBps`, e.g. `250`). Filled by the inference reconciler on the first discovery pass from `InferenceOrderBook.getParams()`. Renders the buyer-side `takerCommission` (÷ 10 000 → `"0.025"`). The seller-side `makerCommission` is the rebate cap `−REBATE_MAX_BPS` (`−0.02`), a protocol constant; like `/api/v1/prediction/markets`, commissions are rendered (not stored per-row). |
| `quote_token_type` | `integer` FK → `ref_tokens(token_type)` | Quote asset of the book. Reconciler sets this to SHELL (`token_type = 2`) as a **constant** on the discovery pass — it is not sourced from a getter field. |
| `price_precision` | `integer` | Decimal places for price-per-tick at API render. Reconciler sets this to the **constant** `9` (SHELL `decimals`) on the discovery pass. |
| `quantity_precision` | `integer` | Decimal places for tick quantity. Reconciler sets this to the **constant** `0` (ticks are integer units) on the discovery pass. |
| `tick_size` | `text` | Minimum price-per-tick increment as a decimal string. Reconciler sets this to the **constant** `"0.000000001"` (1 SHELL atom) on the discovery pass. |
| `step_size` | `text` | Minimum tick-quantity increment as a decimal string. Reconciler sets this to the **constant** `"1"` on the discovery pass. |
| `min_notional` | `text` | Minimum order notional as a decimal string. Reconciler sets this to the **constant** `"0.000000001"` on the discovery pass. |
| `reference_price` | `numeric(78,0)` (nullable) | Weekly-median reference price in SHELL atoms (`getWeeklyMedianPrice()`). Reconciler-filled on the discovery pass and re-fetched on the reference-price refresh cadence. **NULL when the book is dry** — the getter reverts `ERR_NO_LIQUIDITY` on insufficient volume, the reconciler records NULL, and the API surfaces `referencePrice: null`. |
| `reference_price_at` | `timestamptz` (nullable) | When `reference_price` was last refreshed. |
| `created_at_chain` | `timestamptz` (nullable) | On-chain block time the book was first observed (from the seed event's `created_at_chain`). Drives `createdAt`. |
| `last_reconciled_at` | `timestamptz` | Stamped by the inference reconciler after a successful discovery pass. The public API filters on `last_reconciled_at IS NOT NULL` — books without this are invisible to clients (mirrors [`markets`](#prediction-markets)). |
| `last_reconcile_failed_at` | `timestamptz` | Backoff bookkeeping for the inference reconciler. |
| `reconcile_attempts` | `integer` default `0` | Diagnostic counter. |
| `last_swept_at` | `timestamptz` (nullable) | Stamped `now()` on every sweep batch tick that passes the catch-up gates — not only on cycle completion. It drives the Queue B sweep cadence (`now() - last_swept_at >= inference_sweep_interval_ms`), so it must advance each tick. NULL until the first sweep. Used with `inference_markets_sweep_idx` to drive Queue B sweep scheduling. |
| `sweep_cursor` | `numeric(78,0)` (nullable) | The `order_id` cursor for the current bounded round-robin sweep of `OPEN` orders. NULL at cycle start (or after a reset). The sweep resumes from this cursor on the next tick; NULL means start from the lowest `order_id`. |
| `sweep_cycle_max` | `numeric(78,0)` (nullable) | Snapshot of the highest `order_id` at sweep-cycle start. Newly-minted orders above this bound are deferred to the next cycle — they cannot be phantoms yet when the book just accepted them. NULL when no cycle is in progress. |
| `sweep_override_seq` | `bigint` NOT NULL default `0` | Monotonic counter bumped whenever a `Filled` event overrides a provisionally sweep-cancelled order while the book is still in the discovery phase. The discovery visibility stamp (transition to `last_reconciled_at IS NOT NULL`) is CAS-guarded on this counter being unchanged across the completing sweep tick, preventing a premature stamp when an override reset `sweep_cursor` to NULL at the start of a cycle where a plain cursor-CAS cannot distinguish reset-from-NULL from a normal start-of-cycle-NULL. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `inference_markets_model_hash_idx` (partial UNIQUE: `model_hash IS NOT NULL`) | Lookup / dedup by on-chain model identity. Partial to tolerate NULL during the pre-reconcile window. |
| `inference_markets_pending_reconcile_idx` (partial: `last_reconciled_at IS NULL`) | Drives the inference reconciler Queue A (discovery) SELECT, ordered by `last_reconcile_failed_at NULLS FIRST, id`. |
| `inference_markets_refresh_idx` (partial: `last_reconciled_at IS NOT NULL`) | Drives the inference reconciler Queue B (reference-price refresh) SELECT, ordered by `reference_price_at NULLS FIRST`. |
| `inference_markets_sweep_idx` (partial: `last_reconciled_at IS NOT NULL`) | Drives the inference reconciler Queue B sweep scheduling, ordered by `last_swept_at NULLS FIRST`. |

`reference_price` re-queues independently of the discovery reconcile: it moves with trading, so the reconciler refreshes it on a cadence (it is not a write-once field like `markets.orderbook_address`). See [indexer.md](indexer.md#inference-reconciler).

### `inference_orders`

Per-order read model backing `/api/v1/inference/depth` (order-book depth). One row per chain-side order on an `InferenceOrderBook`, mutated in place as `OrderPlaced`, `Filled`, `OrderCancelled`, `Refunded`, and `SubscriptionPlaced` events arrive. Mirrors [`live_orders`](#live_orders): rows are never deleted, so `FILLED` / `CANCELLED` entries remain and the depth handler (`max(last_chain_order)` across **all** rows for the book) still sees them. This version exposes no account-scoped inference order endpoint, so no ownership / private-read columns are required.

| Column | Type | Notes |
| --- | --- | --- |
| `orderbook_address` | `text` (PK part 1) | InferenceOrderBook contract address. |
| `order_id` | `numeric(78,0)` (PK part 2) | Chain-side order id (`OrderPlaced.orderId`). The pair `(orderbook_address, order_id)` is the primary key. |
| `is_buy` | `boolean` | Side. `true` = bid (buy order / subscription), `false` = ask (sell offer). |
| `price` | `numeric(78,0)` | Price per tick `P` in SHELL atoms, as emitted (`OrderPlaced.price`). BUY = max price per tick; SELL = offer price. Pure taker orders (IOC / FOK / MARKET) never rest, so they produce no OPEN row. Decoded ÷ `10^9` at render, formatted at `price_precision`. |
| `amount_initial` | `numeric(78,0)` | Original tick count (`OrderPlaced.ticks`). |
| `amount_remaining` | `numeric(78,0)` | Ticks not yet filled. Set by `OrderPlaced`, decremented by `Filled`. `OrderCancelled` preserves the current value; depth ignores the row because `status != 'OPEN'`. |
| `is_subscription` | `boolean` default `false` | `true` when the resting buy came from `SubscriptionPlaced` (spec §8 — a standing bid throttled by a weekly budget). Rests in the book like any other bid; flagged for diagnostics. |
| `status` | `text` CHECK `IN ('OPEN','FILLED','CANCELLED')` | Order lifecycle. Depth aggregation filters on `status = 'OPEN' AND amount_remaining > 0`. A SELL offer is a one-deal slot consumed on match; a BUY maker reduces across fills. |
| `swept_at` | `timestamptz` (nullable) | Stamped by the reconciler sweep when `getOrder()` confirms the order is no longer in the book and the row is provisionally cancelled. NULL while the order has not yet been swept. |
| `note_address` | `text` (nullable) | Owner note address (`OrderPlaced.note`). Not on the public hot path; kept for diagnostics and cancel attribution. |
| `last_chain_order` | `text` NOT NULL | Chain-order key of the most recent book event that touched this order. Lex-monotonic via `greatest(existing, new)`. Feeds `lastUpdateId` in depth responses as a STRING. |
| `chain_created_at` / `chain_updated_at` | `timestamptz` | On-chain block times of the originating `OrderPlaced` and the most recent touch. Display / diagnostic only. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `inference_orders_open_book_idx` (partial: `status = 'OPEN'`) | `(orderbook_address, is_buy, price DESC)`. Sized for the depth query: top-N price levels per side for one book. |
| `inference_orders_sweep_idx` (partial: `status = 'OPEN'`) | `(orderbook_address, order_id)`. Backs the reconciler's bounded round-robin sweep SELECT over OPEN rows, keyed by book + cursor position. |

## Read-model — inference deals

The inference settlement side tracks the lifecycle of each deal escrow (`TokenContract` — a per-deal streaming-payment contract auto-deployed when a SELL offer is matched) and the individual finalized ticks within it. These tables are written by the SETTLEMENT projector and are intended to back the forthcoming rewards service as its primary read-model. Both tables are created by migration `0007_inference_deals.sql`.

### `inference_deals`

One row per `TokenContract` address. Seeded as a skeleton from the first observed `TokenContract.*` event (keyed by `src_address`); remaining columns filled by the SETTLEMENT projector as `InferenceOrderBook.Filled`, `TokenContract.StreamOpened`, and the stream-close events (`StreamStopped`/`DisputeResolved`/`StreamReclaimed`/`ContractDestroyed`), and related events arrive.

| Column | Type | Notes |
| --- | --- | --- |
| `token_contract_address` | `text` PK | Address of the `TokenContract` escrow deployed when a SELL order is matched. The per-deal identifier. |
| `orderbook_address` | `text` (nullable) | The `InferenceOrderBook` address that matched this deal. Filled from `InferenceOrderBook.Filled`. |
| `seller_note` | `text` (nullable) | PrivateNote address of the seller. Filled from `InferenceOrderBook.Filled`. |
| `buyer_note` | `text` (nullable) | PrivateNote address of the buyer. Filled from `InferenceOrderBook.Filled`. |
| `deposit` | `numeric(78,0)` (nullable) | Initial deposit amount (quote token units). |
| `price_per_tick` | `numeric(78,0)` (nullable) | Agreed price per finalized tick (quote token units). |
| `finalized_ticks` | `integer` NOT NULL default `0` | Running count of finalized ticks. Incremented by the SETTLEMENT projector on each `TokenContract.TickFinalized` event. |
| `finalized_owed_total` | `numeric(78,0)` NOT NULL default `0` | Cumulative owed amount across all finalized ticks. |
| `funded_at_chain` | `timestamptz` (nullable) | Chain timestamp of the funding event. |
| `opened_at_chain` | `timestamptz` (nullable) | Chain timestamp when the deal was opened. |
| `settled_at_chain` | `timestamptz` (nullable) | Chain timestamp when the deal closed cleanly or was resolved. |
| `close_kind` | `text` (nullable) | Terminal close type: one of `STOPPED`, `DISPUTE_RESOLVED`, `RECLAIMED`, `DESTROYED`. Enforced by a CHECK constraint. |
| `clean_settlement` | `boolean` (nullable) | `true` if the deal closed without a dispute; `false` or `null` otherwise. |
| `disputed_at_chain` | `timestamptz` (nullable) | Chain timestamp of the dispute event, if any. |
| `last_chain_order` | `text` (nullable) | The `chain_order` of the most recent `InferenceOrderBook.Filled` cross-link that wrote this row. Advanced via `greatest(existing, new)` by the `Filled` handler; not read or advanced by any `TokenContract` handler. |
| `created_at` / `updated_at` | `timestamptz` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `inference_deals_orderbook_idx` | Lookup all deals for a given `InferenceOrderBook`. |
| `inference_deals_seller_idx` | Lookup all deals by seller PrivateNote — sized for per-seller aggregation queries (e.g. by the forthcoming rewards service). |
| `inference_deals_buyer_idx` | Lookup all deals by buyer PrivateNote. |

### `inference_ticks`

One row per finalized tick within a deal. Written by the SETTLEMENT projector on each `TokenContract.TickFinalized` event. The composite PK `(token_contract_address, chain_order)` is idempotent against redelivery.

| Column | Type | Notes |
| --- | --- | --- |
| `token_contract_address` | `text` NOT NULL (part of PK) FK → `inference_deals(token_contract_address)` ON DELETE CASCADE | Parent deal's `TokenContract` address. The projector always inserts the parent `inference_deals` row before any tick insert, so the FK is always satisfiable at runtime. `ON DELETE CASCADE` makes test cleanup order-independent. |
| `chain_order` | `text` NOT NULL (part of PK) | The `chain_order` of the `TickFinalized` event. Uniquely identifies each tick within a deal. |
| `finalized_owed` | `numeric(78,0)` NOT NULL | Amount owed for this tick (quote token units). |
| `deposit` | `numeric(78,0)` NOT NULL | Deposit snapshot at tick finalization. |
| `chain_at` | `timestamptz` (nullable) | Chain timestamp of the finalization event. |
| `created_at` | `timestamptz` NOT NULL default `now()` | Bookkeeping. |

Indices:

| Index | Purpose |
| --- | --- |
| `inference_ticks_tc_idx` | Fetch all ticks for a deal by `token_contract_address`. |

## Authentication and credentials

Identity and credential storage for the auth middleware. See [auth.md](./auth.md) for the user model, request-verification pipeline, and error mapping.

### `accounts`

One row per logical user. Holds the custodied trading PrivateNote inline; multiple PNs per account are not supported in this version and replacing the PN is operator-only via direct UPDATE on this row.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `uuid` PK default `gen_random_uuid()` | Stable `accountId` surfaced to clients. The only identifier that crosses the API boundary. |
| `label` | `text` (nullable) | Operator-facing label. Not exposed by the API. |
| `pn_address` | `text` UNIQUE | Address of the trading PrivateNote bound to this account. Source of balances for `GET /api/v1/account` and `GET /api/v1/account/balances`. |
| `pn_pubkey` | `numeric(78, 0)` | PN signing pubkey. |
| `pn_seckey_enc` | `bytea` | PN signing seckey, encrypted at rest under the backend master key (`crates/infrastructure/src/crypto.rs`). Never read by the API; used by the trading path to submit transactions. |
| `pn_dih` | `numeric(78, 0)` UNIQUE | Deploy-init hash of the PN. Disambiguates PNs that may share an address across redeploys. |
| `disabled_at` | `timestamptz` (nullable) | Soft-disable marker. NULL = active. |
| `created_at` | `timestamptz` default `now()` | Bookkeeping. |

### `api_keys`

API credential pairs. Multiple per account, each with its own permission set. The api_secret is generated at issuance and only the ciphertext is stored; the cleartext is shown to the operator once and cannot be recovered later.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial` PK | Internal identifier. Never surfaced. |
| `account_id` | `uuid` FK → `accounts(id)` ON DELETE CASCADE | Owning account. |
| `api_key` | `text` | Public half of the credential pair; sent by clients in the `X-DODEX-APIKEY` header. |
| `api_secret_enc` | `bytea` | Encrypted api_secret. Decrypted in-process to recompute the request HMAC. |
| `permissions` | `auth_permission[]` default `{USER_DATA}` | Subset of the `auth_permission` enum (`USER_DATA`, `TRADE`). Endpoints declare a required permission; auth rejects with `-1002` if the key lacks it. |
| `disabled_at` | `timestamptz` (nullable) | Soft-disable marker. Disabled keys are rejected with `-1002`. NULL = active. |
| `last_used_at` | `timestamptz` (nullable) | Stamped by the auth middleware on successful verification. Used for operator audits and stale-key cleanup. |
| `created_at` | `timestamptz` default `now()` | Bookkeeping. |

Indices:

- `api_keys_api_key_active_idx` — UNIQUE partial index on `(api_key) WHERE disabled_at IS NULL`. Lets the auth middleware look up an active credential by `api_key` in O(1) without colliding with historical disabled rows that may have reused the same string (irrelevant in practice with 256-bit random keys, but the partial predicate captures the exact invariant).
- `api_keys_account_id_idx` — supports operator queries that list all keys under an account.

## System tables

`_sqlx_migrations` is created and maintained by `sqlx::migrate!`. It records which migration files have been applied. Do not touch it in application code.

## Schema evolution

Every schema change ships as a new numbered migration file. Conventions:

- Use `if not exists` / `if exists` on DDL so re-runs are idempotent.
- For new columns, prefer `add column if not exists` with a sensible default — never break startup on an empty database.
- Partial indices are preferred over full ones for "pending row" predicates; they shrink with reconciliation progress.
- Add a header comment on every migration explaining *why* the change is needed and which code path requires it. Migrations are read by reviewers and operators as much as the code is.
- Pre-deploy check: index builds inside sqlx transactions can block writers until the build finishes. For hot tables, estimate the lock window from production row counts before deploying or run the change through a non-transactional migration path.

The full migration set (`migrations/*.sql`) is the canonical reference; this document summarises intent but does not replace it.
