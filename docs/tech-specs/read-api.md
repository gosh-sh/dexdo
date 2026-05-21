# Market Data API Technical Specification

Implementation-facing requirements for the HTTP layer that serves the market-data read-model. The public contract (URLs, field names, parameter rules, error shapes, response examples) lives in [api-spec.md](../api-spec.md). Postgres tables referenced below are documented column-by-column in [data-schema.md](data-schema.md). The write side — how those tables get populated — is in [indexer.md](indexer.md).

## Glossary

**Read-model** — Postgres tables prepared for API reads. The indexer builds these tables from chain events and contract state; the API reads them instead of querying contracts directly.

**Owner attribution** — the binding between a chain-side order (`orderbook_address`, `order_id`) and the trading PrivateNote address that placed it. `OrderBook.OrderPlaced` does not carry the owner; attribution arrives separately via `PrivateNote.OrderPlacedConfirmed` and is stored in `live_orders.owner_pn_address`.

**Trading PN** — the trading PrivateNote address of an authenticated account. Resolved from the API key by the existing auth hoop and exposed as `ctx.trading_pn.pn_address`.

**Chain time** — `raw_events.created_at_chain` of the event that produced a state transition. Used for response `time` and `updateTime` so they are stable under indexer backlog.

**Market row** — one row in the `markets` table. It represents one PMP contract and is the main source for `/api/v1/markets`.

**Reconciled market** — a market row with `last_reconciled_at IS NOT NULL`. This means the market reconciler has already read the PMP state and filled the fields required for public responses. `/api/v1/markets` hides markets until this is true.

**Lifecycle status** — the public market phase returned as `status`: `PENDING`, `UPCOMING`, `STAKING`, `AWAITING_FREEZE`, `TRADING`, `RESOLVING`, `RESOLVED`, `CANCELLED`, or `EXPIRED`. It is computed by the API from the market row and current request time; it is not stored as a separate database column.

**serverTime** — the unix-seconds timestamp captured once at the start of a `/api/v1/markets` request. The API returns it in the response and uses the same value to compute lifecycle status.

**Depth** — the `/api/v1/depth` response for one market outcome: sorted bid and ask price levels plus `lastUpdateId`. It is built from `live_orders`, not by querying the OrderBook contract during the HTTP request.

**DTO** — Data Transfer Object. In this document it means the API response object after the backend has assembled it from database rows, but before it is serialized to JSON and sent to the client.

## Market identity

The backend treats `marketAddress` as the PMP address. `orderBookAddress` is the deterministic address returned by `PMP.getOrderBookAddress()` and is stamped on the first successful reconciler pass — pre-`PoolsFrozen` rows already carry it. The pre-reconcile window between `PMPDeployed` and the first reconciler pass is the only state where the column is legitimately null, and such rows are hidden from the API by the `last_reconciled_at IS NOT NULL` visibility filter. The write-side flow is described in [indexer.md](indexer.md#market-reconciler). Clients MUST use `status` to determine whether the order book is currently available for trading — a non-null `orderBookAddress` does not by itself imply the book is open.

## `/api/v1/markets`

Lifecycle status is not stored as a separate database column. The API computes it for each request from the indexed market row and a single unix-seconds `now` value. The same `now` is returned as `serverTime` and used for status calculation, so one response cannot mix timestamps from both sides of a lifecycle boundary.

### Visibility filter

The SQL query behind `GET /api/v1/markets` includes `WHERE m.last_reconciled_at IS NOT NULL`. Markets that the indexer has discovered (the `PMPDeployed` event arrived) but not yet reconciled are hidden — clients only see markets the backend can describe fully. See [indexer.md](indexer.md#visibility-gate) for the symmetric write-side rule.

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
| `orderbook_address` is non-blank on every reconciled market | DB schema CHECK pins NOT NULL; `assemble_market` rejects whitespace-only strings that slip past the CHECK so listing/single-market match the depth contract. |

The validation works on the *built* DTO rather than the raw row so that downstream silent-elision bugs are caught — for example, an unknown `cancel_reason` string would be parsed to `None` and serialized as `cancelReason: null`; the validator rejects the assembled DTO instead of the raw column being non-null.

The matching write-side rules are in [indexer.md](indexer.md#schema-invariants---write-side).

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Market not found / not yet reconciled | `InvalidMarketOrSymbol` | 404 |
| Invalid `status` / `sort` enum value | `InvalidParameter` | 400 |
| Mutually exclusive params (`marketAddress` together with list filters) | `MissingParameter` | 400 |
| Corrupted cursor | `InvalidParameter` (from cursor decode) | 400 |
| Invariant violation on built DTO | `MarketInconsistent` | 503 |

## `/api/v1/depth`

Returns the top of the order book for one outcome of one market: a snapshot of resting bids and asks, the quantity available at each price level, and a sequence number the client uses to tell whether the snapshot has moved since the previous response. The endpoint never queries the contract at request time — every level shown is the projection of indexed `OrderBook` events into a per-order read-model (see [indexer.md](indexer.md#projection--order-events)).

### Resolution

Resolve `(marketAddress, symbol)` to `(orderbook_address, outcome_id, price_precision, quantity_precision)` via [`markets`](data-schema.md#markets) joined with [`market_outcomes`](data-schema.md#market_outcomes). The market must already be reconciled at least once (`last_reconciled_at IS NOT NULL`); otherwise the endpoint returns `InvalidMarketOrSymbol` → 404. The symbol identifies one of the market's outcomes — depth is per-outcome, not per-market.

### Empty-book contract

A reconciled market always has an `orderbook_address`, so an empty book means exactly one thing: no `OrderBook.OrderPlaced` events have landed for `(orderbook_address, outcome_id)` yet. The response is structurally well-formed — empty `bids`, empty `asks`, `lastUpdateId = ""` — and is the steady-state shape for a market that has not yet started trading. Clients can poll cheaply while the market is warming up. A NULL or blank `orderbook_address` on a reconciled row is treated as `MarketInconsistent` (HTTP 503), not silently served as an empty book.

### Aggregation

The API issues one SQL query that produces both sides of the book in a single round trip. Per side, the database:

1. Filters [`live_orders`](data-schema.md#live_orders) to `status = 'OPEN' AND amount_remaining > 0` scoped to this `(orderbook_address, outcome_id)`.
2. Groups by `price`, sums `amount_remaining` — multiple resting orders at one price collapse into a single level. Clients see "quantity available at this price", not the underlying orders.
3. Orders by price (bids descending, asks ascending) and applies `LIMIT $limit`.

Postgres applies the sort and `LIMIT` while reading, so the API receives only the top N price levels per side instead of loading the full open book into memory. The partial index `live_orders_open_book_idx` (`WHERE status = 'OPEN'`) is designed for this depth query.

After the database returns, each side is re-sorted in Rust using exact-numeric `BigUint` comparison — lexicographic string comparison would silently misrank prices of different lengths (`"100" < "99"` lexicographically). Each `[price, quantity]` is then scaled to a fixed-point decimal using the outcome's `price_precision` and `quantity_precision`. The result matches the `[price, quantity]` shape in [api-spec.md](../api-spec.md).

### `lastUpdateId`

`max(live_orders.last_chain_order)` over rows for this `(orderbook_address, outcome_id)` pair. `last_chain_order` is the lex-sortable chain-order string (`msg_chain_order` from the GraphQL gateway) of the most recent event that touched the row; the public `lastUpdateId` is therefore a STRING, not an integer (see [api-spec.md §Order Book](../api-spec.md#order-book)). The per-outcome scope is intentional: a single OrderBook serves multiple outcomes, and a per-orderbook cursor would let a quiet outcome inherit activity from sibling outcomes.

Empty string means no OrderBook event has touched this pair yet. The value never lex-decreases between successive snapshots — `last_chain_order` is updated via `greatest(existing, new)` on the write side, and the reproject loop applies events in `chain_order` so the natural arrival order is already monotonic (see [indexer.md](indexer.md#projection--order-events)).

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

## `/api/v1/orders`

This endpoint replaces the former `/api/v1/openOrders` (GET) and `/api/v1/allOrders` (GET). `DELETE /api/v1/openOrders` (cancel-all-open) is a separate TRADE operation and is out of scope here — its tech spec lives in [write-api.md](write-api.md).

### Source data

The endpoint reads exclusively from [`live_orders`](data-schema.md#live_orders). A row contributes to the response iff all hold:

- `owner_pn_address = ctx.trading_pn.pn_address` — caller is the owner.
- The parent market in [`markets`](data-schema.md#markets) has `last_reconciled_at IS NOT NULL` — pre-reconcile markets are hidden symmetrically with `/api/v1/markets`.
- The row's `status` (combined with `amount_remaining` vs `amount_initial` for OPEN rows) maps to at least one of the public statuses requested in the `status` filter — or, if `status` is omitted, all rows pass.

The query joins through `markets` and [`market_outcomes`](data-schema.md#market_outcomes) to recover the public identifiers `pmp_address` and `symbol` for each row, plus `price_precision` / `quantity_precision` for scaling. See [§ SQL](#sql) for the two query variants.

### Filter resolution

Market filter (same three shapes as before):

| Inputs | Behaviour |
| --- | --- |
| neither `marketAddress` nor `symbol` | all-markets query, owner-scoped. |
| both present | resolve `(marketAddress, symbol)` to `(orderbook_address, outcome_id)` via `markets ⨝ market_outcomes`. If the pair is missing or its market is not reconciled → `DomainError::InvalidMarketOrSymbol` → `-1121` / 404. |
| exactly one present | `DomainError::MissingParameter` → `-1102` / 400. |

The pair-resolution lookup is a separate SQL round-trip that runs before the main query so the unknown-pair case can be distinguished cleanly from "owner has no orders here". Resolution is bound by `last_reconciled_at IS NOT NULL` so a pair that exists in `markets` but has never reconciled is reported the same way as a pair that does not exist.

Status filter (CSV). The handler parses `status` once at request entry into a `BTreeSet<PublicOrderStatus>`:

1. Split on `,`, trim each token of ASCII whitespace, drop empty tokens, de-duplicate.
2. Each token must match exactly one of the five canonical strings `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `REJECTED`. Anything else → `DomainError::InvalidParameter` → `-1130` / 400.
3. Absent (or empty after trim) `status` parameter means "all five statuses".

The set is then translated into a SQL `OR`-disjunction (see [§ Status mapping](#status-mapping)). Allow-list matching guarantees the SQL fragment contains only safe literal status strings — no user input flows into the SQL string.

### Status mapping

The public `status` enum is partly derived from row state (OPEN-side `NEW` vs `PARTIALLY_FILLED`), partly mirrored from the stored `status` column:

| Requested public `status` | `live_orders` predicate |
| --- | --- |
| `NEW` | `status = 'OPEN' AND amount_remaining = amount_initial` |
| `PARTIALLY_FILLED` | `status = 'OPEN' AND amount_remaining < amount_initial AND amount_remaining > 0` |
| `FILLED` | `status = 'FILLED'` |
| `CANCELED` | `status = 'CANCELLED'` (the DB stores the British spelling; the public enum uses the American one — see [api-spec §Order Status](../api-spec.md#order-status)) |
| `REJECTED` | `status = 'REJECTED'` — produced by the future projector documented in [§ REJECTED — future work](#rejected--future-work). Until that projector ships, no row matches and the filter returns empty. |

For OPEN rows the projection layer derives the response-side public status with the same `executed_qty == 0 ? 'NEW' : 'PARTIALLY_FILLED'` split (see [§ Field projection](#field-projection)). The OPEN-side `amount_remaining > 0` guard is kept inside the `PARTIALLY_FILLED` predicate (rather than as a global filter) — a stale `OPEN` row with `amount_remaining = 0` would be a projector bug and we don't want to silently surface it as `NEW`.

If `status` is absent, the SQL emits no status predicate at all and every owner row passes — defence-in-depth checks live in the projection layer instead.

### Field projection

`origQty = scale(amount_initial)`, `executedQty = scale(amount_initial - amount_remaining)`, `price = scale(price)`. Scaling uses `market_outcomes.price_precision` / `quantity_precision`. `timeInForce` is always `GTC`, `type` is always `LIMIT` in v1 (no other combinations are produced by the order-placement path).

Public `status` per row:

| Stored `live_orders.status` | `amount_remaining` | Public `status` |
| --- | --- | --- |
| `OPEN` | `= amount_initial` | `NEW` |
| `OPEN` | `> 0 AND < amount_initial` | `PARTIALLY_FILLED` |
| `OPEN` | `0` | projector bug — log an error and skip the row |
| `FILLED` | (any) | `FILLED` |
| `CANCELLED` | (any) | `CANCELED` |
| `REJECTED` | (any) | `REJECTED` |

`orderId` rendering: the underlying column is `numeric(78,0)`. The renderer emits the empty string for rows where the chain has not assigned an id — today that is exactly the `status = 'REJECTED'` lifecycle (the rejected placement never produced an `OrderBook.OrderPlaced` event). Otherwise it emits the decimal string form of `order_id`. The status-based predicate decouples this from whatever physical-storage choice the REJECTED follow-up adopts for `order_id` (see [§ REJECTED — future work](#rejected--future-work)). `clientOrderId` projects an empty string when the column is `NULL`.

### Time fields

`time` and `updateTime` come from `live_orders.chain_created_at` / `chain_updated_at`, not from DB bookkeeping columns. Rationale: DB `created_at` / `updated_at` drift from real chain time during indexer backlog, which is observable to clients and would make pagination cursors non-monotonic across replays.

### Pagination

Cursor-based on `live_orders.placed_chain_order` with a strict `<` comparison (DESC sort).
`msg_chain_order` is globally unique and lexicographically monotonic by GraphQL gateway
design, so no tie-breakers are needed. The column is set once by the
`OrderPlaced` projector (and by the future REJECTED projector — see below) via
`coalesce` (first-write-wins) and never changes on replay or subsequent
events, which preserves cursor stability across reprojects and fills.

Consequence: between two paginated reads, an order that transitions to FILLED or CANCELLED keeps its position in the result — closed rows do not drop out of `/orders` (they only drop out of a filter that excluded their new status). No duplication or skipping is possible. `OrderFilled` and `OrderCancelled` advance `last_chain_order` and `chain_updated_at` but do not modify `placed_chain_order`, so the row's position in the sort order remains fixed.

#### Cursor format

The cursor is the `placed_chain_order` value of the last retained row and is returned verbatim. The server validates only that the value is a non-empty UTF-8 string after trimming whitespace; a corrupted or empty cursor surfaces as `DomainError::MissingParameter` → `-1102` / `400`. A well-formed cursor whose value lexicographically precedes every order in scope returns an empty page with `nextCursor: null` and is not treated as an error.

The format is not opaque: clients may read the cursor as a plain string, but they must not parse its internal structure or generate cursors of their own. It should be treated as a token to pass back verbatim.

#### Page-size protocol

- `limit` defaults to `100` when omitted.
- Valid range is `[1, 500]`. Out-of-range → `-1102` / 400.
- The SQL query fetches `LIMIT $limit + 1` rows. If `$limit + 1` rows are returned, the last row is omitted from the response and `next_cursor` is built from the row that remains at position `$limit` (the last retained row); otherwise, `next_cursor` is `null`. The `+1` lookahead is the only mechanism by which the server distinguishes between "exactly `$limit` rows remaining" and "more rows available". Building the cursor from the last retained row ensures that the next page's strict `<` predicate advances past that boundary row, including any retained row the response mapper later drops as invalid, instead of re-reading it.

### Auth & permissions

`USER_DATA`. Handled by the existing auth hoop:

- `-1003` / 401 for missing or unparseable envelope (`X-DODEX-APIKEY`, `timestamp`, `signature`, `recvWindow`).
- `-1002` / 401 for unknown / disabled key.
- `-1002` / 401 for a key without the `USER_DATA` permission. Identical on the wire to a credential rejection — intentional `msg` opacity, see [auth.md](auth.md#authorization-permissions).

The handler reads `ctx` via `require_auth(depot, Permission::UserData)` and uses `ctx.trading_pn.pn_address` as the `owner_pn_address` filter. No additional permission logic.

### Error mapping

| Condition | `DomainError` | API code | HTTP |
| --- | --- | --- | --- |
| Exactly one of `marketAddress` / `symbol` present | `MissingParameter` | `-1102` | 400 |
| `limit` out of `[1, 500]` | `MissingParameter` | `-1102` | 400 |
| `limit` present but non-numeric | `InvalidParameter` | `-1130` | 400 |
| `cursor` is empty or whitespace-only | `MissingParameter` | `-1102` | 400 |
| Unknown token in `status` CSV | `InvalidParameter` | `-1130` | 400 |
| Pair not found, or its market is unreconciled | `InvalidMarketOrSymbol` | `-1121` | 404 |
| Missing / invalid signature / API key / timestamp | upstream auth | `-1003` | 401 |
| Missing `USER_DATA` permission | upstream auth | `-1002` | 401 |
| Unexpected (DB / decode / etc.) | `Unexpected` | `-1000` | 500 |

### SQL

Both variants share the same projection list (`pmp_address`, `symbol`, `order_id`, `client_order_id`, `price`, `amount_initial`, `amount_remaining`, `is_buy`, `chain_created_at_us`, `chain_updated_at_us`, `placed_chain_order`, `lo.status`, `price_precision`, `quantity_precision`). The base predicate is `owner_pn_address = $1 AND m.last_reconciled_at IS NOT NULL AND chain_created_at IS NOT NULL AND chain_updated_at IS NOT NULL`.

`chain_created_at IS NOT NULL AND chain_updated_at IS NOT NULL` are SQL-side heap filters. They guard against a rare ingestion path in which the GraphQL gateway omits `created_at` on an edge — such rows must not surface through the endpoint (otherwise the response decoder would fail when mapping `NULL` into `i64`) — while keeping the index independent of the display-only timestamp columns.

The status predicate is built dynamically from the parsed `BTreeSet<PublicOrderStatus>`:

- Empty set / `status` absent → no status predicate (every row passes).
- Otherwise → `AND (<per-status predicate> OR <per-status predicate> ...)`, one disjunct per public-status token, drawn from the [§ Status mapping](#status-mapping) table. The disjunct fragments are compile-time string constants; only the allow-listed set drives which fragments are joined.

The cursor predicate uses a single text comparison against `placed_chain_order` with strict `<`. No tie-breaker columns are required — `msg_chain_order` from the gateway is globally unique. Sort: `ORDER BY lo.placed_chain_order DESC`.

The filtered variant pre-resolves `(orderbook_address, outcome_id)` via a separate query against `markets ⨝ market_outcomes`. That query is likewise gated by `last_reconciled_at IS NOT NULL`. The pair predicate (`lo.orderbook_address = $X AND lo.outcome_id = $Y`) is appended to the base predicate; the all-markets variant omits it.

### Index reliance

The existing `live_orders_open_owner_idx` is partial on `status = 'OPEN' AND amount_remaining > 0` — it covers the OPEN-only path but cannot serve closed-row queries. It is replaced (migration ships with the implementation):

- **Drop**: `live_orders_open_owner_idx`.
- **Create**: `live_orders_owner_idx` — partial index on `(owner_pn_address, placed_chain_order DESC)` with predicate `owner_pn_address IS NOT NULL AND chain_created_at IS NOT NULL`. Covers the default-status query (all five statuses) and any CSV-driven subset.

Status filters become heap predicates on top of the index range. Per-owner cardinalities are expected in the hundreds even on power-trader accounts; a heap filter over a single-owner range is cheap relative to maintaining a wider composite index that would also need to track the derived NEW/PARTIALLY_FILLED split.

The market-filter pair predicate (`orderbook_address = $X AND outcome_id = $Y`) is likewise a heap filter, matching the strategy already used for the OPEN-only variant.

`live_orders_open_book_idx` (used by `/api/v1/depth`) is unaffected.

The data-schema doc ([`live_orders`](data-schema.md#live_orders)) is updated synchronously with the migration.

### Visibility / eventual consistency

Between `OrderBook.OrderPlaced` and `PrivateNote.OrderPlacedConfirmed`, the row exists in `live_orders` with `owner_pn_address = NULL`. The partial index excludes `NULL` owners, so the row contributes to public depth but cannot appear in `/api/v1/orders`.

The confirmation event projector attaches the owner; if the confirmation event arrives first, it is deferred and replayed once the OrderBook row exists (via the existing `Deferred → Applied` reprojection mechanism). This window is exposed to clients as an eventual-consistency note in `api-spec.md`; no additional mitigation is provided in v1.

REJECTED rows, once the future projector ships, are created with `owner_pn_address` already set (the source event lives on the PN itself), so there is no equivalent two-stage attribution window for that lifecycle.

### Contract event consumption

This endpoint is downstream of the indexer; it consumes only what the projectors write to `live_orders`. The chain-side surface consumed by `/orders` is:

| Event | Producer | Read-model effect |
| --- | --- | --- |
| `OrderBook.OrderPlaced` | OrderBook | Creates `live_orders` row, `status='OPEN'`. |
| `OrderBook.OrderFilled` | OrderBook | Decrements `amount_remaining`; flips `status` to `FILLED` on full fill. |
| `OrderBook.OrderCancelled` | OrderBook | Preserves the current `amount_remaining` as the cancelled remainder; flips `status` to `CANCELLED`. |
| `PrivateNote.OrderPlacedConfirmed` | PrivateNote | Attaches `owner_pn_address`. |

PrivateNote may emit additional confirmation events for account accounting (for example fee or balance updates), but those are routed to the `/api/v1/account` code path, not to `/orders`. The outward shape of the three OrderBook events above and of `OrderPlacedConfirmed` is the only chain-side surface this endpoint depends on.

### REJECTED — future work

This scope depends on a contracts + indexer change outside the current read-model implementation.

**Chain-side gap.** `OrderBook._notifyRejectedPlace` already calls `PrivateNote.onOrderRejected(...)` with the full original `PlaceParams` (outcomeId, isBuy, flags, price, amount, clientOrderId, opNonce). `onOrderRejected` restores the held balance but emits no public event. The external `OrderBook.Rejected(entryType, depositHash)` event has too little payload — no order parameters, no owner attribution — to reconstruct a `live_orders` row.

**Contract change.** Add a PN-side event emitted at the end of `onOrderRejected`:

```solidity
event OrderPlaceRejected(
    address orderBook,
    uint256 eventId,
    uint128 clientOrderId,
    uint32  outcomeId,
    bool    isBuy,
    uint8   flags,
    uint256 price,
    uint128 amount,
    uint64  opNonce
);
```

Destination tag: `address.makeAddrExtern(PRIVATENOTE_ORDER_REJECTED, bitCntAddress)`, by analogy with `PRIVATENOTE_ORDER_PLACED`. Source address of the event (the PN itself) provides owner attribution.

**Projector.** A new `OrderPlaceRejectedProjector` in `crates/infrastructure/src/projectors.rs` writes one row to `live_orders` per event:

- `orderbook_address = event.orderBook`.
- `order_id = 0` (sentinel — no chain id is assigned; the API renders it as `""`).
- `outcome_id`, `is_buy`, `price`, `client_order_id`, `amount_initial = amount`, `amount_remaining = 0` from the event payload.
- `owner_pn_address = event.source_address` (the PN that emitted the event).
- `status = 'REJECTED'`.
- `chain_created_at = chain_updated_at = event.created_at`, `placed_chain_order = last_chain_order = event.msg_chain_order`.

**Open question — primary-key collision.** `live_orders` PK is `(orderbook_address, order_id)`. Multiple rejected placements against the same OB would collide on `order_id = 0`. Two viable options:

1. Add a `synthetic_id numeric(78,0) NOT NULL DEFAULT 0` column and extend the PK to `(orderbook_address, order_id, synthetic_id)`. REJECTED rows fill `synthetic_id` from a deterministic hash of `msg_chain_order`; all other lifecycles keep the default `0`.
2. For REJECTED rows, store the hashed `msg_chain_order` directly in `order_id`, partitioning the id space ("real" chain ids are bounded by uint128; we can carve the high half for synthetic ids). Cheaper schema-wise but couples the column's meaning to its high bit.

Decide this with the contracts change; the choice should not perturb the `/orders` query plan (both options leave `(owner_pn_address, placed_chain_order)` as the seek key).

**Schema impact.** Extend the `live_orders.status` CHECK to `IN ('OPEN', 'FILLED', 'CANCELLED', 'REJECTED')`. Either add `synthetic_id` (option 1) or document the high-bit reservation on `order_id` (option 2). Update [`data-schema.md`](data-schema.md#live_orders) with the migration.

**Idempotency.** `INSERT ... ON CONFLICT DO NOTHING` on the resulting PK. Replays of the same PN event must not double-write.

**Test coverage** for the contracts/indexer change: scenarios in `crates/infrastructure/tests/orders.rs` exercising the projector with synthetic gateway fixtures, plus a `services/api/tests/orders_http.rs` case asserting that `status=REJECTED` switches from empty before projector support to populated after projector support on the same fixture row.

### Test coverage

Three integration suites, all gated on `TEST_DATABASE_URL`:

- `crates/infrastructure/tests/orders.rs` — owner scoping, DESC sort, scaling, the three market-filter shapes, `status` CSV across all five tokens (REJECTED returns empty before contracts/indexer support), cursor advance, cursor stability under concurrent fills and cancellations (closed rows retain their position), `limit` defaults and bounds, invalid `status` tokens, invalid cursor, `executedQty > 0` for `CANCELED` partial-then-cancel rows.
- `crates/infrastructure/tests/reprojection.rs` — extend the existing deferred-replay tests to cover `OrderPlacedConfirmed` arriving after the row has already transitioned to `FILLED` / `CANCELLED`; assert the owner attaches and the row appears under those public statuses in `/orders`.
- `services/api/tests/orders_http.rs` — happy path through the production router with the wrapped response, the four error codes (`-1102`, `-1121`, `-1130`, auth), and the pagination round-trip across mixed-status pages.

## `/api/v1/account`

See [api-spec §Account Balance](../api-spec.md#account-balance) for the public contract. Balance sourcing rules: [auth.md §Balance Source](auth.md#balance-source).

_Implementation tech spec to be filled in._
