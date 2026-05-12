# Market Data API Technical Specification

Implementation-facing requirements for the HTTP layer that serves the market-data read-model. The public contract (URLs, field names, parameter rules, error shapes, response examples) lives in [api-spec.md](../api-spec.md). Postgres tables referenced below are documented column-by-column in [data-schema.md](data-schema.md). The write side — how those tables get populated — is in [market-data-indexer.md](market-data-indexer.md).

## Glossary

**Read-model** — Postgres tables prepared for API reads. The indexer builds these tables from chain events and contract state; the API reads them instead of querying contracts directly.

**Market row** — one row in the `markets` table. It represents one PMP contract and is the main source for `/api/v1/markets`.

**Reconciled market** — a market row with `last_reconciled_at IS NOT NULL`. This means the market reconciler has already read the PMP state and filled the fields required for public responses. `/api/v1/markets` hides markets until this is true.

**Lifecycle status** — the public market phase returned as `status`: `PENDING`, `UPCOMING`, `STAKING`, `AWAITING_FREEZE`, `TRADING`, `RESOLVING`, `RESOLVED`, `CANCELLED`, or `EXPIRED`. It is computed by the API from the market row and current request time; it is not stored as a separate database column.

**serverTime** — the unix-seconds timestamp captured once at the start of a `/api/v1/markets` request. The API returns it in the response and uses the same value to compute lifecycle status.

**Depth** — the `/api/v1/depth` response for one market outcome: sorted bid and ask price levels plus `lastUpdateId`. It is built from `live_orders`, not by querying the OrderBook contract during the HTTP request.

**DTO** — Data Transfer Object. In this document it means the API response object after the backend has assembled it from database rows, but before it is serialized to JSON and sent to the client.

## Market identity

The backend treats `marketAddress` as the PMP address. `orderBookAddress` is the deterministic address returned by `PMP.getOrderBookAddress()` and is stamped on the first successful reconciler pass — pre-`PoolsFrozen` rows already carry it (migration 0014 enforces the invariant `last_reconciled_at IS NOT NULL ⇒ orderbook_address IS NOT NULL` via a CHECK constraint). The pre-reconcile window between `PMPDeployed` and the first reconciler pass is the only state where the column is legitimately null, and such rows are hidden from the API by the `last_reconciled_at IS NOT NULL` visibility filter. The write-side flow is described in [market-data-indexer.md](market-data-indexer.md#market-reconciler). Clients MUST use `status` to determine whether the order book is currently available for trading — a non-null `orderBookAddress` does not by itself imply the book is open.

## `/api/v1/markets`

Lifecycle status is not stored as a separate database column. The API computes it for each request from the indexed market row and a single unix-seconds `now` value. The same `now` is returned as `serverTime` and used for status calculation, so one response cannot mix timestamps from both sides of a lifecycle boundary.

### Visibility filter

The SQL query behind `GET /api/v1/markets` includes `WHERE m.last_reconciled_at IS NOT NULL`. Markets that the indexer has discovered (the `PMPDeployed` event arrived) but not yet reconciled are hidden — clients only see markets the backend can describe fully. See [market-data-indexer.md](market-data-indexer.md#visibility-gate) for the symmetric write-side rule.

### Status derivation

Source: a row in [`markets`](data-schema.md#markets) plus the request `now`. Order of checks (terminal events take precedence over time-derived phases):

1. `cancelled_at IS NOT NULL` OR `is_cancelled` → `CANCELLED`.
2. `resolved_at IS NOT NULL` → `RESOLVED`.
3. `stake_start IS NULL` → `PENDING`.
4. `frozen_at IS NULL`:
   - `now ≥ stake_end` → `AWAITING_FREEZE` (indefinitely, no upper bound on `now`).
   - `now ≥ stake_start` → `STAKING`.
   - Otherwise → `UPCOMING`.
5. `frozen_at IS NOT NULL`:
   - `now ≥ result_end` → `EXPIRED`.
   - `now ≥ result_start` → `RESOLVING`.
   - Otherwise → `TRADING`.

The same logic is mirrored in the SQL `STATUS_CASE` used by the `?status=` filter pushdown, so the SQL filter cannot drift from the Rust-side derivation.

### Building the response

For each row in the page, the API:

1. Derives `status` as above.
2. Builds `timings` from the four timing columns — returns `null` only when at least one is missing (PENDING).
3. Builds `terminal` (with `kind`, `at`, `resolvedOutcomeId`, `cancelReason`) for terminal statuses, `null` otherwise.
4. Joins [`market_outcomes`](data-schema.md#market_outcomes) for the outcomes array, including per-outcome `pricePrecision`, `tickSize`, `stepSize`, `minNotional`, `maxBatchSize`.
5. Fetches the `event.*` block in a separate batch (`fetch_oracle_events`) joined across [`oracle_events`](data-schema.md#oracle_events) ⨝ [`oracle_event_lists`](data-schema.md#oracle_event_lists) ⨝ [`oracles`](data-schema.md#oracles) for every `pmp_address` on the page. A PMP can be confirmed by multiple `OracleEventList` contracts (`PrivateNote.PMPDeployed.oracleEventLists: address[]`), producing N rows here; the API collapses them into one `event.oracles[]` array. Joining `oracle_events` directly into the main markets SELECT would have multiplied the market row by N, inflating `has_more`/cursor and emitting duplicate listings.

`description` and other reconciler-only fields rely on data filled by the OracleEventList reconciler — they may be null briefly after a market is discovered but before the reconciler-side metadata lands. `eventName`/`description` are derived from `eventId = hash(eventName, description, deadline, outcomeNames)`, so every confirmation row for the same `pmp_address` must agree on those values; `aggregate_oracle_events` validates this cross-row equality and fails closed (`MarketInconsistent`) on mismatch.

### Pagination

Two sort modes:

- `sort=resultStart` (default, ascending) — sort key is `coalesce(result_start, +∞)` so PENDING / UPCOMING rows without a resolved `result_start` sort to the end.
- `sort=createdAt` (descending) — sort key is `created_at_micros` (microsecond precision). Sub-second keying avoids the keyset bug where two markets created in the same second could be skipped or duplicated across page boundaries.

Cursor format: URL-safe base64 of `"<sort_key>:<id>"`. The handler decodes and validates the cursor; a corrupted cursor surfaces as `DomainError::InvalidParameter` → HTTP 400, not as an internal error.

### Fail-closed validation

After building the DTO, the API checks the assembled shape against spec invariants. Any violation surfaces as `DomainError::MarketInconsistent` → HTTP 503. The 503 status is deliberate: the inconsistency is transient (the indexer is mid-replay), and the client should retry rather than treat the market as permanently broken. The checks live in `postgres_repo.rs::validate_invariants`:

| Rule | Source |
| --- | --- |
| `timings` is null exactly when status is PENDING | [api-spec Timings](../api-spec.md#timings): "`timings` itself is `null` only for `PENDING`." |
| `terminal` is non-null exactly when status is RESOLVED, CANCELLED, or EXPIRED | [api-spec Terminal](../api-spec.md#terminal) |
| RESOLVED requires `frozen_at`, kind=RESOLVED, **`resolvedOutcomeId`** set | [api-spec Terminal](../api-spec.md#terminal) ("without it the client cannot know which side won") |
| CANCELLED requires kind=CANCELLED and a **valid** `cancelReason` (PMP_CANCELLED or EVENT_CANCELLED) | [api-spec Terminal](../api-spec.md#terminal): cancelReason must distinguish source |
| EXPIRED requires kind=EXPIRED | spec consistency |
| TRADING / RESOLVING require `frozen_at` | spec consistency with `frozenAt != null` for post-freeze statuses |
| `event.eventName` / `event.description` agree across every confirming oracle for one market | Hash invariant `eventId = hash(eventName, description, deadline, outcomeNames)` on chain. Enforced by `aggregate_oracle_events` in `postgres_repo.rs`. |

The validation works on the *built* DTO rather than the raw row so that downstream silent-elision bugs are caught — for example, an unknown `cancel_reason` string would be parsed to `None` and serialized as `cancelReason: null`; the validator rejects the assembled DTO instead of the raw column being non-null.

The matching write-side rules are in [market-data-indexer.md](market-data-indexer.md#schema-invariants---write-side).

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Market not found / not yet reconciled | `InvalidMarketOrSymbol` | 404 |
| Invalid `status` / `sort` enum value | `InvalidParameter` | 400 |
| Mutually exclusive params (`marketAddress` together with list filters) | `MissingParameter` | 400 |
| Corrupted cursor | `InvalidParameter` (from cursor decode) | 400 |
| Invariant violation on built DTO | `MarketInconsistent` | 503 |

## `/api/v1/depth`

Returns the top of the order book for one outcome of one market: a snapshot of resting bids and asks, the quantity available at each price level, and a sequence number the client uses to tell whether the snapshot has moved since the previous response. The endpoint never queries the contract at request time — every level shown is the projection of indexed `OrderBook` events into a per-order read-model (see [market-data-indexer.md](market-data-indexer.md#projection--order-events)).

### Resolution

Resolve `(marketAddress, symbol)` to `(orderbook_address, outcome_id, price_precision, quantity_precision)` via [`markets`](data-schema.md#markets) joined with [`market_outcomes`](data-schema.md#market_outcomes). The market must already be reconciled at least once (`last_reconciled_at IS NOT NULL`); otherwise the endpoint returns `InvalidMarketOrSymbol` → 404. The symbol identifies one of the market's outcomes — depth is per-outcome, not per-market.

### Empty-book contract

A reconciled market always has an `orderbook_address`, so an empty book means exactly one thing: no `OrderBook.OrderPlaced` events have landed for `(orderbook_address, outcome_id)` yet. The response is structurally well-formed — empty `bids`, empty `asks`, `lastUpdateId = 0` — and is the steady-state shape for a market that has not yet started trading. Clients can poll cheaply while the market is warming up. A NULL or blank `orderbook_address` on a reconciled row is treated as `MarketInconsistent` (HTTP 503), not silently served as an empty book.

### Aggregation

The API issues one SQL query that produces both sides of the book in a single round trip. Per side, the database:

1. Filters [`live_orders`](data-schema.md#live_orders) to `status = 'OPEN' AND amount_remaining > 0` scoped to this `(orderbook_address, outcome_id)`.
2. Groups by `price`, sums `amount_remaining` — multiple resting orders at one price collapse into a single level. Clients see "quantity available at this price", not the underlying orders.
3. Orders by price (bids descending, asks ascending) and applies `LIMIT $limit`.

Postgres applies the sort and `LIMIT` while reading, so the API receives only the top N price levels per side instead of loading the full open book into memory. The partial index `live_orders_open_book_idx` (`WHERE status = 'OPEN'`) is designed for this depth query.

After the database returns, each side is re-sorted in Rust using exact-numeric `BigUint` comparison — lexicographic string comparison would silently misrank prices of different lengths (`"100" < "99"` lexicographically). Each `[price, quantity]` is then scaled to a fixed-point decimal using the outcome's `price_precision` and `quantity_precision`. The result matches the `[price, quantity]` shape in [api-spec.md](../api-spec.md).

### `lastUpdateId`

`max(live_orders.last_chain_order)` over rows for this `(orderbook_address, outcome_id)` pair. `last_chain_order` is the lex-sortable chain-order string (`msg_chain_order` from the GraphQL gateway) of the most recent event that touched the row; the public `lastUpdateId` is therefore a STRING, not an integer (see [api-spec.md §Order Book](../api-spec.md#order-book)). The per-outcome scope is intentional: a single OrderBook serves multiple outcomes, and a per-orderbook cursor would let a quiet outcome inherit activity from sibling outcomes.

Empty string means no OrderBook event has touched this pair yet. The value never lex-decreases between successive snapshots — `last_chain_order` is updated via `greatest(existing, new)` on the write side, and the reproject loop applies events in `chain_order` so the natural arrival order is already monotonic (see [market-data-indexer.md](market-data-indexer.md#projection--order-events)).

### Invariants

1. `bids` sorted by price descending; `asks` ascending. Comparison is exact-numeric.
2. Each price level surfaces as one `[price, quantity]` entry. Quantity is the sum across every resting order at that price.
3. `lastUpdateId` is scoped to `(orderbook_address, outcome_id)`. It is an empty string when no OrderBook event has touched this pair yet, and never lex-decreases between successive snapshots.
4. A non-null `orderBookAddress` on the underlying market is necessary for non-empty depth but not sufficient — orders only land after the `PoolsFrozen` event is observed and clients start posting.

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Market unknown or pre-reconcile | `InvalidMarketOrSymbol` | 404 |
| Reconciled market with NULL/blank `orderbook_address` | `MarketInconsistent` | 503 |
| Missing `marketAddress` or `symbol` | `MissingParameter` | 400 |
| Invalid `limit` (non-numeric) | `InvalidParameter` | 400 |
