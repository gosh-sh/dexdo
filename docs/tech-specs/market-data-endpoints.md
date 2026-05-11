# Market Data Endpoints: Backend Notes

This document contains implementation-facing notes for the DODEX market-data backend. The public HTTP contract, field names, parameter rules, error shapes, and response examples live in [api-spec.md](../api-spec.md).

## Market Identity

The backend treats `marketAddress` as the PMP address. `orderBookAddress` is derived from PMP state through `PMP.getOrderBookAddress()` or the equivalent `DexLib.computeOrderBookAddress(...)` path and can be known before the OrderBook account is active. Availability of the order book is never inferred from the address alone; it follows the derived market status, with `TRADING` as the active book phase.

### `/api/v1/markets`

The backend computes lifecycle status from indexed contract events and the latest known timings. Relevant events are `OracleEventList.EventConfirmed`, `PMP.TimingsSet`, `PMP.PoolsFrozen`, `PMP.Resolved`, `PMP.PMPCancelled`, and `PMP.EventCancelled`. The request handler captures one unix-seconds `now` value and uses it for both `serverTime` and status derivation so a response cannot cross a lifecycle boundary halfway through rendering.

Implementation outline:

1. Enumerate oracles via `RootOracle.getOracleAddress(name)` or by listening to `RootOracle.OracleDeployed`.
2. For each Oracle, list its `OracleEventList` instances via `Oracle.getEventListAddress(index)`.
3. On each EventList, read `_events` or indexed event metadata. The event info source contains `event_id`, `event_name`, `oracle_fee`, `deadline`, `outcomeNames`, `describe`, `count`, and `trustAddr`.
4. Discover market/PMP addresses from `OracleEventList.EventConfirmed(eventId, pmpAddress)`. Use the block timestamp of this event as `createdAt` for API sorting by recency.
5. For each PMP, read market details from `PMP.getDetails()` and the relevant event metadata. Use `PMP.getOrderBookAddress()` or `DexLib.computeOrderBookAddress(...)` for the deterministic order-book address and expose it in every `/api/v1/markets` response. Use market `status` and `frozenAt` to determine whether the order book is available.
6. Join with precision constants from `modifiers.sol`: `minOrderAmount(token_type)` for minimum order size, `lotSize(token_type)` for amount quantisation, and `TICK_SIZE = 10` for price quantisation. There is no upper bound on price; outcome tokens may trade above one collateral unit.

Status derivation is event-sourced with terminal events taking precedence over time-derived phases: cancellation first, resolution second, then missing timings, pre-freeze phases, and post-freeze phases. `PMP.TimingsSet` supplies the latest stake/result windows; the contract may emit it repeatedly while `now < resultStart`, so keep the latest by block time. `PMP.PoolsFrozen` supplies `frozenAt` and gates post-freeze statuses. `PMP.Resolved` supplies `resolved_at` and `resolved_outcome_id`. `PMP.PMPCancelled` and `PMP.EventCancelled` both mark the market cancelled, but keep distinct cancellation reasons because they have different UI meaning.

Oracle metadata is joined through `OracleEventList.EventAdded`, `OracleEventList.EventConfirmed`, `oracle_event_lists`, and `oracles`. `OracleEventList` state reconciliation fills metadata not carried by events, especially event description and trust address.

Semantic invariants:

1. `status == "TRADING"` implies `timings.frozenAt != null && serverTime < timings.resultStart`.
2. `status == "RESOLVING"` implies `timings.frozenAt != null && timings.resultStart <= serverTime < timings.resultEnd`.
3. `status == "PENDING"` implies `timings == null`.
4. `status == "RESOLVED"` implies `terminal.kind == "RESOLVED" && timings.frozenAt != null`; resolution always follows freeze, see `PMP.sol:1005`.
5. `orderBookAddress` may be known before the OrderBook contract is active on-chain; order-book availability still implies `timings.frozenAt != null` and `status == "TRADING"`.

If any invariant is violated, the backend MUST fail the request closed rather than return an inconsistent market. This protects clients against indexer desyncs.

### `/api/v1/depth`

Compute the OrderBook address via `DexLib.computeOrderBookAddress(PrivateNoteCode, orderBookCode, event_id, ohash, token_type)`. Fetch `OrderBook._state` via a raw account query and decode it offchain; the format is `next_order_id(16) + num_orders(4) + N * 126-byte order records`. Group records by `outcomeId` and side, sort by price, and sum amount per level. For a single order, use the `OrderBook.getOrder(uint128 orderId)` getter.

**Aggregated from `indexer` and `api` services**:
The depth endpoint returns the top of the order book for one outcome of one market: a snapshot of resting bids and asks, the quantity available at each price level, and a sequence number the client uses to tell whether the snapshot has moved since the previous response. The endpoint never queries the contract at request time — every level shown is the projection of indexed `OrderBook` events into a per-order read-model.

Implementation outline:

1. Resolve `(marketAddress, symbol)` to `(orderbook_address, outcome_id, price_precision, quantity_precision)`. The market must already be reconciled at least once; otherwise the endpoint returns `InvalidMarketOrSymbol` (404). The symbol identifies one of the market's outcomes — depth is per-outcome, not per-market.
2. If the market is reconciled but no OrderBook has been observed on-chain yet (`orderBookAddress` still null per invariant #5 above), return an empty snapshot: empty `bids`, empty `asks`, `lastUpdateId = 0`. This is the steady-state shape for pre-freeze markets and is preferable to hiding the market from depth queries.
3. Aggregate the open orders for this `(orderbook_address, outcome_id)` pair into price levels, sort each side, scale prices and quantities to decimals, and compute the sequence number. Details below.

Where the orders come from. Three `OrderBook` events are projected into a per-order read-model:

- `OrderPlaced` inserts a row in the `OPEN` state with the order's price, side (buy or sell), outcome, full quantity, and the chain timestamp as the per-order sequence marker.
- `OrderFilled` subtracts the filled amount from the row's `amount_remaining` and flips status to `FILLED` once the remainder reaches zero.
- `OrderCancelled` flips the row to `CANCELLED` and zeroes its remaining quantity.

Out-of-order delivery from the chain is handled with a monotonic `greatest(existing, new)` update on the per-order sequence marker, so a late `OrderFilled` cannot regress an order's last-update timestamp. Prices and amounts are stored as the raw integers the contract emitted — decimal scaling is a render-time concern, not a write-time concern.

How a depth response is built. Per side, the backend filters to `OPEN` orders with non-zero remaining quantity for the requested outcome, groups them by price, sums their quantities, sorts (bids descending, asks ascending), and takes the requested `limit`. Multiple resting orders at the same price collapse into a single price level — clients see "quantity available at this price", not the individual orders making it up. Sorting is exact-numeric, not lexicographic, so prices of different lengths rank correctly.

Both sides are aggregated and limited inside the database in a single round trip so the API never holds the full open book in memory. The supporting partial index covers exactly this query shape.

After the database returns, each `[price, quantity]` is scaled to a fixed-point decimal using the outcome's `price_precision` and `quantity_precision`. The result is the `[price, quantity]` shape documented in [api-spec.md](../api-spec.md).

Sequence number (`lastUpdateId`). The response carries the maximum per-order sequence marker over the `live_orders` rows for **this** `(orderbook_address, outcome_id)` pair. The per-outcome scope is intentional: a single OrderBook contract serves multiple outcomes, and using a per-orderbook sequence would let a quiet outcome inherit activity from a sibling outcome — clients comparing depth snapshots would see the number advance with no corresponding change to their bids and asks. With the per-outcome scope, `lastUpdateId` advances only when this outcome's book changes.

Semantic invariants:

1. `bids` are sorted by price descending; `asks` ascending. Comparison is exact-numeric.
2. Each price level surfaces as one `[price, quantity]` entry. Quantity is the sum across every resting order at that price.
3. `lastUpdateId` is scoped to `(orderbook_address, outcome_id)`. It is `0` when no `OrderBook` event has touched this pair yet, and never decreases between successive snapshots.
4. A non-null `orderBookAddress` on the underlying market is necessary for non-empty depth but not sufficient — orders only land after `PoolsFrozen` is observed and clients start posting.

If the market is unknown or still pre-discovery (`/api/v1/markets` does not list it), `/api/v1/depth` returns 404. Clients must wait for the market to appear in the listing before querying depth.
