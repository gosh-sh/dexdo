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
