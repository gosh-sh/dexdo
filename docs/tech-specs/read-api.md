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

**Trade tape** — the `/api/v1/trades` response for one market outcome: a newest-first list of maker↔taker matches built from the append-only `trades` table, not by querying the OrderBook contract during the HTTP request.

**DTO** — Data Transfer Object. In this document it means the API response object after the backend has assembled it from database rows, but before it is serialized to JSON and sent to the client.

**Available event** — an `oracle_events` row eligible to surface in `/api/v1/oracles`: `is_deleted = false`, `deadline` strictly in the future relative to request `now`, and metadata-reconciled (`meta_reconciled_at IS NOT NULL`). Event lists and oracles with no available events are omitted from the response.

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
4. Joins [`market_outcomes`](data-schema.md#market_outcomes) for the outcomes array, including per-outcome `pricePrecision`, `tickSize`, `stepSize`, `minNotional`. `maxBatchSize` is filled from api config (`chain.max_batch_size`) at render — backend policy mirroring the chain's compiled-in cap, not read-model data.
5. Fetches the `event.*` block in a separate batch (`fetch_oracle_events`) joined across [`oracle_events`](data-schema.md#oracle_events) ⨝ [`oracle_event_lists`](data-schema.md#oracle_event_lists) ⨝ [`oracles`](data-schema.md#oracles) for every `pmp_address` on the page. A PMP can be confirmed by multiple `OracleEventList` contracts (`PrivateNote.PMPDeployed.oracleEventLists: address[]`), producing N rows here; the API collapses them into one `event.oracles[]` array. Joining `oracle_events` directly into the main markets SELECT would have multiplied the market row by N, inflating `has_more`/cursor and emitting duplicate listings.
6. Stamps `makerCommission` and `takerCommission` from the global constants `MAKER_COMMISSION` / `TAKER_COMMISSION` in `crates/domain/src/lib.rs`. These mirror `TAKER_FEE_RATE` / `MAKER_REBATE_*` / `FEE_DENOMINATOR` in `contracts/modifiers/modifiers.sol` and are not stored in the `markets` table — the contract defines fees globally today, so the read path does not look them up per market.

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
| CANCELLED requires kind=CANCELLED and a **valid** `cancelReason` (PMP_REJECTED_BY_ORACLE or EVENT_CANCELLED) | [api-spec Terminal](../api-spec.md#terminal): cancelReason must distinguish source |
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

## `/api/v1/oracles`

Public discovery endpoint: lists oracles, their event lists, and the events those lists currently offer for market creation. Public contract: [api-spec §Oracles](../api-spec.md#oracles). No authentication — the route is mounted alongside `/api/v1/markets` and `/api/v1/depth`, outside the auth subrouter. The Postgres source is [`oracles`](data-schema.md#oracles) ⨝ [`oracle_event_lists`](data-schema.md#oracle_event_lists) ⨝ [`oracle_events`](data-schema.md#oracle_events); the write side (projectors plus the OracleEventList reconciler) is in [indexer.md](indexer.md). The endpoint never queries the oracle contracts at request time — it reads the indexed discovery read-model.

The response is grouped oracle → event list → event. Pagination is **by oracle** (`limit` counts oracles); an oracle's full set of event lists and available events is always returned whole — there is no inner pagination in v1.

### serverTime

`serverTime` is the unix-seconds timestamp captured once at handler entry. The same value is the `now` used for the deadline-availability predicate, so one response cannot mix an availability boundary computed from a different clock reading than the one it reports — the same discipline `/api/v1/markets` applies to lifecycle status.

### Available events

An `oracle_events` row is **available** (eligible to surface) when all hold:

- `is_deleted = false` — not soft-deleted. No projector sets this `true` today: the OracleEventList contract emits no delete/cancel event, so the conjunct is currently a no-op kept for forward-compatibility.
- `deadline > now` — the event can still be used for a new market. The boundary is strict: at `now == deadline` the event is past and hidden, mirroring the `now >= result_end → EXPIRED` boundary in `/api/v1/markets`.
- `meta_reconciled_at IS NOT NULL` — the OracleEventList reconciler has filled the metadata that lives only in the `_events` getter (`describe`, `trust_addr`, and `outcome_names_jsonb`). This is the symmetric analogue of the `last_reconciled_at IS NOT NULL` visibility gate on `/api/v1/markets`: an event is hidden until the backend can describe it fully, so `outcomes` is never served empty merely because reconciliation has not run. `event_name`, `oracle_fee`, and `deadline` arrive earlier via the `EventAdded` projector, but the reconciler-only fields gate visibility.

Event lists with no available events, and oracles with no non-empty event lists, are omitted (see [api-spec §Oracles](../api-spec.md#oracles): "Event lists and oracles with no remaining events are omitted").

### Filters

All four query filters combine freely — there is no mutually-exclusive mode like `/api/v1/markets`'s `marketAddress`:

| Param | Predicate |
| --- | --- |
| `oracleAddress` | `oracles.address = $addr`, applied to oracle selection only. Blank / whitespace is treated as absent. |
| `eventId` | The client passes the hex form (as rendered in `eventId` responses); the read path converts it to decimal and matches `oracle_events.internal_id_in_eventlist = $decimal::numeric`. With this filter the `events[]` arrays contain only the matching event, and lists / oracles without it are omitted. Un-decodable hex → `InvalidParameter` / 400. |
| `deadlineBefore` | `oracle_events.deadline < $deadlineBefore` (unix seconds), combined with the availability `deadline > now`, i.e. `now < deadline < deadlineBefore`. Non-numeric → `InvalidParameter` / 400. |
| `limit` | Oracle page size. Default 50, clamped to `[1, 200]`; non-numeric → `InvalidParameter` / 400. Clamping (rather than rejecting) out-of-range values matches `/api/v1/markets`. |

The availability predicate plus the `eventId` / `deadlineBefore` event-level filters form a single shared SQL fragment, bound identically into the Phase-1 `EXISTS` sub-query and the Phase-2 fetch (see [§ Query](#query)). Sharing one fragment is load-bearing: were the two to diverge, Phase 1 could select an oracle whose events Phase 2 then filters away, emitting an empty oracle and inflating `hasMore` — the same class of bug the markets `STATUS_CASE` sharing prevents.

### Query

Two round-trips, mirroring `/api/v1/markets`'s `fetch_listing` → `fetch_outcomes` shape:

1. **Oracle page.** Select the next page of oracle ids, keyset-ordered by `(name, id)`:

   ```sql
   select o.id, o.name, o.address
     from oracles o
    where ($oracle_address is null or o.address = $oracle_address)
      and ( $cursor_name is null
            or o.name > $cursor_name
            or (o.name = $cursor_name and o.id > $cursor_id) )
      and exists (
          select 1
            from oracle_event_lists oel
            join oracle_events oe on oe.eventlist_id = oel.id
           where oel.oracle_id = o.id
             and <available-event predicate + event filters>
      )
    order by o.name asc, o.id asc
    limit $limit + 1;
   ```

   `oracles.name` is `UNIQUE`, so `name` alone is a total order and `id` is only a defensive tiebreaker. The `+1` lookahead is the sole signal distinguishing "exactly `$limit` oracles remain" from "more follow", identical to the markets listing. `hasMore` is true iff `$limit + 1` rows return; the extra row is dropped and `nextCursor` is built from the last *retained* oracle.

2. **List + event fetch.** For the retained oracle ids, one query returns every available list+event row:

   ```sql
   select oel.oracle_id,
          oel.list_index,
          oel.address                       as eventlist_address,
          oel.description                   as eventlist_description,
          oe.internal_id_in_eventlist::text as event_id,
          oe.event_name,
          oe.describe                       as event_description,
          oe.oracle_fee::text               as oracle_fee,
          oe.deadline,
          oe.trust_addr,
          oe.outcome_names_jsonb
     from oracle_event_lists oel
     join oracle_events oe on oe.eventlist_id = oel.id
    where oel.oracle_id = any($ids)
      and <available-event predicate + event filters>
    order by oel.oracle_id, oel.list_index asc, oe.deadline asc, oe.internal_id_in_eventlist asc;
   ```

   The rows are grouped in Rust: `oracle_id` → `(list_index, address, description)` → events. SQL already emits them in the api-spec order (oracle name fixed by Phase 1, then list index, deadline, event id), so grouping preserves order without a re-sort.

### Response assembly

Field mapping (see [api-spec §Oracles](../api-spec.md#oracles) for the public shapes):

| Response field | Source | Notes |
| --- | --- | --- |
| `oracles[].name` / `.address` | `oracles.name` / `.address` | |
| `eventLists[].index` | `oracle_event_lists.list_index` | |
| `eventLists[].address` | `oracle_event_lists.address` | |
| `eventLists[].description` | `oracle_event_lists.description` | `NOT NULL` column written by the deploy projector from the `OracleEventListDeployed` payload (read strictly). The public field is therefore a plain `STRING` (possibly empty, never null). |
| `events[].eventId` | `oracle_events.internal_id_in_eventlist` → `numeric_to_hex` | The same hex rendering `/api/v1/markets` uses for `event.eventId`, so the value round-trips back into the `eventId` filter. |
| `events[].eventName` | `oracle_events.event_name` | `EventAdded` projector. |
| `events[].description` | `oracle_events.describe` | Reconciler-only; `null` until reconciled to a non-null value. |
| `events[].oracleFee.asset` | literal `"SHELL"` | The oracle contracts denominate fees in SHELL today; not stored per-event. A second fee asset would turn this into a `ref_tokens` lookup. |
| `events[].oracleFee.amount` | `oracle_events.oracle_fee::text` | Raw chain integer as a decimal string — **not** scaled, matching the unscaled `fee` rendering in `/api/v1/markets`'s `event.oracles[]`. |
| `events[].deadline` | `oracle_events.deadline` | Unix seconds. |
| `events[].trustAddress` | `oracle_events.trust_addr` | Reconciler-only; the raw `0x…` form returned by the `_events` getter. `null` when absent on chain. |
| `events[].outcomes` | `oracle_events.outcome_names_jsonb` | Decoded to `[{outcomeId, outcomeName}]` sorted by `outcomeId` ascending. |

`outcome_names_jsonb` holds the on-chain `outcomeNames` map as `{"<outcomeId>": "<name>"}`. The decoder parses each key as `u32`, sorts ascending, and rejects a malformed map (non-object, non-numeric or out-of-`u32` key, non-string value) as `MarketInconsistent` — see fail-closed below. A legitimately empty `{}` (the chain published no labels) yields an empty `outcomes` array, not an error.

The top-level response is returned directly (no envelope), like `/api/v1/markets`: `{ serverTime, nextCursor, hasMore, oracles }`.

### Pagination cursor

`nextCursor` is `URL_SAFE_NO_PAD(base64("<id>:<name>"))` of the last retained oracle. The id is written first so the split stays unambiguous when an oracle name itself contains `:` — the decoder splits on the first `:`, parses the left side as `i64`, and takes the entire remainder as the name. A corrupted cursor surfaces as `InvalidParameter` → 400 (wrapped via the same typed-error pattern as the markets cursor), never as a 500.

The cursor is opaque to clients: pass it back verbatim, do not synthesize it. Standard keyset caveat: an oracle inserted (a fresh `OracleDeployed`) with a name sorting *before* the current position is missed until a fresh first-page read; one sorting *after* is picked up. No duplication or skipping of oracles already in range.

### Indexer changes

The read path depends on two write-side additions (write-side detail in [indexer.md](indexer.md)):

1. **`oracle_event_lists.description`** — a new `text NOT NULL` column (migration; [`data-schema.md`](data-schema.md#oracle_event_lists) updated synchronously). The `Oracle.OracleEventListDeployed` event carries `description` alongside `eventListAddress` and `index`; the `apply_oracle_event_list_deployed` projector reads it strictly (a missing field is a decoder/ABI mismatch and fails the projection) and writes it via `coalesce(description, $new)` so replays do not clobber it. Because every list is created from such an event, the column is `NOT NULL` and the public contract stays a plain `STRING`.
2. **`oracle_events.outcome_names_jsonb` population** — the OracleEventList reconciler already fetches each list's `_events` getter, whose per-event tuple includes `outcomeNames` (`map(uint32,string)`), but today extracts only `describe` / `trustAddr`. It is extended to also persist `outcomeNames` into `outcome_names_jsonb` (`coalesce`, idempotent, stamped under the same `meta_reconciled_at` pass). Until this ships every `oracle_events` row carries the default `'{}'`, so `outcomes` would be empty for all events; the availability gate's `meta_reconciled_at IS NOT NULL` conjunct keeps unreconciled events hidden in the meantime.

Decoder checkpoint: the contract change also added a new `Oracle.EventPublished` event, which changes Oracle's event count. The decoder's event-count assertion in `crates/infrastructure/src/decoder.rs` must be re-pinned to the new total when the ABI lands.

### Fail-closed validation

After building each oracle DTO the assembled shape is checked; any violation is `MarketInconsistent` → 503. The 503 is deliberate: the inconsistency is transient (the indexer is mid-replay) and the client should retry.

| Rule | Source |
| --- | --- |
| `outcome_names_jsonb` is a JSON object; every key parses to `u32`; every value is a string | The `_events.outcomeNames` map shape; a non-conforming blob is reconciler / ingestion corruption, not a renderable outcome set. |
| `eventId` renders (numeric → hex conversion succeeds on `internal_id_in_eventlist`) | The same `numeric_to_hex` invariant `/api/v1/markets` enforces on `event.eventId`. |

### Error mapping

| Condition | DomainError | API code | HTTP |
| --- | --- | --- | --- |
| `limit` present but non-numeric | `InvalidParameter` | `-1130` | 400 |
| `deadlineBefore` present but non-numeric | `InvalidParameter` | `-1130` | 400 |
| `eventId` present but not decodable as a uint256 hex | `InvalidParameter` | `-1130` | 400 |
| Corrupted `cursor` | `InvalidParameter` | `-1130` | 400 |
| Invariant violation on built DTO | `MarketInconsistent` | `-1500` | 503 |
| Unexpected (DB / decode / etc.) | `Unexpected` | `-1000` | 500 |

The endpoint is public, so there are no auth rows.

### Decisions for review

Three points decided here for the team to confirm:

1. **Confirmed events are not excluded.** The default availability filter is exactly "not deleted, not past deadline" per [api-spec §Oracles](../api-spec.md#oracles); an event with `confirmed_pmp_address IS NOT NULL` (already backing a market) still lists. Rationale: `OracleEventList.confirmEvent(eventId, oracleListHash, tokenType)` is parameterized by `(oracleListHash, tokenType)`, so one event can back more than one market, and the read-model's single `confirmed_pmp_address` column cannot express "fully consumed". If product intent is "hide once used", add `confirmed_pmp_address IS NULL` to the availability predicate.
2. **`eventLists[].description` is required (resolved).** The api-spec keeps it `STRING`; the fresh path is hardened to match — `OracleEventListDeployed` always carries `description`, the projector reads it strictly, and the column is `NOT NULL`. The value may be an empty string but is never null, so no `STRING | null` softening is needed.
3. **`oracleFee.amount` is unscaled.** Rendered as the raw chain integer (decimal string), consistent with `/api/v1/markets`. If clients need a human-scaled amount, scale by SHELL's `ref_tokens.decimals` — deferred until a concrete need.

### Test coverage

Three suites, the DB-backed ones gated on `TEST_DATABASE_URL`:

- `crates/infrastructure/tests/oracles.rs` — grouping (oracle → list → event); api-spec ordering (name, list index, deadline, event id); the availability gate (deleted / past-deadline / unreconciled rows excluded); each filter (`oracleAddress`, `eventId` narrowing `events[]` to one, `deadlineBefore`); empty-list and empty-oracle omission; cursor advance and stability across pages; `limit` default and clamp; `outcomes` decoded from `outcome_names_jsonb` sorted by `outcomeId`; fail-closed on a malformed `outcome_names_jsonb`.
- Projector / reconciler tests — `apply_oracle_event_list_deployed` persists `description` from the deploy event (and `coalesce` keeps it across replays); the reconciler persists `outcomeNames` into `outcome_names_jsonb` alongside `describe` / `trust_addr`.
- `services/api/tests/oracles_http.rs` — happy path through the production router (top-level `serverTime` / `nextCursor` / `hasMore` / `oracles`, plus the rendered `eventId` hex and nested `oracleFee` / `outcomes`); the single-page `nextCursor: null` shape; the four `400` shapes (`limit`, `deadlineBefore`, `eventId`, `cursor`); the route is reachable without an auth envelope (public). The multi-page cursor walk is exercised at the repo layer (`oracles.rs` above), not re-driven through HTTP.

Domain note: the unused `Oracle` / `OracleEventList` / `OracleEvent` structs in `crates/domain/src/lib.rs` predate the api-spec shape and are replaced by the response types this endpoint introduces (`OracleListing` / `OracleEventListEntry` / `OracleEventEntry` / `OracleOutcome` / `OracleFee`).

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

After the database returns, each side is re-sorted in Rust using exact-numeric `BigUint` comparison — lexicographic string comparison would silently misrank prices of different lengths (`"100" < "99"` lexicographically). Each `[price, quantity]` is then decoded from chain units: `live_orders` stores the raw integers the contract emitted (price in basis points, amount in token atoms), so price is divided by `FULL_PERCENT` (10 000) and amount by `10^decimals` (`decimals` joined from `ref_tokens`), then formatted at the outcome's `price_precision` / `quantity_precision`. The result matches the `[price, quantity]` shape in [api-spec.md](../api-spec.md).

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

## `/api/v1/trades`

Returns the most recent public trades for one outcome of one market: a newest-first tape of maker↔taker matches, each carrying price, size, quote notional, direction, and chain time. The endpoint never queries the contract at request time — every trade shown is the projection of an indexed `OrderBook.OrderFilled` event into the append-only [`trades`](data-schema.md#trades) read-model (write side in [indexer.md](indexer.md#projection--public-trades)). It is public (`NONE`) and unaffected by market lifecycle status: terminal markets (`RESOLVED` / `CANCELLED` / `EXPIRED`) still serve their tape so history stays readable after the book closes.

### Resolution

Resolve `(marketAddress, symbol)` to `(orderbook_address, outcome_id, price_precision, quantity_precision, decimals)` via [`markets`](data-schema.md#markets) ⨝ [`market_outcomes`](data-schema.md#market_outcomes) ⨝ [`ref_tokens`](data-schema.md#ref_tokens) — the quote-asset `decimals` feeds `quoteQty` scaling. The market must already be reconciled at least once (`last_reconciled_at IS NOT NULL`); otherwise the endpoint returns `InvalidMarketOrSymbol` → 404, the same way an unknown pair is reported. A pair that exists in `markets` but has never reconciled cannot be distinguished from one that does not exist, matching [`/api/v1/depth`](#apiv1depth). The symbol identifies one of the market's outcomes — the tape is per-outcome, not per-market.

### Empty-tape contract

A reconciled market with no matched trades yet returns a bare empty array `[]`, not an error — the steady-state shape for a market that has opened but not yet traded, and the trade-tape analogue of depth's empty-book contract. A NULL or blank `orderbook_address` on a reconciled row is `MarketInconsistent` (503), never silently served as an empty tape.

### Query

After resolution, one SQL produces the page in a single round trip:

```sql
SELECT trade_id, price, qty, is_buyer_maker, chain_time
  FROM trades
 WHERE orderbook_address = $1
   AND outcome_id        = $2
   AND chain_time IS NOT NULL
 ORDER BY trade_id DESC
 LIMIT $limit;
```

`trades_tape_idx` (`(orderbook_address, outcome_id, trade_id DESC)`) serves this as an index range scan — the top `$limit` rows per outcome, newest first, without loading the full tape. There is no pagination cursor in v1: the public contract exposes only `limit` (see [api-spec §Recent Trades](../api-spec.md#recent-trades)), so the endpoint is a bounded newest-first window, not a keyset walk.

`trade_id` is the taker-side `chain_order` (`msg_chain_order` from the gateway), which is globally unique and lexicographically monotonic by gateway design — the same total-order property `/api/v1/orders` relies on for `placed_chain_order`. The text `ORDER BY trade_id DESC` therefore already yields true chain order with no tie-breaker and no Rust-side numeric re-sort (unlike depth, where price levels of differing length must be re-ranked with exact-numeric comparison).

`chain_time IS NOT NULL` is a heap filter guarding the rare ingestion path where the gateway delivered the `OrderFilled` edge without a parseable `created_at`; such a row would otherwise crash the response decoder mapping `NULL` into the `time` `i64`. This mirrors the `chain_created_at IS NOT NULL` guard on [`/api/v1/orders`](#apiv1orders).

### Field projection

`trades` holds the raw chain integers the contract emitted; the API decodes each field at render (see [api-spec §Recent Trades](../api-spec.md#recent-trades) for the public shapes):

| Response field | Source | Rendering |
| --- | --- | --- |
| `tradeId` | `trade_id` | Verbatim. Opaque lex-comparable token; the identical value is the `t` field on the [`orderUpdate`](../api-spec.md#order-updates) fill frame for the same match. |
| `price` | `price` | ÷ `FULL_PERCENT` (10 000), formatted at `price_precision` — the same price decode as depth / orders. |
| `qty` | `qty` | ÷ `10^decimals`, formatted at `quantity_precision`. |
| `quoteQty` | `price`, `qty` | Quote-asset notional, computed as the contract computes it: `notional_atoms = price * qty / FULL_PERCENT` (integer division, `BigUint`), then ÷ `10^decimals` formatted at the quote asset's `decimals`. Deriving from the same two raw integers — rather than storing a column — keeps the value reconciled with the on-chain notional that drove settlement; the chain emits no separate notional field. |
| `time` | `chain_time` | Extracted to microseconds and truncated to Unix milliseconds — the same convention as `time` / `updateTime` on `/api/v1/orders`. |
| `isBuyerMaker` | `is_buyer_maker` | Verbatim. `true` ⇒ the resting (maker) side was the buy order and the taker sold (downtick). |

The integer-division order matters: `price * qty / FULL_PERCENT` floors *after* multiplying, exactly as `OrderBook` derives the match notional, so `quoteQty` never drifts from chain by the rounding ulp a `round(price_decimal × qty_decimal)` could introduce.

### Page-size protocol

- `limit` defaults to `20` when omitted.
- Valid range is `[1, 1000]`. Out-of-range → `-1102` / 400; present but non-numeric → `-1130` / 400. The split (range vs parse) matches `/api/v1/orders`'s `limit` semantics rather than depth's silent clamp — the trade tape is aligned with the paginated-read family even though it carries no cursor.

### Invariants

1. Rows are returned strictly newest-first by `trade_id` (DESC), a total chain order; no duplication or skipping across `limit` boundaries.
2. Each row is one taker-side fill = one match; the maker-side `OrderFilled` writes no `trades` row, so a single match never double-counts (write-side rule in [indexer.md](indexer.md#projection--public-trades)).
3. `quoteQty` equals the on-chain match notional under the contract's integer-division rounding.

### Error mapping

| Condition | `DomainError` | API code | HTTP |
| --- | --- | --- | --- |
| `marketAddress` or `symbol` missing or blank | `MissingParameter` | `-1102` | 400 |
| `limit` out of `[1, 1000]` | `MissingParameter` | `-1102` | 400 |
| `limit` present but non-numeric | `InvalidParameter` | `-1130` | 400 |
| Pair not found, or its market is unreconciled | `InvalidMarketOrSymbol` | `-1121` | 404 |
| Reconciled market with NULL/blank `orderbook_address`, or an undecodable raw `price` / `qty` | `MarketInconsistent` | `-1500` | 503 |
| Unexpected (DB / decode / etc.) | `Unexpected` | `-1000` | 500 |

The endpoint is public, so there are no auth rows. The 503 is deliberate and transient: a blank `orderbook_address` means the reconciler is mid-replay, and the client should retry.

### Write side

The read path depends on one write-side projection (detail in [indexer.md §Projection — public trades](indexer.md#projection--public-trades)):

- **`trades` table + `trades_tape_idx`** — an append-only table ([`data-schema.md`](data-schema.md#trades)). The `OrderBook.OrderFilled` projector that maintains `live_orders` also inserts one `trades` row **on the taker-side event** (`isTaker = true`) and nothing on the maker side, so a match is recorded exactly once. `trade_id` is that taker event's `chain_order`; a replayed insert conflicts on it and only coalesces a `NULL` `chain_time` (first-write-wins), so reprojection from `raw_events` is idempotent. An `OrderFilled` observed before its parent `OrderPlaced` is `Deferred` and replayed, the same deferral contract as `live_orders`.

No read-side gate guards the projector: an empty `trades` table simply reads `[]`, already the valid steady state for a market that has opened but not yet traded.

### Eventual consistency

A just-matched trade briefly lags the fill that produced it: the row appears once the taker-side `OrderFilled` is projected (seconds, or after deferred-replay if the fill edge arrived before its parent `OrderPlaced`). This is the same indexer-backlog window `/api/v1/orders` exposes, surfaced to clients as the eventual-consistency note in [api-spec §Recent Trades](../api-spec.md#recent-trades). The endpoint reads only the indexed `trades` table — it never reaches chain at request time.

### Test coverage

Three suites, the DB-backed ones gated on `TEST_DATABASE_URL`:

- `crates/infrastructure/tests/trades.rs` (repo) — resolution (unknown / unreconciled pair → the `InvalidMarketOrSymbol` mapping; blank `orderbook_address` → `MarketInconsistent`); per-outcome scoping (a sibling outcome's trades do not leak); DESC-by-`trade_id` order and `limit` default / bounds; empty tape → `[]`; `price` / `qty` / `quoteQty` scaling, including the integer-division notional matching the contract; `isBuyerMaker` passthrough; a `chain_time IS NULL` row excluded.
- Projector test (alongside the `live_orders` projector scenarios in `crates/infrastructure/tests/reprojection.rs`) — the taker-side event (`isTaker = true`) writes exactly one `trades` row and the maker-side writes none; `trade_id` equals the taker event's `chain_order`; replay is idempotent (`ON CONFLICT`); one taker crossing N makers yields N rows with N distinct `trade_id`s.
- `services/api/tests/trades_http.rs` — happy path returns a bare JSON array newest-first through the production router; the error shapes (`-1102` missing param, `-1102` limit out of range, `-1130` non-numeric limit, `-1121` unknown pair, `-1500` inconsistent); the route is reachable without an auth envelope (public); a terminal-status market still serves its tape.

## `/api/v1/orders`

`DELETE /api/v1/openOrders` (cancel-all-open) is a separate TRADE operation and is out of scope here — its tech spec lives in [write-api.md](write-api.md).

### Source data

The endpoint reads exclusively from [`live_orders`](data-schema.md#live_orders). A row contributes to the response iff all hold:

- `owner_pn_address = ctx.trading_pn.pn_address` — caller is the owner.
- The parent market in [`markets`](data-schema.md#markets) has `last_reconciled_at IS NOT NULL` — pre-reconcile markets are hidden symmetrically with `/api/v1/markets`.
- `chain_created_at IS NOT NULL AND chain_updated_at IS NOT NULL` — rows that the gateway delivered without a parseable timestamp would otherwise crash the decoder when mapping `NULL` into the `time` / `updateTime` `i64` fields. See [§ SQL](#sql) for how this is enforced and [§ Index reliance](#index-reliance) for why only the `chain_created_at` conjunct is part of the partial index.
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

Status filter (CSV). The handler parses `status` once at request entry into `OrderStatusFilter`, an `All | Only(BTreeSet<QueryableOrderStatus>)` enum:

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
| `REJECTED` | `status = 'REJECTED'` — projector unimplemented; no row currently matches and the filter returns empty. See [§ REJECTED status](#rejected-status) for the projector contract. |

For OPEN rows the projection layer derives the response-side public status with the same `executed_qty == 0 ? 'NEW' : 'PARTIALLY_FILLED'` split (see [§ Field projection](#field-projection)). The OPEN-side `amount_remaining > 0` guard is kept inside the `PARTIALLY_FILLED` predicate (rather than as a global filter) — a stale `OPEN` row with `amount_remaining = 0` would be a projector bug and we don't want to silently surface it as `NEW`.

If `status` is absent, the SQL emits no status predicate at all and every owner row passes — defence-in-depth checks live in the projection layer instead.

### Field projection

`origQty = decode(amount_initial)`, `executedQty = decode(amount_initial - amount_remaining)`, `price = decode(price)`. `live_orders` holds raw chain integers (price in basis points, amount in token atoms); decoding divides price by `FULL_PERCENT` (10 000) and amount by `10^decimals`, then formats at `market_outcomes.price_precision` / `quantity_precision` (`decimals` joined from `ref_tokens`). `timeInForce` is always `GTC`, `type` is always `LIMIT` in v1 (no other combinations are produced by the order-placement path).

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
`OrderPlaced` projector (the REJECTED projector, when present, writes it identically — see below) via
`coalesce` (first-write-wins) and never changes on replay or subsequent
events, which preserves cursor stability across reprojects and fills.

Consequence: between two paginated reads, an order that transitions to FILLED or CANCELLED keeps its position in the result — closed rows do not drop out of `/orders` (they only drop out of a filter that excluded their new status). No duplication or skipping is possible. `OrderFilled` and `OrderCancelled` advance `last_chain_order` and `chain_updated_at` but do not modify `placed_chain_order`, so the row's position in the sort order remains fixed.

#### Cursor format

The cursor is the `placed_chain_order` value of the last retained row and is returned verbatim. The server validates that the value is a non-empty UTF-8 string after trimming whitespace AND no longer than `MAX_CURSOR_LEN` (128 chars — real `msg_chain_order` values are an order of magnitude shorter); an empty / blank cursor surfaces as `DomainError::MissingParameter` → `-1102` / `400`, an oversized cursor as `DomainError::InvalidParameter` → `-1130` / `400`. The length cap prevents an authenticated client from binding a multi-megabyte string into the SQL `placed_chain_order < $cursor::text` comparison. A well-formed cursor whose value lexicographically precedes every order in scope returns an empty page with `nextCursor: null` and is not treated as an error.

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
| `marketAddress` / `symbol` pair is incomplete (only one present, or either is present but blank/whitespace) | `MissingParameter` | `-1102` | 400 |
| `limit` out of `[1, 500]` | `MissingParameter` | `-1102` | 400 |
| `limit` present but non-numeric | `InvalidParameter` | `-1130` | 400 |
| `cursor` is empty or whitespace-only | `MissingParameter` | `-1102` | 400 |
| `cursor` length exceeds `MAX_CURSOR_LEN` (128) | `InvalidParameter` | `-1130` | 400 |
| Unknown token in `status` CSV | `InvalidParameter` | `-1130` | 400 |
| Pair not found, or its market is unreconciled | `InvalidMarketOrSymbol` | `-1121` | 404 |
| Missing / invalid signature / API key / timestamp | upstream auth | `-1003` | 401 |
| Missing `USER_DATA` permission | upstream auth | `-1002` | 401 |
| Unexpected (DB / decode / etc.) | `Unexpected` | `-1000` | 500 |

### SQL

Both variants share the same projection list (`pmp_address`, `symbol`, `order_id`, `client_order_id`, `price`, `orig_qty`, `executed_qty`, `fully_filled`, `corrupt_remainder`, `is_buy`, `chain_created_at_us`, `chain_updated_at_us`, `placed_chain_order`, `lo.status as raw_status`, `price_precision`, `quantity_precision`). The base predicate is `owner_pn_address = $1 AND m.last_reconciled_at IS NOT NULL AND chain_created_at IS NOT NULL AND chain_updated_at IS NOT NULL`.

`chain_created_at IS NOT NULL AND chain_updated_at IS NOT NULL` are SQL-side heap filters. They guard against a rare ingestion path in which the GraphQL gateway omits `created_at` on an edge — such rows must not surface through the endpoint (otherwise the response decoder would fail when mapping `NULL` into `i64`) — while keeping the index independent of the display-only timestamp columns.

The status predicate is built dynamically from `OrderStatusFilter`. `OrderStatusFilter::All` emits no predicate; `OrderStatusFilter::Only` carries a non-empty `BTreeSet<QueryableOrderStatus>`:

- Empty set / `status` absent → no status predicate (every row passes).
- Otherwise → `AND (<per-status predicate> OR <per-status predicate> ...)`, one disjunct per public-status token, drawn from the [§ Status mapping](#status-mapping) table. The disjunct fragments are compile-time string constants; only the allow-listed set drives which fragments are joined.

The cursor predicate uses a single text comparison against `placed_chain_order` with strict `<`. No tie-breaker columns are required — `msg_chain_order` from the gateway is globally unique. Sort: `ORDER BY lo.placed_chain_order DESC`.

The filtered variant pre-resolves `(orderbook_address, outcome_id)` via a separate query against `markets ⨝ market_outcomes`. That query is likewise gated by `last_reconciled_at IS NOT NULL`. The pair predicate (`lo.orderbook_address = $X AND lo.outcome_id = $Y`) is appended to the base predicate; the all-markets variant omits it.

### Index reliance

`live_orders_owner_idx` is a partial index on `(owner_pn_address, placed_chain_order DESC)` with predicate `owner_pn_address IS NOT NULL AND chain_created_at IS NOT NULL`. It covers the default-status query (all five statuses) and any CSV-driven subset.

Status filters become heap predicates on top of the index range. Per-owner cardinalities are expected in the hundreds even on power-trader accounts; a heap filter over a single-owner range is cheap relative to maintaining a wider composite index that would also need to track the derived NEW/PARTIALLY_FILLED split.

The market-filter pair predicate (`orderbook_address = $X AND outcome_id = $Y`) is likewise a heap filter, matching the strategy already used for the OPEN-only variant.

`live_orders_open_book_idx` (used by `/api/v1/depth`) is unaffected.

The data-schema doc ([`live_orders`](data-schema.md#live_orders)) is updated synchronously with the migration.

### Visibility / eventual consistency

Between `OrderBook.OrderPlaced` and `PrivateNote.OrderPlacedConfirmed`, the row exists in `live_orders` with `owner_pn_address = NULL`. The partial index excludes `NULL` owners, so the row contributes to public depth but cannot appear in `/api/v1/orders`.

The confirmation event projector attaches the owner; if the confirmation event arrives first, it is deferred and replayed once the OrderBook row exists (via the existing `Deferred → Applied` reprojection mechanism). This window is exposed to clients as an eventual-consistency note in `api-spec.md`; no additional mitigation is provided in v1.

REJECTED rows (when a projector for them is wired in) carry `owner_pn_address` from the start — the source event lives on the PN itself — so the lifecycle has no equivalent two-stage attribution window.

### Contract event consumption

This endpoint is downstream of the indexer; it consumes only what the projectors write to `live_orders`. The chain-side surface consumed by `/orders` is:

| Event | Producer | Read-model effect |
| --- | --- | --- |
| `OrderBook.OrderPlaced` | OrderBook | Creates `live_orders` row, `status='OPEN'`. |
| `OrderBook.OrderFilled` | OrderBook | Decrements `amount_remaining`; flips `status` to `FILLED` on full fill. |
| `OrderBook.OrderCancelled` | OrderBook | Preserves the current `amount_remaining` as the cancelled remainder; flips `status` to `CANCELLED`. |
| `PrivateNote.OrderPlacedConfirmed` | PrivateNote | Attaches `owner_pn_address`. |

PrivateNote may emit additional confirmation events for account accounting (for example fee or balance updates), but those are routed to the `/api/v1/account` code path, not to `/orders`. The outward shape of the three OrderBook events above and of `OrderPlacedConfirmed` is the only chain-side surface this endpoint depends on.

### REJECTED status

The `REJECTED` status surfaces orders that the OrderBook refused to place. The chain-side carrier is `PrivateNote.OrderPlaceRejected` (declared in `contracts/PrivateNote.sol`, emitted from `onOrderRejected`, modifier id `PRIVATENOTE_ORDER_REJECTED = 153`); the decoder's event count test in `crates/infrastructure/src/decoder.rs` is pinned to the new total. The indexer projector that writes `live_orders` rows for these events is not yet shipped — `status=REJECTED` queries currently return empty.

```
event OrderPlaceRejected(address orderBook, uint256 eventId, uint128 clientOrderId, uint32 outcomeId, bool isBuy, uint8 flags, uint256 price, uint128 amount, uint64 opNonce);
```

`OrderBook._notifyRejectedPlace` calls `PrivateNote.onOrderRejected(...)` with the full original `PlaceParams` (outcomeId, isBuy, flags, price, amount, clientOrderId, opNonce). The external `OrderBook.Rejected(entryType, depositHash)` event has too little payload — no order parameters, no owner attribution — to reconstruct a `live_orders` row, so the projector reads `OrderPlaceRejected` directly.

**Projector contract** — `OrderPlaceRejectedProjector` in `crates/infrastructure/src/projectors.rs` is not implemented; the design below pins the contract any implementation must satisfy. It writes one row to `live_orders` per event:

- `orderbook_address = event.orderBook`.
- `order_id = 0` (sentinel — no chain id is assigned; the API renders it as `""`).
- `outcome_id`, `is_buy`, `price`, `client_order_id`, `amount_initial = amount`, `amount_remaining = 0` from the event payload.
- `owner_pn_address = event.source_address` (the PN that emitted the event).
- `status = 'REJECTED'`.
- `chain_created_at = chain_updated_at = event.created_at`, `placed_chain_order = last_chain_order = event.msg_chain_order`.
- Replays use `INSERT ... ON CONFLICT DO NOTHING` against the resulting PK to stay idempotent.

**Primary-key collision** — `live_orders` PK is `(orderbook_address, order_id)`. Multiple rejected placements against the same OB would collide on `order_id = 0`. Two viable schema options:

1. Add a `synthetic_id numeric(78,0) NOT NULL DEFAULT 0` column and extend the PK to `(orderbook_address, order_id, synthetic_id)`. REJECTED rows fill `synthetic_id` from a deterministic hash of `msg_chain_order`; all other lifecycles keep the default `0`.
2. For REJECTED rows, store the hashed `msg_chain_order` directly in `order_id`, partitioning the id space ("real" chain ids are bounded by uint128; we can carve the high half for synthetic ids). Cheaper schema-wise but couples the column's meaning to its high bit.

The choice should not perturb the `/orders` query plan (both options leave `(owner_pn_address, placed_chain_order)` as the seek key). The migration also extends the `live_orders.status` CHECK to `IN ('OPEN', 'FILLED', 'CANCELLED', 'REJECTED')` and updates [`data-schema.md`](data-schema.md#live_orders).

**Test coverage** for the projector: scenarios in `crates/infrastructure/tests/orders.rs` exercise the projector against synthetic gateway fixtures, and `services/api/tests/orders_http.rs` pins the `status=REJECTED` query shape — empty when the `live_orders.status` CHECK does not yet admit `'REJECTED'`, populated once it does, against the same fixture row.

### Test coverage

Three integration suites, all gated on `TEST_DATABASE_URL`:

- `crates/infrastructure/tests/orders.rs` — owner scoping, DESC sort, scaling, the three market-filter shapes, `status` CSV across all five tokens (REJECTED returns empty while the `live_orders.status` CHECK forbids `'REJECTED'`), cursor advance, cursor stability under concurrent fills and cancellations (closed rows retain their position), `limit` defaults and bounds, invalid `status` tokens, invalid cursor, `executedQty > 0` for `CANCELED` partial-then-cancel rows.
- `crates/infrastructure/tests/reprojection.rs` — `OrderPlacedConfirmed` deferred-replay and idempotency-on-already-attributed paths pin owner attribution, and full place/fill/cancel pipeline scenarios pin terminal-state precedence: cancel-after-full-fill stays `FILLED`, cancel-before-fill stays `CANCELED` with the unfilled remainder, partial-fill-then-cancel reports a non-zero `executedQty`.
- `services/api/tests/orders_http.rs` — happy path through the production router with the wrapped response, the four error codes (`-1102`, `-1121`, `-1130`, auth), and the pagination round-trip across mixed-status pages.

## `/api/v1/account`

Public contract: [api-spec §Account Balance](../api-spec.md#account-balance). Balance sourcing rules: [auth.md §Balance Source](auth.md#balance-source).

The endpoint reads collateral balances directly from chain state — every request runs one off-chain getter call against the caller's trading PrivateNote. Outcome-token holdings live behind [`/api/v1/account/balances`](#apiv1accountbalances) instead, because outcome ownership is scoped per market and the chain-side accessor (`PrivateNote._stakes`) is a per-market mapping lookup.

### Source data

Two inputs feed one response:

1. **Trading PN state.** The auth context resolves the caller to a `pn_address` (from [`accounts.pn_address`](data-schema.md#accounts)). The handler fetches that PN's BOC through the GraphQL gateway (`blockchain { account(address: $pn_address) { info { boc } } }`) and executes the `getDetails()` getter against it via `tvm_runner::run_getter` (the same off-chain TVM executor the market reconciler uses — see [indexer.md §Reconciler](indexer.md)). The getter returns `balance: map[uint32 → uint128]` and `lockedInOrders: map[uint32 → uint128]`, both keyed by `tokenType`. (The underlying contract storage vars are `_balance` / `_lockedInOrders` per Solidity convention; TVM's auto-generated getter strips the leading underscore in the ABI's `outputs` declaration — see `contracts/abi/dex/PrivateNote.abi.json`.)
2. **Token reference.** [`ref_tokens`](data-schema.md#ref_tokens) maps each `tokenType` to its public `token_code` and `decimals`. The lookup is a per-`tokenType` SELECT; cardinality is small (three tokens today) and the JOIN happens on the API side, not in SQL.

### Pipeline

1. `require_auth(Permission::UserData)` resolves `(account_id, pn_address)`.
2. Capture `now_ms` once at handler entry — surfaces as `updateTime`.
3. Fetch the PN BOC. A missing account (`Account::is_none`) surfaces as `DomainError::AccountNotDeployed` → 404 so clients can offer "deploy your account" rather than retry. HTTP / decode failures stay on `MarketInconsistent` → 503 (transient: gateway hiccup or indexer lag clears on its own). Step 5 also surfaces 503, but for a different reason: unknown `tokenType` is read-model drift.
4. Run `getDetails()` through `tvm_runner`. ABI decode errors → `MarketInconsistent`.
5. For each key in the **union** of `balance` and `lockedInOrders`, look up the matching `ref_tokens` row. A key absent from `ref_tokens` → `MarketInconsistent` (the indexer ships with the canonical set; an unknown token type means data drift the API cannot resolve safely). Iterating `balance` alone would skip a locked-only token and leak past the ref-token check; the union closes that gap.
6. Build `balances[]`: one entry per `tokenType` in that same union, with `free` from `balance[tokenType]` and `locked` from `lockedInOrders[tokenType]` (each side defaults to `0` when the key is missing in its own map). The textbook locked-only case is a LIMIT SELL that has consumed the caller's entire free balance — `balance[X]` is gone but `lockedInOrders[X] > 0`. Scale both with `ref_tokens.decimals`. Sort by `asset` ASC for deterministic output.

### Fail-closed validation

After assembly, the API checks:

| Rule | Source |
| --- | --- |
| Every `tokenType` returned by `getDetails()` — in either `balance` or `lockedInOrders` — resolves to a `ref_tokens` row | `ref_tokens` is authoritative for token codes/decimals; a locked-only `tokenType` cannot get a free pass since the API still needs `decimals` to render it |
| `accountId` is non-nil (UUID) | Auth context guarantee; sanity check before serializing |

Violations surface as `MarketInconsistent` → 503.

### Eventual consistency

The endpoint never reads `live_orders`, so it does not inherit any indexer-backlog window. The single chain-side read is atomic with the PN state at the time the gateway captured the account snapshot.

### Error mapping

| Condition | DomainError | API code | HTTP |
| --- | --- | --- | --- |
| Missing / invalid auth envelope | upstream | `-1003` | 401 |
| Unknown / disabled key, or key lacks `USER_DATA` | upstream | `-1002` | 401 |
| Authenticated PN address has no deployed contract on chain | `AccountNotDeployed` | `-2013` | 404 |
| Chain getter / BOC decode failure / unknown token type | `MarketInconsistent` | `-1500` | 503 |
| Request budget elapsed | `RequestTimeout` | `-1007` | 504 |
| Unexpected (DB / decode / etc.) | `Unexpected` | `-1000` | 500 |

## `/api/v1/account/balances`

Public contract: [api-spec §Market Outcome Balances](../api-spec.md#market-outcome-balances). Balance sourcing rules: [auth.md §Balance Source](auth.md#balance-source).

Returns the caller's outcome-token holdings for one market. `free` comes from a chain-side mapping lookup on the trading PrivateNote; `lockedInOrders` comes from the indexed `live_orders` read-model. The two sources differ on purpose — see [Locked source split](#locked-source-split) below.

### Source data

Three inputs feed one response:

1. **Market resolution.** Two SELECTs (one on [`markets`](data-schema.md#markets) INNER-joined to [`ref_tokens`](data-schema.md#ref_tokens) for the quote-asset `decimals`, one on [`market_outcomes`](data-schema.md#market_outcomes)) return `(event_id, oracle_list_hash, token_type, orderbook_address, num_outcomes, decimals, [(outcome_id, symbol, quantity_precision) …])`. The first is gated on `last_reconciled_at IS NOT NULL`; pre-reconcile markets are hidden symmetrically with `/api/v1/markets`. The `ref_tokens` join cannot hide a market: `markets.token_type` is `NOT NULL` and FK-references the statically seeded `ref_tokens` PK, so it is a strict 1:1. The market lifecycle status is NOT a gate — terminal markets still serve balances so holders can see what they own until they claim or settle. Keeping the per-outcome rows in a separate SELECT keeps the row types simple at the cost of one extra round trip.
2. **PN stake state.** The chain-side accessor is the auto-generated getter for the public mapping `PrivateNote._stakes`. TVM Solidity auto-getters for public mappings take no arguments and return the entire `map(uint256 → StakeInfo)` — see the PN ABI under `contracts/abi/dex/PrivateNote.abi.json`. The API computes the per-market key `stake_hash = tvm.hash(abi.encode(event_id, oracle_list_hash, token_type))` — the same hash the PN itself uses internally — and looks it up on the returned map. The hash is built off-chain in Rust via a thin wrapper around `tvm_types`. Each `StakeInfo` value carries three parallel `uint128[]` arrays (`amount`, `debtAmount`, `couponsAmount`) indexed by `outcome_id`, plus housekeeping fields the API ignores. A missing key on the returned map (caller never staked on this market) is treated as "all outcomes at zero", not as an error.

   Returning the whole mapping in one call costs the same as one keyed lookup would on EVM (the ABI shape is fixed by TVM Solidity), so this is an opportunity, not a tax: a future "all my outcomes" view across markets needs no additional chain calls.
3. **`live_orders` aggregation.** One SQL groups OPEN sell orders by outcome:

   ```sql
   SELECT outcome_id, SUM(amount_remaining) AS locked
     FROM live_orders
    WHERE orderbook_address = $1
      AND owner_pn_address  = $2
      AND status = 'OPEN'
      AND is_buy = false
    GROUP BY outcome_id;
   ```

   The partial index `live_orders_owner_idx` (`owner_pn_address IS NOT NULL`) backs this scan; the `(orderbook_address, status, is_buy)` predicates fall on the heap, but per-owner cardinality is small enough that adding a wider composite index is not worth it.

### Pipeline

1. `require_auth(Permission::UserData)` resolves `(account_id, pn_address)`.
2. Parse `marketAddress` — blank or missing → `MissingParameter` → 400 / `-1102`.
3. Run the market-resolution SELECT. Unknown market or `last_reconciled_at IS NULL` → `InvalidMarketOrSymbol` → 404 / `-1121`.
4. Compute `stake_hash = tvm.hash(abi.encode(event_id, oracle_list_hash, token_type))`.
5. In parallel (`tokio::try_join!` — the first error short-circuits, but a typed `DomainError` from either branch is preserved so the handler still maps it correctly):
   - Fetch the PN BOC and run the `_stakes` getter through `tvm_runner` (returns the full `map(uint256 → StakeInfo)`); the API then looks up `map[stake_hash]`.
   - Run the `live_orders` aggregation SELECT.
6. Build `balances[]` in `outcome_id` ASC order. For each outcome:
   - `free = scale(amount[outcome_id] + debtAmount[outcome_id] + couponsAmount[outcome_id], decimals)`. Scaled by the quote asset's on-chain `decimals` (not `quantity_precision`) — the `_stakes` amounts are chain atoms, the same scale `/api/v1/account` uses; scaling by `quantity_precision` would over-report by `10^(decimals − quantity_precision)`. The three pools are summed because the public surface is "what the user owns" — clean, debt-bound, and coupon-bound stakes are all the user's tokens; the distinction is internal accounting that the UI does not need at this layer.
   - `lockedInOrders = scale(coalesce(SUM, 0), decimals)` from the aggregation map (the `live_orders` amounts are chain atoms too); outcomes without a row default to 0.
7. Capture `now_ms` once in the handler before executing the use case — surfaces as `updateTime`.

### Locked source split

Why `free` reads chain and `lockedInOrders` reads `live_orders`:

- `free` comes from `PrivateNote._stakes(hash)`, which the contract mutates atomically with every stake / claim / split / merge / cancel-callback. There is no equivalent indexer projection today, and building one would require projectors for the five stake-mutation events listed in the [stake-projection follow-up](#stake-projection--future-work).
- `lockedInOrders` is the sum of resting sell orders against this outcome. The indexer already tracks these in `live_orders`, with a partial index already sized for per-owner queries. The chain-side analogue would require iterating the OrderBook's internal red-black tree of orders — there is no public per-outcome getter for it.

The split means the two numbers can drift while the indexer is replaying behind chain head: a sell that just landed on chain shows up in `_stakes.amount` (because the OB has not yet acknowledged the lock) AND in `live_orders` (because `OrderPlaced` was projected) — appearing as if both `free` and `lockedInOrders` count it. The window is small (seconds) and self-resolves once `OrderPlacedConfirmed` advances PN state; it surfaces to clients as the same eventual-consistency note that already applies to `/api/v1/orders`.

### Fail-closed validation

Three fail-closed checks guard the pipeline at different stages:

| Rule | Source |
| --- | --- |
| Resolved market has a non-blank `orderbook_address` | DB schema CHECK (`last_reconciled_at IS NULL OR orderbook_address IS NOT NULL`) plus a whitespace re-check, matching `/api/v1/depth`'s contract |
| `_stakes.amount.len() == num_outcomes` (and same for `debtAmount`, `couponsAmount`) when any array is non-empty | The contract initializes all three arrays to `num_outcomes` length on first stake; a mismatch means the indexer's view of `num_outcomes` diverged from chain state |
| Every `live_orders.outcome_id` returned by the aggregation is within `[0, num_outcomes)` | Sanity: a row outside this range is indexer corruption (`OrderBook.OrderPlaced` projector wrote an unknown `outcome_id`) |

Violations surface as `MarketInconsistent` → 503.

### Eventual consistency

`lockedInOrders` inherits the same indexer-backlog window as `/api/v1/orders`: a sell order whose `OrderBook.OrderPlaced` event has not been projected yet is invisible here. Once projected (typically seconds later), the next response shows it. `free` is read live from chain state and does not inherit this window.

### Error mapping

| Condition | DomainError | API code | HTTP |
| --- | --- | --- | --- |
| Missing / invalid auth envelope | upstream | `-1003` | 401 |
| Unknown / disabled key, or key lacks `USER_DATA` | upstream | `-1002` | 401 |
| `marketAddress` missing or blank | `MissingParameter` | `-1102` | 400 |
| `marketAddress` not found, or its market is unreconciled | `InvalidMarketOrSymbol` | `-1121` | 404 |
| Authenticated PN address has no deployed contract on chain | `AccountNotDeployed` | `-2013` | 404 |
| Chain getter / BOC fetch / decode failure, or invariant violation on assembled DTO | `MarketInconsistent` | `-1500` | 503 |
| Request budget elapsed | `RequestTimeout` | `-1007` | 504 |
| Unexpected (DB / decode / etc.) | `Unexpected` | `-1000` | 500 |

### Stake projection — future work

The current design reads `_stakes` from chain on every request. Each request costs one GraphQL `accounts(...) { boc }` fetch plus one local TVM execution. For low call rates (one frontend session per user) this is acceptable; for power users polling rapidly or for shared dashboards this becomes the bottleneck.

A future projection table would mirror PN stake state in Postgres so the API can serve `free` from a DB read. The projection requires handling the full PN-side stake-mutation event surface — `StakeConfirmed`, `StakeCancelled`, `FullSetStakeConfirmed`, `FullSetStakeCancelled`, and `ClaimAccepted` (per `contracts/abi/dex/PrivateNote.abi.json`) — and a reproject path that drains all of them in chain order before responses become trustworthy. The on-demand getter shipping in v1 lets the endpoint be useful immediately and gives us evidence about real-world call patterns before we commit to projector complexity.

### PnStake shape — future work

`PnStake` carries three parallel `Vec<String>` arrays (`amount`, `debt_amount`, `coupons_amount`) indexed by `outcome_id`. The all-or-nothing length invariant ("every array is either empty OR exactly `num_outcomes`") is enforced at runtime by guards in `GetMarketBalancesUseCase::execute` — an illegal value such as `PnStake { amount: vec!["1"], debt_amount: vec![], coupons_amount: vec![] }` returns 503, not silently-wrong per-outcome balances. The mirror concern of a duplicate `outcome_id` in `res.outcomes` is ruled out by the schema `UNIQUE (pmp_address, outcome_id)` on `market_outcomes` (see [data-schema.md](data-schema.md#market_outcomes)), so the runtime length check is the only guarantee needed.

A future refactor could promote the invariant into the type system — for example, a single `Vec<StakeRow { amount, debt_amount, coupons_amount }>` shape that makes the parallel structure unrepresentable. That is purely a maintainability improvement: today the runtime guard already fails closed, so the change is not load-bearing.

### Test coverage

Four test suites, all gated on `TEST_DATABASE_URL`:

- Use-case unit (`crates/application/src/lib.rs`):
  - `get_account_use_case_tests`: renders multiple assets sorted by asset code; `locked` defaults to zero when the `_balance` key is absent on the locked side; `free` defaults to zero when only `_lockedInOrders` carries a `tokenType`; unknown token type → 503; PN reader failure → 503; `scale_decimal` zero-padding.
  - `get_market_balances_use_case_tests`: happy path sums the three stake pools per outcome; absent stake key yields zero free; stake arrays shorter / longer than `num_outcomes`; mixed empty / populated stake arrays; unknown market; PN failure; hasher failure; out-of-range `outcome_id`.
- Repo integration (`crates/infrastructure/tests/balances.rs`): `lookup_ref_token` happy path; `resolve_market_for_balances` happy path / unknown market / unreconciled market / num_outcomes mismatch; the three fail-closed guards (NULL `oracle_list_hash`, blank `orderbook_address`, negative `token_type`); `sum_open_sell_remaining` groups by outcome and filters; empty when no rows match.
- HTTP integration (`services/api/tests/account_http.rs`): happy path; missing API key → 401 / `-1003`; chain-getter failure → 503 / `-1500`; unknown token type → 503 / `-1500`; two credentials produce distinct `accountId`.
- HTTP integration (`services/api/tests/account_balances_http.rs`): happy path sorted by `outcomeId`; absent stake key yields zero free with nonzero locked; missing `marketAddress` → 400 / `-1102`; unknown market → 404 / `-1121`; stake-array mismatch → 503 / `-1500`; terminal market still serves; stake gateway failure → 503 / `-1500`; missing API key → 401 / `-1003`; cross-tenant isolation; production hasher wiring; stake registered at wrong hash yields zero; trade-only key → `-1002` on the user-data route.
