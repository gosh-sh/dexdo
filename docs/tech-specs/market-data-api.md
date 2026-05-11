# Market Data — API: Backend Notes

Implementation-facing notes for the HTTP layer that serves the market-data read-model. The public contract (URLs, field names, parameter rules, error shapes, response examples) lives in [api-spec.md](../api-spec.md). Postgres tables referenced below are documented column-by-column in [data-schema.md](data-schema.md). The write side — how those tables get populated — is in [market-data-indexer.md](market-data-indexer.md).

## Market identity

The backend treats `marketAddress` as the PMP address. `orderBookAddress` is `null` until the indexer observes `PMP.PoolsFrozen` on chain; once non-null it is stable. The write-side gate is described in [market-data-indexer.md](market-data-indexer.md#market-reconciler). Clients MUST use `status` to determine whether the order book is currently available for trading — a non-null `orderBookAddress` does not by itself imply the book is open.

## `/api/v1/markets`

Lifecycle status is computed from the indexed row at request time. The handler captures one unix-seconds `now` value and uses it for both `serverTime` and status derivation so a response cannot cross a lifecycle boundary halfway through rendering.

### Visibility filter

The listing query carries `WHERE m.last_reconciled_at IS NOT NULL`. Markets that the indexer has discovered (`PMPDeployed` arrived) but not yet reconciled are hidden — clients only see markets the backend can describe fully. See [market-data-indexer.md](market-data-indexer.md#visibility-gate) for the symmetric write-side rule.

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

The same logic is mirrored in the SQL `STATUS_CASE` used by the `?status=` filter pushdown, so the listing predicate cannot drift from the Rust-side derivation.

### Building the response

For each row in the page, the API:

1. Derives `status` as above.
2. Builds `timings` from the four timing columns — returns `null` only when at least one is missing (PENDING).
3. Builds `terminal` (with `kind`, `at`, `resolvedOutcomeId`, `cancelReason`) for terminal statuses, `null` otherwise.
4. Joins [`market_outcomes`](data-schema.md#market_outcomes) for the outcomes array, including per-outcome `pricePrecision`, `tickSize`, `stepSize`, `minNotional`, `maxBatchSize`.
5. Joins [`oracles`](data-schema.md#oracles) + [`oracle_events`](data-schema.md#oracle_events) for the `event.*` block (`eventName`, `description`, `oracleName`, `oracleAddress`, `oracleFee`).

`description` and other reconciler-only fields rely on data filled by the OEL reconciler — they may be null briefly after a market is discovered but before the reconciler-side metadata lands.

### Pagination

Two sort modes:

- `sort=resultStart` (default, ascending) — sort key is `coalesce(result_start, +∞)` so PENDING / UPCOMING rows without a resolved `result_start` sort to the end.
- `sort=createdAt` (descending) — sort key is `created_at_micros` (microsecond precision). Sub-second keying avoids the keyset bug where two markets created in the same second could be skipped or duplicated across page boundaries.

Cursor format: URL-safe base64 of `"<sort_key>:<id>"`. The handler decodes and validates the cursor; a corrupted cursor surfaces as `DomainError::InvalidParameter` → HTTP 400, not as an internal error.

### Fail-closed validation

After building the DTO, the API checks the assembled shape against spec invariants. Any violation surfaces as `DomainError::MarketInconsistent` → HTTP 503. The 503 status is deliberate: the inconsistency is transient (the indexer is mid-replay), and the client should retry rather than treat the market as permanently broken. The checks live in `postgres_repo.rs::validate_invariants`:

| Rule | Source |
| --- | --- |
| `timings` is null IFF status is PENDING | api-spec §Timings: "`timings` itself is `null` only for `PENDING`." |
| `terminal` is non-null IFF status ∈ {RESOLVED, CANCELLED, EXPIRED} | api-spec §Terminal |
| RESOLVED requires `frozen_at`, kind=RESOLVED, **`resolvedOutcomeId`** set | api-spec §Terminal ("without it the client cannot know which side won") |
| CANCELLED requires kind=CANCELLED and a **valid** `cancelReason` (PMP_CANCELLED or EVENT_CANCELLED) | api-spec §Terminal: cancelReason must distinguish source |
| EXPIRED requires kind=EXPIRED | spec consistency |
| TRADING / RESOLVING require `frozen_at` | spec consistency with `frozenAt != null` for post-freeze statuses |

The validation works on the *built* DTO rather than the raw row so that downstream silent-elision bugs are caught — for example, an unknown `cancel_reason` string would be parsed to `None` and serialized as `cancelReason: null`; the validator rejects the assembled DTO instead of the raw column being non-null.

The matching write-side rules are in [market-data-indexer.md](market-data-indexer.md#schema-invariants---write-side).

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Market not found / not yet reconciled | `InvalidMarketOrSymbol` | 404 |
| Invalid `status` / `sort` enum value | `InvalidParameter` | 400 |
| Mutually exclusive params (`marketAddress` together with listing filters) | `MissingParameter` | 400 |
| Corrupted cursor | `InvalidParameter` (from cursor decode) | 400 |
| Invariant violation on built DTO | `MarketInconsistent` | 503 |

## `/api/v1/depth`

Returns the top of the order book for one outcome of one market: a snapshot of resting bids and asks, the quantity available at each price level, and a sequence number the client uses to tell whether the snapshot has moved since the previous response. The endpoint never queries the contract at request time — every level shown is the projection of indexed `OrderBook` events into a per-order read-model (see [market-data-indexer.md](market-data-indexer.md#projection--order-events)).

### Resolution

Resolve `(marketAddress, symbol)` to `(orderbook_address, outcome_id, price_precision, quantity_precision)` via [`markets`](data-schema.md#markets) joined with [`market_outcomes`](data-schema.md#market_outcomes). The market must already be reconciled at least once (`last_reconciled_at IS NOT NULL`); otherwise the endpoint returns `InvalidMarketOrSymbol` → 404. The symbol identifies one of the market's outcomes — depth is per-outcome, not per-market.

### Empty-book contract

If the market is reconciled but no OrderBook has been observed on-chain yet (`orderbook_address` still null — see [market-data-indexer.md](market-data-indexer.md#market-reconciler) for when this flips), return a structurally well-formed empty snapshot: empty `bids`, empty `asks`, `lastUpdateId = 0`. This is the steady-state shape for pre-freeze markets and is preferable to hiding the market from depth queries — the client can poll cheaply while the market is still warming up.

### Aggregation

The API issues one SQL query that produces both sides of the book in a single round trip. Per side, the database:

1. Filters [`live_orders`](data-schema.md#live_orders) to `status = 'OPEN' AND amount_remaining > 0` scoped to this `(orderbook_address, outcome_id)`.
2. Groups by `price`, sums `amount_remaining` — multiple resting orders at one price collapse into a single level. Clients see "quantity available at this price", not the underlying orders.
3. Orders by price (bids descending, asks ascending) and applies `LIMIT $limit`.

Pushing the top-N into Postgres keeps the API from materialising the full open book in memory; the partial index `live_orders_open_book_idx` (`WHERE status = 'OPEN'`) covers exactly this query shape.

After the database returns, each side is re-sorted in Rust using exact-numeric `BigUint` comparison — lexicographic string comparison would silently misrank prices of different lengths (`"100" < "99"` lexicographically). Each `[price, quantity]` is then scaled to a fixed-point decimal using the outcome's `price_precision` and `quantity_precision`. The result matches the `[price, quantity]` shape in [api-spec.md](../api-spec.md).

### `lastUpdateId`

The maximum `live_orders.last_event_lt` over rows for this `(orderbook_address, outcome_id)` pair. The per-outcome scope is intentional: a single OrderBook serves multiple outcomes, and a per-orderbook sequence would let a quiet outcome inherit activity from sibling outcomes — clients comparing snapshots would see the number advance with no corresponding change to their depth.

`0` means no OrderBook event has touched this pair yet. The value never decreases between successive snapshots — `last_event_lt` is updated via `greatest(existing, new)` on the write side (see [market-data-indexer.md](market-data-indexer.md#projection--order-events)).

### Invariants

1. `bids` sorted by price descending; `asks` ascending. Comparison is exact-numeric.
2. Each price level surfaces as one `[price, quantity]` entry. Quantity is the sum across every resting order at that price.
3. `lastUpdateId` is scoped to `(orderbook_address, outcome_id)`. It is `0` when no OrderBook event has touched this pair yet, and never decreases between successive snapshots.
4. A non-null `orderBookAddress` on the underlying market is necessary for non-empty depth but not sufficient — orders only land after `PoolsFrozen` is observed and clients start posting.

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Market unknown or pre-reconcile | `InvalidMarketOrSymbol` | 404 |
| Missing `marketAddress` or `symbol` | `MissingParameter` | 400 |
| Invalid `limit` (non-numeric) | `InvalidParameter` | 400 |
