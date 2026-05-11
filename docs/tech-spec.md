# DODEX Technical Specification

This document contains implementation-facing notes for the DODEX REST backend. The public HTTP contract is documented in [api-spec.md](api-spec.md).

## REST API Shape

The API shape follows the Binance Spot REST style where useful, but intentionally removes advanced order types, margin, OCO, strategy parameters, iceberg orders, STP, trailing stops, and other non-basic trading features.

Reference documents:

- Binance Spot REST general API information: https://developers.binance.com/docs/binance-spot-api-docs/rest-api/general-api-information
- Binance Spot REST market data endpoints: https://developers.binance.com/docs/binance-spot-api-docs/rest-api/market-data-endpoints
- Binance Spot REST trading endpoints: https://developers.binance.com/docs/binance-spot-api-docs/rest-api/trading-endpoints
- Binance Spot REST account endpoints: https://developers.binance.com/docs/binance-spot-api-docs/rest-api/account-endpoints

Supported in this API version:

- Limit and market orders.
- Buy and sell sides only.
- `GTC`, `IOC`, `FOK`, and `POST_ONLY` for limit orders.
- Public market data.
- Account balances.
- Open order and closed order history.
- Single and batch order creation.
- Single, batch, and symbol-wide order cancellation.

Not included in this API version:

- WebSocket for fills.
- Maker and taker fees per market. This can be exposed either in `/api/v1/markets` as `makerFee` and `takerFee`, or via a separate `/api/v1/fees` endpoint.

## Market Identity

`marketAddress` is the address of the Prediction Market Pool contract. It is the stable market identifier across the entire lifecycle, the metadata anchor, and the target of `setStake`. `orderBookAddress` is the deterministic OrderBook contract address used for placing orders. The PMP can know it ahead of time, and `/api/v1/markets` may expose it before the contract is active on-chain. Public API requests that target one order book use `marketAddress` and `symbol`, and order-book availability is determined from market `status`.

The public symbol is formed as:

```text
symbol = <marketName>-<OUTCOME_NAME>
```

Quote token type variants:

| tokenType | quoteAsset |
| --- | --- |
| `1` | `NACKL` |
| `2` | `SHELL` |
| `3` | `USDC` |

`tokenType` is the numeric quote-token type accepted by `setStake.token_type`.

Outcome identifiers are `u32` values accepted by `setStake`, `OrderPlaced.outcomeId`, and `Resolved.outcomeId`. API clients MUST use `outcomeId`, not the outcome array index.

## Market Data Backend

### `/api/v1/markets`

The backend computes market lifecycle status from indexed contract events and the latest known timings. Relevant events are `OracleEventList.EventConfirmed`, `PMP.TimingsSet`, `PMP.PoolsFrozen`, `PMP.Resolved`, `PMP.PMPCancelled`, and `PMP.EventCancelled`. The API returns `serverTime` in unix seconds because the contract operates in seconds (`block.timestamp` is `uint64` seconds); `serverTime` and `status` MUST be evaluated from a single `now` value within one request so that the response is internally consistent.

Implementation outline:

1. Enumerate oracles via `RootOracle.getOracleAddress(name)` or by listening to `RootOracle.OracleDeployed`.
2. For each Oracle, list its `OracleEventList` instances via `Oracle.getEventListAddress(index)`.
3. On each EventList, read `_events` or indexed event metadata. The event info source contains `event_id`, `event_name`, `oracle_fee`, `deadline`, `outcomeNames`, `describe`, `count`, and `trustAddr`.
4. Discover market/PMP addresses from `OracleEventList.EventConfirmed(eventId, pmpAddress)`. Use the block timestamp of this event as `createdAt` for API sorting by recency.
5. For each PMP, read market details from `PMP.getDetails()` and the relevant event metadata. Use `PMP.getOrderBookAddress()` or `DexLib.computeOrderBookAddress(...)` for the deterministic order-book address and expose it in every `/api/v1/markets` response. Use market `status` and `frozenAt` to determine whether the order book is available.
6. Join with precision constants from `modifiers.sol`: `minOrderAmount(token_type)` for minimum order size, `lotSize(token_type)` for amount quantisation, and `TICK_SIZE = 10` for price quantisation. There is no upper bound on price; outcome tokens may trade above one collateral unit.

Market Status computation:

| Market Status | Source condition |
| --- | --- |
| `PENDING` | `EventConfirmed` received; no `TimingsSet` yet. `timings` is `null`. |
| `UPCOMING` | Latest `TimingsSet` exists and `serverTime < timings.stakeStart`. |
| `STAKING` | `timings.stakeStart <= serverTime < timings.stakeEnd`, `PoolsFrozen` not received. |
| `AWAITING_FREEZE` | `serverTime >= timings.stakeEnd`, `PoolsFrozen` not received. |
| `TRADING` | `PoolsFrozen` received and `serverTime < timings.resultStart`. |
| `RESOLVING` | `serverTime >= timings.resultStart`, `Resolved` not received. |
| `RESOLVED` | `PMP.Resolved` received. |
| `CANCELLED` | `PMP.PMPCancelled` or `PMP.EventCancelled` received. |
| `EXPIRED` | `serverTime >= timings.resultEnd` without resolution. |

Timing fields:

| Field | Source | Nullability |
| --- | --- | --- |
| `stakeStart` | Latest `PMP.TimingsSet` | Always present when `timings != null`. |
| `stakeEnd` | Latest `PMP.TimingsSet` | Always present when `timings != null`. |
| `resultStart` | Latest `PMP.TimingsSet` | Always present when `timings != null`. The contract may emit `TimingsSet` repeatedly while `serverTime < resultStart`; take the latest by block time. |
| `resultEnd` | Latest `PMP.TimingsSet` | Always present when `timings != null`. |
| `frozenAt` | Block timestamp of `PMP.PoolsFrozen` | `null` for `UPCOMING`, `STAKING`, and `AWAITING_FREEZE`. |

`event` is sourced from `OracleEventList.EventAdded(eventId, eventName, oracleFee, deadline)` and `addEvent.describe`. `eventName` and `description` are the user-facing labels for the market.

Terminal mapping:

| API status | `terminal.kind` | Trigger |
| --- | --- | --- |
| `RESOLVED` | `RESOLVED` | `PMP.Resolved` received. Sets `resolvedOutcomeId`. |
| `CANCELLED` | `CANCELLED` | `PMP.PMPCancelled` or `PMP.EventCancelled` received. Sets `cancelReason`. |
| `EXPIRED` | `EXPIRED` | `serverTime >= timings.resultEnd` reached without resolution. |

`cancelReason` MUST distinguish cancellation sources. `PMP_CANCELLED` means this specific market was cancelled by the PMP; `EVENT_CANCELLED` means the underlying oracle event was cancelled, which kills every market attached to it. These reasons come from different contract events and have different UI meaning.

Semantic invariants:

1. `status == "TRADING"` implies `timings.frozenAt != null && serverTime < timings.resultStart`.
2. `status == "RESOLVING"` implies `timings.frozenAt != null && timings.resultStart <= serverTime < timings.resultEnd`.
3. `status == "PENDING"` implies `timings == null`.
4. `status == "RESOLVED"` implies `terminal.kind == "RESOLVED" && timings.frozenAt != null`; resolution always follows freeze, see `PMP.sol:1005`.
5. `orderBookAddress` is the deterministic OrderBook address and may be known before the OrderBook contract is active on-chain; once non-null it is stable. Order-book availability still implies `timings.frozenAt != null` and the appropriate market `status` - a non-null `orderBookAddress` alone does not imply the book is open.

If any invariant is violated, the backend MUST fail the request closed rather than return an inconsistent market. This protects clients against indexer desyncs.

Endpoint design notes:

- The endpoint does not return derived fields such as `tradingDuration`, `phaseStartedAt`, `timeRemaining`, or `expectedTradingStart`; clients compute these from `timings` and `serverTime`. Duplicating them server-side creates a desync source.
- The endpoint does not return history of `TimingsSet` updates. It always returns the latest `TimingsSet`. If history becomes necessary it will live under `/api/v1/markets/{address}/timings/history`.
- The endpoint does not return raw contract flags such as `approved`, `frozen`, or `numberOfOracleEvents`. Clients act on `status`. A future `/api/v1/markets/{address}/raw` endpoint may expose them for debugging.

### `/api/v1/depth`

Compute the OrderBook address via `DexLib.computeOrderBookAddress(PrivateNoteCode, orderBookCode, event_id, ohash, token_type)`. Fetch `OrderBook._state` via a raw account query and decode it offchain; the format is `next_order_id(16) + num_orders(4) + N * 126-byte order records`. Group records by `outcomeId` and side, sort by price, and sum amount per level. For a single order, use the `OrderBook.getOrder(uint128 orderId)` getter.
