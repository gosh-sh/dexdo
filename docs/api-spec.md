- [Dodex REST API Specification](#dodex-rest-api-specification)
  - [Base URL](#base-url)
  - [Data Types](#data-types)
  - [Symbol Model](#symbol-model)
  - [Security Types](#security-types)
    - [Signature Formation](#signature-formation)
  - [Error Response](#error-response)
  - [Endpoint Summary](#endpoint-summary)
  - [Market Data Endpoints](#market-data-endpoints)
    - [Markets](#markets)
      - [Common Enums](#common-enums)
        - [Market Status](#market-status)
      - [Common Objects](#common-objects)
        - [Timings](#timings)
        - [Event](#event)
        - [Terminal](#terminal)
        - [Outcome](#outcome)
    - [Order Book](#order-book)
  - [Account Endpoints](#account-endpoints)
    - [Account Balance](#account-balance)
  - [Trading Endpoints](#trading-endpoints)
    - [New Order](#new-order)
    - [Cancel Order](#cancel-order)
    - [New Batch Orders](#new-batch-orders)
    - [Cancel Batch Orders](#cancel-batch-orders)
    - [Cancel All Open Orders On Symbol](#cancel-all-open-orders-on-symbol)
    - [Orders](#orders)
    - [Common Enums](#common-enums-1)
      - [Order Side](#order-side)
      - [Order Type](#order-type)
      - [Time In Force](#time-in-force)
      - [Order Status](#order-status)
  - [Validation Rules](#validation-rules)

# Dodex REST API Specification

Status: Draft

This document defines the public REST interface required for basic spot-style trading on Dodex.

## Base URL

```text
https://api.dodex.example.com
```

All endpoints use JSON responses.

```http
Content-Type: application/json
```

## Data Types

| Type | Description | Example |
| --- | --- | --- |
| `STRING` | UTF-8 string | `"PM-2026-ELECTION-NO"` |
| `DECIMAL` | Decimal number encoded as string | `"0.615"` |
| `LONG` | Integer timestamp or ID | `1710000000000` |
| `INT` | JSON integer | `5` |
| `BOOLEAN` | JSON boolean | `true` |
| `ARRAY` | JSON array | `[]` |
| `OBJECT` | JSON object | `{}` |

All asset amounts and human-facing prices MUST be encoded as strings to avoid
floating-point precision loss.

## Symbol Model

Dodex uses the following market identifiers:

- `marketAddress` is the stable market identifier across the entire lifecycle. Used in all market-specific requests. Example: `0:market-address`.
- `orderBookAddress` is the deterministic order-book address returned by `/api/v1/markets`. It is always present on any market that appears in API responses — the backend stamps it on the first reconcile, before the OrderBook contract is active on-chain. The only state where it can be null internally is the pre-reconcile window, and such markets are hidden from the API. Clients MUST use `status` to determine whether the order book is currently available for trading; a non-null `orderBookAddress` does not by itself imply the book is open.
- `marketName` is the market name. Example: `PM-2026-ELECTION`.
- `symbol` is the outcome-token symbol and is formed as `<marketName>-<OUTCOME_NAME>`. Example: `PM-2026-ELECTION-YES`.

Requests that target one order book use `marketAddress` and `symbol`.
Responses return the same identifiers where relevant.

Examples:

```text
marketAddress = 0:market-address
marketName    = PM-2026-ELECTION
symbol        = PM-2026-ELECTION-YES
```


## Security Types

| Security | Description |
| --- | --- |
| `NONE` | Public endpoint. No authentication required. |
| `USER_DATA` | Requires account authentication. Used for balances and order history. |
| `TRADE` | Requires account authentication and trading permission. Used for order creation and cancellation. |

Private endpoints require:

| Location | Name | Type | Mandatory | Description |
| --- | --- | --- | --- | --- |
| Header | `X-DODEX-APIKEY` | STRING | YES | API key or API token issued by the Dodex backend. |
| Query | `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| Query | `recvWindow` | LONG | NO | Request validity window in milliseconds. Default: `5000`. Max: `60000`. |
| Query | `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

### Signature Formation

Private requests are signed with the API secret that belongs to the provided `X-DODEX-APIKEY`.
The API secret MUST NOT be sent in any request.

Signature payload:

```text
canonicalQueryString + canonicalRequestBody
```

- `canonicalQueryString` contains all query parameters except `signature`, sorted by key.
- `canonicalRequestBody` is the exact minified JSON body string sent over the wire for requests with a body, or an empty string.
- JSON keys in `canonicalRequestBody` are not re-sorted. The signature MUST be computed over the same key order and bytes that are sent in the request body.
- The signature is HMAC SHA256 using the API secret.

Formula:

```text
signature = HMAC_SHA256(canonicalQueryString + canonicalRequestBody, apiSecret)
```

Example for `POST /api/v1/order`:

```text
canonicalQueryString = recvWindow=5000&timestamp=1710000000000
canonicalRequestBody = {"marketAddress":"0:market-address","symbol":"PM-2026-ELECTION-YES","side":"BUY","quantity":"1.500000","price":"0.615","type":"LIMIT","timeInForce":"GTC"}

signature = HMAC_SHA256(
  'recvWindow=5000&timestamp=1710000000000{"marketAddress":"0:market-address","symbol":"PM-2026-ELECTION-YES","side":"BUY","quantity":"1.500000","price":"0.615","type":"LIMIT","timeInForce":"GTC"}',
  apiSecret
)
```

## Error Response

All errors use the same response format.

```json
{
  "code": -1121,
  "msg": "Invalid market or symbol."
}
```

Recommended common error codes:

| Code | Message | HTTP |
| --- | --- | --- |
| `-1000` | Unknown error. | 500 |
| `-1002` | Authentication required. | 401 |
| `-1003` | Required auth parameter missing. | 401 |
| `-1007` | Request timed out before completion. | 504 |
| `-1009` | Request body too large. | 413 |
| `-1021` | Timestamp outside recvWindow. | 401 |
| `-1022` | Invalid signature. | 401 |
| `-1102` | Mandatory parameter was not sent. | 400 |
| `-1111` | Precision is over the maximum defined for this asset. | 400 |
| `-1121` | Invalid market or symbol. | 404 |
| `-1130` | Invalid value for a query or body parameter. | 400 |
| `-1500` | Market data is temporarily inconsistent. | 503 |
| `-2010` | Order would immediately fail validation. | 400 |
| `-2011` | Unknown order. | 404 |
| `-2014` | Trading note busy with a previous order; retry shortly. | 429 |

`-1007` means the request did not complete in time. The order may still have been accepted by the exchange. Retry `POST /api/v1/order` with the same `newOrderClientId` — the server will deduplicate so the same order is not placed twice.

`-2014` means another order from the same account is still being processed. Retry after a short delay; the in-flight order will appear in `/api/v1/orders` shortly.

Authentication errors are split intentionally: `-1003` signals a malformed
request envelope (missing or unparseable `X-DODEX-APIKEY`, `timestamp`,
`signature`, or `recvWindow`) — the server could not even attempt
verification. `-1002` signals that verification was attempted and the
credential was rejected (unknown api_key, disabled key, or key lacks the
required permission). `msg` returns generic copy and never identifies which
envelope field failed or why a credential was rejected.

## Endpoint Summary

| Function | Method | Path | Security |
| --- | --- | --- | --- |
| List markets | `GET` | `/api/v1/markets` | `NONE` |
| Fetch order book | `GET` | `/api/v1/depth` | `NONE` |
| Fetch account balance | `GET` | `/api/v1/account` | `USER_DATA` |
| Create single order | `POST` | `/api/v1/order` | `TRADE` |
| Cancel single order by ID | `DELETE` | `/api/v1/order` | `TRADE` |
| Create batch orders | `POST` | `/api/v1/batchOrders` | `TRADE` |
| Cancel batch orders by IDs | `DELETE` | `/api/v1/batchOrders` | `TRADE` |
| Cancel all open orders on one symbol | `DELETE` | `/api/v1/openOrders` | `TRADE` |
| Fetch orders | `GET` | `/api/v1/orders` | `USER_DATA` |

## Market Data Endpoints

### Markets

```http
GET /api/v1/markets
```

Fetch available prediction markets, their outcomes, lifecycle phase, timings, and oracle event metadata.

A market in Dodex has a finite lifecycle anchored to an oracle event. The lifecycle has nine phases — see [Market Status](#market-status). Clients MUST treat `status` as an opaque enum value and not derive it from raw timings.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | NO | Return one market only. Mutually exclusive with the filter and pagination parameters below. |
| `status` | STRING | NO | Comma-separated list of statuses to include. Example: `TRADING,AWAITING_FREEZE`. |
| `quoteAsset` | STRING | NO | Filter by quote asset. Example: `USDC`. |
| `oracleName` | STRING | NO | Filter by oracle name. A market matches if **any** of its confirming oracles has this name — a multi-oracle PMP is included as long as one of its `event.oracles[]` entries matches. |
| `closingBefore` | LONG | NO | Return only markets with `timings.resultEnd < closingBefore` (unix seconds). |
| `sort` | STRING | NO | Sort field. One of: `resultStart` (default, ASC), `createdAt` (DESC). |
| `cursor` | STRING | NO | Opaque pagination cursor returned by a previous call. |
| `limit` | INT | NO | Page size. Default: `50`. Max: `200`. |

Response:

```json
{
  "serverTime": 1710000000,
  "nextCursor": "eyJ...",
  "hasMore": false,
  "markets": [
    {
      "marketAddress": "0:b286...",
      "orderBookAddress": "0:c12d...",
      "marketName": "PM-2026-ELECTION",
      "status": "TRADING",
      "quoteAsset": "USDC",
      "tokenType": 1,
      "createdAt": 1709980000,
      "timings": {
        "stakeStart": 1709990000,
        "stakeEnd": 1709991800,
        "resultStart": 1710008000,
        "resultEnd": 1710011600,
        "frozenAt": 1709991850
      },
      "event": {
        "eventId": "0xabc...",
        "eventName": "2026 US Presidential Election",
        "description": "Will candidate X win?",
        "oracles": [{
          "name": "ElectionOracle",
          "address": "0:oracle-addr",
          "fee": "100"
        }]
      },
      "terminal": null,
      "outcomes": [
        {
          "outcomeId": 0,
          "outcomeName": "NO",
          "symbol": "PM-2026-ELECTION-NO",
          "pricePrecision": 3,
          "quantityPrecision": 2,
          "tickSize": "0.001",
          "stepSize": "0.01",
          "minNotional": "1",
          "maxBatchSize": 5
        },
        {
          "outcomeId": 1,
          "outcomeName": "YES",
          "symbol": "PM-2026-ELECTION-YES",
          "pricePrecision": 3,
          "quantityPrecision": 2,
          "tickSize": "0.001",
          "stepSize": "0.01",
          "minNotional": "1",
          "maxBatchSize": 5
        }
      ]
    }
  ]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `serverTime` | LONG | Unix timestamp in seconds. All timestamp fields returned by `/api/v1/markets` are unix seconds unless explicitly stated otherwise. |
| `nextCursor` | STRING \| null | Pagination cursor for the next page. `null` when `hasMore` is `false`. |
| `hasMore` | BOOLEAN | Whether more pages follow. |
| `marketAddress` | STRING | Stable market identifier. |
| `orderBookAddress` | STRING | Deterministic order-book address. Always present on markets visible to the API (the backend stamps it on the first reconcile). Trading availability depends on market `status`. |
| `marketName` | STRING | Technical market name. Not the user-facing title; see `event.eventName`. |
| `status` | ENUM | Market phase. See [Market Status](#market-status). |
| `quoteAsset` | STRING | Quote-asset symbol for display. |
| `tokenType` | INT | Numeric quote-asset token type. |
| `createdAt` | LONG | Unix seconds. Market creation timestamp. Used for sorting by recency. |
| `timings` | OBJECT \| null | See [Timings](#timings). `null` when `status == "PENDING"`. |
| `event` | OBJECT | See [Event](#event). |
| `terminal` | OBJECT \| null | See [Terminal](#terminal). `null` for non-terminal statuses. |
| `outcomes` | ARRAY | Outcome-token descriptors. See [Outcome](#outcome). |

#### Common Enums

##### Market Status

A market is in exactly one of nine phases.

| Value | Description |
| --- | --- |
| `PENDING` | Market exists, but timings are not set yet. `timings` is `null`. |
| `UPCOMING` | Timings are set and staking has not started yet. |
| `STAKING` | Staking is open. |
| `AWAITING_FREEZE` | Staking has ended and trading has not started yet. |
| `TRADING` | Orders may be placed. |
| `RESOLVING` | Trading is closed and the market is waiting for resolution. |
| `RESOLVED` | Terminal. Market has a winning outcome. |
| `CANCELLED` | Terminal. Market or underlying event was cancelled. |
| `EXPIRED` | Terminal. Market reached the result deadline without resolution. |

#### Common Objects

##### Timings

All timestamps are unix seconds.

```json
{
  "stakeStart": 1709990000,
  "stakeEnd": 1709991800,
  "resultStart": 1710008000,
  "resultEnd": 1710011600,
  "frozenAt": 1709991850
}
```

| Field | Type | Description |
| --- | --- | --- |
| `stakeStart` | LONG | Unix seconds. Always present when `timings != null`. |
| `stakeEnd` | LONG | Unix seconds. Always present when `timings != null`. |
| `resultStart` | LONG | Unix seconds. Always present when `timings != null`. |
| `resultEnd` | LONG | Unix seconds. Always present when `timings != null`. |
| `frozenAt` | LONG \| null | Unix seconds. `null` before the order book is active. |

`timings` itself is `null` only for `PENDING`.

##### Event

```json
{
  "eventId": "0xabc...",
  "eventName": "2026 US Presidential Election",
  "description": "Will candidate X win?",
  "oracles": [
    {
      "name": "ElectionOracle",
      "address": "0:oracle-a",
      "fee": "100"
    },
    {
        "name": "BackupElectionOracle",
        "address": "0:oracle-b",
        "fee": "200"
    }
  ]
}
```

| Field | Type | Description |
| --- | --- | --- |
| `eventId` | STRING | `0x`-prefixed uint256 hex digest. Computed on-chain as a hash of `eventName`, `description`, `deadline`, `outcomeNames`; therefore identical across every oracle that confirms the same event. |
| `eventName` | STRING \| null | User-facing event title. Shared across all confirming oracles by the hash invariant above. `null` until at least one `EventAdded` has landed. |
| `description` | STRING \| null | User-facing description. Same shared-by-hash invariant as `eventName`. |
| `oracles` | ARRAY of [OracleEntry](#oracleentry) | One entry per oracle that confirmed this PMP. A PMP can require confirmation from multiple `OracleEventList` contracts; each adds an entry with its own `fee`. Empty array means no oracle has confirmed yet (the row exists in `markets` but no `EventConfirmed` has landed). |

###### OracleEntry

| Field | Type | Description |
| --- | --- | --- |
| `name` | STRING \| null | Oracle name from `oracles.name`. `null` if the indexer has not yet reconciled the oracle row. |
| `address` | STRING \| null | Oracle contract address. |
| `fee` | DECIMAL \| null | Oracle fee for this confirmation, as a uint128 decimal string. Different oracles can charge different fees for the same event. |

If any two entries in `oracles[]` for the same market disagree on `eventName` or `description`, the backend fails the request closed with `MarketInconsistent` (HTTP 503) — that disagreement contradicts the hash invariant and indicates indexer corruption.

##### Terminal

Describes how the market ended.

`terminal` is `null` while the market is alive (`status` ∈ `PENDING`, `UPCOMING`, `STAKING`, `AWAITING_FREEZE`, `TRADING`, `RESOLVING`) and is populated when `status` ∈ `RESOLVED`, `CANCELLED`, `EXPIRED`. The `status` field tells the client _that_ the market is terminal; `terminal` tells the client _how_ — the winning outcome, the reason for cancellation, and when it happened.

Mapping from `status` to `kind`:

| `status` | `terminal.kind` | Description |
| --- | --- | --- |
| `RESOLVED` | `RESOLVED` | `resolvedOutcomeId` is set. |
| `CANCELLED` | `CANCELLED` | `cancelReason` is set. |
| `EXPIRED` | `EXPIRED` | Market reached `timings.resultEnd` without resolution. |

Example for a resolved market (`status == "RESOLVED"`):

```json
{
  "kind": "RESOLVED",
  "at": 1710010000,
  "resolvedOutcomeId": 1,
  "cancelReason": null
}
```

Example for a cancelled market (`status == "CANCELLED"`):

```json
{
  "kind": "CANCELLED",
  "at": 1710003500,
  "resolvedOutcomeId": null,
  "cancelReason": "EVENT_CANCELLED"
}
```

Example for a non-terminal market (any of the six live statuses, including the top-level `/markets` example which is `TRADING`):

```json
"terminal": null
```

| Field | Type | Description |
| --- | --- | --- |
| `kind` | ENUM | `RESOLVED`, `CANCELLED`, or `EXPIRED`. Mirrors `status` (see table above). |
| `at` | LONG | Unix seconds. Time when the market entered the terminal state. |
| `resolvedOutcomeId` | INT \| null | The winning outcome's `outcomeId`. Present only when `kind == "RESOLVED"`; `null` otherwise. Without it the client cannot know which side won. |
| `cancelReason` | ENUM \| null | `PMP_CANCELLED` or `EVENT_CANCELLED`. Present only when `kind == "CANCELLED"`; `null` otherwise. |

##### Outcome

```json
{
  "outcomeId": 0,
  "outcomeName": "NO",
  "symbol": "PM-2026-ELECTION-NO",
  "pricePrecision": 3,
  "quantityPrecision": 2,
  "tickSize": "0.001",
  "stepSize": "0.01",
  "minNotional": "1",
  "maxBatchSize": 5
}
```

| Field | Type | Description |
| --- | --- | --- |
| `outcomeId` | INT | Stable outcome ID. Clients MUST use this field, not the array index. |
| `outcomeName` | STRING | Outcome name. |
| `symbol` | STRING | Outcome-token symbol used in trading and order-book requests. |
| `pricePrecision` | INT | Maximum number of decimal places accepted for order prices. |
| `quantityPrecision` | INT | Maximum number of decimal places accepted for order quantities. |
| `tickSize` | DECIMAL | Minimum price increment. |
| `stepSize` | DECIMAL | Minimum quantity increment. |
| `minNotional` | DECIMAL | Minimum accepted notional value for an order. |
| `maxBatchSize` | INT | Maximum number of orders accepted in one batch request for this outcome. |

### Order Book

```http
GET /api/v1/depth
```

Fetch bids and asks for one symbol in one market.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `limit` | INT | NO | Number of price levels per side. Default: `100`. Max: `1000`. |

Response:

```json
{
  "marketAddress": "0:market-address",
  "symbol": "PM-2026-ELECTION-YES",
  "lastUpdateId": "5f8000000000017c5a",
  "bids": [
    ["0.614", "100.00"],
    ["0.613", "25.50"]
  ],
  "asks": [
    ["0.616", "50.00"],
    ["0.617", "75.25"]
  ]
}
```

| Field | Type | Description |
| --- | --- | --- |
| `lastUpdateId` | STRING | Opaque chain-order cursor. Lex-comparable: a larger string means a newer event has touched this `(marketAddress, symbol)`. Empty string when no order event has landed yet. Clients SHOULD compare for equality to detect "no change" and string-lex order to detect "moved forward"; they SHOULD NOT parse it as an integer. |

Each bid or ask item is:

```text
[price, quantity]
```

## Account Endpoints

### Account Balance

```http
GET /api/v1/account
```

Security: `USER_DATA`

Fetch account collateral balances and outcome-token balances.

Parameters:

No endpoint-specific parameters.

Signed parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "accountId": "account-1",
  "updateTime": 1710000000000,
  "balances": [
    {
      "asset": "NACKL",
      "free": "10.000000",
      "locked": "1.500000"
    },
    {
      "asset": "USDC",
      "free": "25000.00",
      "locked": "3750.00"
    }
  ],
  "outcome_balances": [
    {
      "marketAddress": "0:market-address",
      "symbol": "PM-2026-ELECTION-YES",
      "free": "10.00",
      "lockedInOrders": "1000.00"
    }
  ]
}
```

## Trading Endpoints

### New Order

```http
POST /api/v1/order
```

Security: `TRADE`

Create a single order.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `newOrderClientId` | STRING | NO | Optional client-defined order identifier. If omitted, the API generates a random value and returns it as `clientOrderId` in the response. |
| `side` | ENUM | YES | Order side. See [Order Side](#order-side). |
| `quantity` | DECIMAL | YES | Outcome-token quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the amount of the market `quoteAsset` to spend, for example `USDC`. In that case, `quantityPrecision` and `stepSize` from `/api/v1/markets` apply to the quote-asset spend amount exactly as sent in the request. |
| `price` | DECIMAL | NO | Required for `LIMIT` orders. Must follow `tickSize`. |
| `type` | ENUM | NO | Order type. See [Order Type](#order-type). Default: `LIMIT`. |
| `timeInForce` | ENUM | NO | For `LIMIT` orders only. See [Time In Force](#time-in-force). Default: `GTC`. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "clientOrderId": "mm-order-0001",
  "transactTime": 1710000000000,
  "status": "PENDING_NEW"
}
```

Response fields:

| Name | Type | Description |
| --- | --- | --- |
| `clientOrderId` | STRING | Either the `newOrderClientId` sent in the request, or a server-generated identifier when none was provided. Use it to look up the order in `/api/v1/orders` until the server-assigned `orderId` is available. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the order was accepted. |
| `status` | ENUM | Always [`PENDING_NEW`](#order-status) on success. |

The response confirms acceptance only. The full order state — `orderId`, fills, accepted price — becomes available through [`GET /api/v1/orders`](#orders) shortly after; look up by `clientOrderId`.

### Cancel Order

```http
DELETE /api/v1/order
```

Security: `TRADE`

Cancel a single open order by ID.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `orderId` | STRING | YES | Order ID to cancel. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "orderId": "123456789",
  "clientOrderId": "mm-order-0001",
  "transactTime": 1710000000000,
  "status": "PENDING_CANCEL"
}
```

Response fields:

| Name | Type | Description |
| --- | --- | --- |
| `orderId` | STRING | The `orderId` from the request, echoed for correlation. |
| `clientOrderId` | STRING | The order's `clientOrderId` as recorded on placement. Empty string if the order was placed without one. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the cancel request was accepted. |
| `status` | ENUM | Always [`PENDING_CANCEL`](#order-status) on success. |

The response confirms acceptance only. The final outcome — `CANCELED`, or `FILLED` if matching raced the cancel — becomes visible through [`GET /api/v1/orders`](#orders) shortly after.

### New Batch Orders

```http
POST /api/v1/batchOrders
```

Security: `TRADE`

Create multiple orders in one request. All orders in the batch are submitted to the single market symbol identified by the top-level `marketAddress` and `symbol`.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `orders` | ARRAY | YES | List of orders to create on the specified market symbol. Must contain at least one item; the maximum is the outcome's `maxBatchSize` from `/api/v1/markets`. The backend rejects an empty array before submission with `-1130 / 400`. |

Each order item:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `newOrderClientId` | STRING | NO | Optional client-defined order identifier. If omitted, the API generates a random value and returns it as `clientOrderId` in the response. Each item is generated or accepted independently; intra-batch duplicates are detected by the exchange during placement and surface as `-1130 / 400`. |
| `side` | ENUM | YES | Order side. See [Order Side](#order-side). |
| `quantity` | DECIMAL | YES | Outcome-token quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the amount of the market `quoteAsset` to spend, for example `USDC`. In that case, `quantityPrecision` and `stepSize` from `/api/v1/markets` apply to the quote-asset spend amount exactly as sent in the request. |
| `price` | DECIMAL | NO | Required for `LIMIT` orders. Must follow `tickSize`. |
| `type` | ENUM | NO | Order type. See [Order Type](#order-type). Default: `LIMIT`. |
| `timeInForce` | ENUM | NO | For `LIMIT` orders only. See [Time In Force](#time-in-force). Default: `GTC`. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Request body:

```json
{
  "marketAddress": "0:market-address",
  "symbol": "PM-2026-ELECTION-YES",
  "orders": [
    {
      "newOrderClientId": "1001",
      "side": "BUY",
      "quantity": "1.500000",
      "price": "0.615",
      "type": "LIMIT",
      "timeInForce": "GTC"
    },
    {
      "newOrderClientId": "1002",
      "side": "SELL",
      "quantity": "0.750000",
      "price": "0.620",
      "type": "LIMIT",
      "timeInForce": "POST_ONLY"
    }
  ]
}
```

Response:

```json
[
  {
    "clientOrderId": "1001",
    "transactTime": 1710000000000,
    "status": "PENDING_NEW"
  },
  {
    "clientOrderId": "1002",
    "transactTime": 1710000000000,
    "status": "PENDING_NEW"
  }
]
```

Response items, in request order:

| Name | Type | Description |
| --- | --- | --- |
| `clientOrderId` | STRING | Either the `newOrderClientId` sent in the corresponding request item, or a server-generated identifier when none was provided. Use it to look up the order in `/api/v1/openOrders` until the server-assigned `orderId` is available. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the batch was accepted. The same value is repeated for every item in the response. |
| `status` | ENUM | Always [`PENDING_NEW`](#order-status) on success. |

The response confirms acceptance only. For each item, the full order state — `orderId`, fills, accepted price — becomes available through [`GET /api/v1/openOrders`](#current-open-orders) shortly after; look up by `clientOrderId`.

Response shape depends on outcome: on success the body is a JSON array with one object per accepted item, in request order; on failure the body is a single standard error envelope (`{ "code": ..., "msg": ... }`), never an array.

Batch creation is atomic: if any order in the batch fails validation, the whole request is rejected and no orders are created. Validation runs item by item in request order and the first failure returns its error code. Atomicity holds on the exchange side too — even after the request is accepted, if the exchange rejects any item during placement the whole batch is reverted.

### Cancel Batch Orders

```http
DELETE /api/v1/batchOrders
```

Security: `TRADE`

Cancel multiple open orders by IDs.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `orderIds` | ARRAY | YES | List of order IDs to cancel on the specified market symbol. Must contain at least one item; the maximum is the outcome's `maxBatchSize` from `/api/v1/markets`. The backend rejects an empty array before submission with `-1130 / 400`. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Request body:

```json
{
  "marketAddress": "0:market-address",
  "symbol": "PM-2026-ELECTION-YES",
  "orderIds": ["123456789", "123456790"]
}
```

Response:

```json
[
  {
    "orderId": "123456789",
    "clientOrderId": "mm-order-0001",
    "transactTime": 1710000000000,
    "status": "PENDING_CANCEL"
  },
  {
    "orderId": "123456790",
    "clientOrderId": "mm-order-0002",
    "transactTime": 1710000000000,
    "status": "PENDING_CANCEL"
  }
]
```

Response fields (one element per requested `orderId`, in request order):

| Name | Type | Description |
| --- | --- | --- |
| `orderId` | STRING | The `orderId` from the request, echoed for correlation. |
| `clientOrderId` | STRING | The order's `clientOrderId` as recorded on placement. Empty string if the order was placed without one. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the cancel batch was accepted. Identical across every item — one chain submission, one moment of acceptance. |
| `status` | ENUM | Always [`PENDING_CANCEL`](#order-status) on success. |

The response confirms acceptance only. The final outcome per id — `CANCELED`, or `FILLED` if matching raced the cancel — becomes visible through [`GET /api/v1/orders`](#orders) shortly after.

### Cancel All Open Orders On Symbol

```http
DELETE /api/v1/openOrders
```

Security: `TRADE`

Cancel all open orders on one market symbol.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
[
  {
    "marketAddress": "0:market-address",
    "symbol": "PM-2026-ELECTION-YES",
    "orderId": "123456789",
    "price": "0.615",
    "origQty": "1.500000",
    "executedQty": "0.000000",
    "status": "CANCELED",
    "timeInForce": "GTC",
    "type": "LIMIT",
    "side": "BUY",
    "time": 1710000000000,
    "updateTime": 1710000010000
  }
]
```

### Orders

```http
GET /api/v1/orders
```

Security: `USER_DATA`

Fetch orders for the authenticated account across all lifecycle states — currently open, filled, canceled, and (once produced by the indexer) rejected. The response never includes orders owned by another account.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | NO | Market address. Together with `symbol`, selects one market symbol. If both are omitted, returns orders across all markets. |
| `symbol` | STRING | NO | Outcome-token symbol. Together with `marketAddress`, selects one market symbol. If one is sent without the other, the request is invalid. |
| `status` | STRING | NO | Comma-separated list of [Order Status](#order-status) values to include. Allowed: `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `REJECTED`. Tokens are trimmed and de-duplicated. Default: include all statuses. |
| `limit` | INT | NO | Page size. Default: `100`. Range: `[1, 500]`. |
| `cursor` | STRING | NO | Opaque lex-comparable string returned by the server as `nextCursor`. Pass back verbatim to fetch the next page. An empty or whitespace-only cursor returns `-1102 / 400`. A well-formed cursor that lies past the last order returns an empty page with `nextCursor: null` — not an error. Omit for the first page. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Behavior:

- If both `marketAddress` and `symbol` are omitted, returns orders for the authenticated account across all markets.
- If both `marketAddress` and `symbol` are sent, returns orders for that one market symbol.
- If only one of `marketAddress` / `symbol` is sent, returns `-1102` with HTTP `400`.
- If `limit` is outside `[1, 500]`, returns `-1102` with HTTP `400`.
- If `limit` is present but not an integer, returns `-1130` with HTTP `400`.
- If `cursor` is empty or whitespace-only, returns `-1102` with HTTP `400`. A well-formed cursor that points past the current set of orders is not an error — the response is `{ "orders": [], "nextCursor": null }`.
- If `status` contains an unknown token, returns `-1130` with HTTP `400`.
- If the `(marketAddress, symbol)` pair does not exist, returns `-1121` with HTTP `404`.
- Empty results are returned as `{ "orders": [], "nextCursor": null }`. A page may also return `orders: []` together with a non-null `nextCursor`; clients should keep paging until `nextCursor` is `null`.
- Results are sorted by a single server-internal chain-order key, **descending** (most recently placed first). For all-market requests this ordering is global across all returned orders.
- Pagination is cursor-based on the same chain-order key. The key is set once when the order is placed and never moves for the life of the order — subsequent fills, cancels, and status transitions do not touch it — so concurrent activity between page reads cannot duplicate or skip rows.
- The endpoint is eventually consistent: a freshly placed order may briefly not appear, between the time the public `OrderPlaced` event is indexed and the time the private confirmation that carries owner attribution is indexed.

Response:

```json
{
  "orders": [
    {
      "marketAddress": "0:market-address",
      "symbol": "PM-2026-ELECTION-YES",
      "orderId": "123456789",
      "clientOrderId": "mm-order-0001",
      "price": "0.615",
      "origQty": "1.500000",
      "executedQty": "0.500000",
      "status": "PARTIALLY_FILLED",
      "timeInForce": "GTC",
      "type": "LIMIT",
      "side": "BUY",
      "time": 1710000000000,
      "updateTime": 1710000001000
    }
  ],
  "nextCursor": "5f8000000000017c5a"
}
```

Top-level response fields:

| Name | Type | Description |
| --- | --- | --- |
| `orders` | ARRAY | Orders matching the filter, in stable chain-order descending. Empty when there are no matches. |
| `nextCursor` | STRING \| null | Opaque lex-comparable pagination cursor. Pass back verbatim to fetch the next page; do not parse or generate. `null` when the last page has been returned. |

Order fields:

| Name | Type | Description |
| --- | --- | --- |
| `marketAddress` | STRING | Market address. |
| `symbol` | STRING | Outcome-token symbol. |
| `orderId` | STRING | Chain-side order id. Empty string for `REJECTED` orders (the chain never assigns an id to a rejected placement). |
| `clientOrderId` | STRING | Client-supplied id, or an empty string if absent. |
| `price` | DECIMAL | Limit price, scaled by the outcome price precision. |
| `origQty` | DECIMAL | Original order quantity, scaled by the outcome quantity precision. |
| `executedQty` | DECIMAL | Filled quantity, scaled by the outcome quantity precision. Can be `> 0` for `CANCELED` orders that filled partially before cancellation. Always `0` for `NEW`, `REJECTED`. |
| `status` | ENUM | One of `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `REJECTED`. |
| `timeInForce` | ENUM | `GTC`. |
| `type` | ENUM | `LIMIT`. |
| `side` | ENUM | `BUY` or `SELL`. |
| `time` | LONG | On-chain order creation time in Unix milliseconds, truncated from the indexed microsecond timestamp. Stable under indexer backlog. |
| `updateTime` | LONG | On-chain time of the most recent book event that touched the order (place / fill / cancel), in Unix milliseconds, truncated from the indexed microsecond timestamp. |

### Common Enums

#### Order Side

| Value | Description |
| --- | --- |
| `BUY` | Buy the selected outcome token using the market quote asset. |
| `SELL` | Sell the selected outcome token for the market quote asset. |

#### Order Type

| Value | Description |
| --- | --- |
| `LIMIT` | Limit order. Unfilled quantity remains in the order book. |
| `MARKET` | Market order. Matches available liquidity and does not rest in the order book. |

#### Time In Force

`timeInForce` values:

| Value | Description |
| --- | --- |
| `GTC` | Good Til Canceled. An order will be on the book unless the order is canceled. |
| `IOC` | Immediate Or Cancel. An order will try to fill the order as much as it can before the order expires. |
| `FOK` | Fill or Kill. An order will expire if the full order cannot be filled upon execution. |
| `POST_ONLY` | Post Only. An order must rest on the book and will be canceled if it would immediately match. |

#### Order Status

| Value | Description |
| --- | --- |
| `PENDING_NEW` | Order accepted by the exchange and not yet on the book. Will transition to `NEW` (or `PARTIALLY_FILLED` if it immediately matches) once visible in `/api/v1/orders`. |
| `NEW` | Order is open and has no fills. |
| `PARTIALLY_FILLED` | Order is open and partially filled. |
| `PENDING_CANCEL` | Cancel request accepted by the exchange but not yet applied to the book. Will transition to `CANCELED` (or `FILLED` if matching raced the cancel) once the order's stored status flips in `/api/v1/orders`. |
| `FILLED` | Order is completely filled. |
| `CANCELED` | Order was canceled by the user or system. |
| `REJECTED` | Order was rejected and was not opened. |

## Validation Rules

Order creation MUST validate:

| Rule | Source |
| --- | --- |
| `marketAddress` exists | `/api/v1/markets` |
| `symbol` exists within the selected market | `/api/v1/markets` |
| The selected market has `status == "TRADING"` (any other phase rejects order placement) | `/api/v1/markets` |
| For `LIMIT` orders, `price` decimal places do not exceed `pricePrecision` | `/api/v1/markets` |
| For `LIMIT` orders, `price` is a multiple of `tickSize` | `/api/v1/markets` |
| `quantity` decimal places do not exceed `quantityPrecision` | `/api/v1/markets` |
| `quantity` is a multiple of `stepSize` | `/api/v1/markets` |
| For `LIMIT` orders, `price * quantity` is at least `minNotional` | `/api/v1/markets` |
| For `MARKET` buy orders, `quantity` in the market `quoteAsset` is at least `minNotional` | `/api/v1/markets` |
| For `MARKET` buy orders, `quantityPrecision` and `stepSize` apply to the quote-asset spend amount, not to outcome-token units | `/api/v1/markets` |
| Account has enough available balance | `/api/v1/account` |
