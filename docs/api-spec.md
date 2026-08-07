- [DEX.DO REST API Specification](#dexdo-rest-api-specification)
  - [Base URL](#base-url)
  - [Data Types](#data-types)
  - [Symbol Model](#symbol-model)
  - [Security Types](#security-types)
    - [Signature Formation](#signature-formation)
  - [Error Response](#error-response)
  - [Endpoint Summary](#endpoint-summary)
  - [Prediction Market Data](#prediction-market-data)
    - [Prediction Markets](#prediction-markets)
      - [Common Enums](#common-enums)
        - [Market Status](#market-status)
      - [Common Objects](#common-objects)
        - [Timings](#timings)
        - [Event](#event)
          - [OracleEntry](#oracleentry)
        - [Terminal](#terminal)
        - [Outcome](#outcome)
        - [resolvesFrom](#resolvesfrom)
    - [Prediction Order Book](#prediction-order-book)
    - [Prediction Trades](#prediction-trades)
  - [Oracles](#oracles)
    - [Oracle Data Objects](#oracle-data-objects)
      - [OracleEntry](#oracleentry-1)
      - [OracleEventList](#oracleeventlist)
      - [OracleEvent](#oracleevent)
      - [OracleOutcome](#oracleoutcome)
  - [Inference Market Data](#inference-market-data)
    - [Inference Markets](#inference-markets)
    - [Inference Depth](#inference-depth)
    - [Inference Orders](#inference-orders)
    - [Inference Trades](#inference-trades)
  - [Account Endpoints](#account-endpoints)
    - [Account Balance](#account-balance)
    - [Market Outcome Balances](#market-outcome-balances)
  - [Prediction Position Endpoints](#prediction-position-endpoints)
    - [Buy Full Set](#buy-full-set)
    - [Sell Full Set](#sell-full-set)
    - [Claim](#claim)
  - [Prediction Trading Endpoints](#prediction-trading-endpoints)
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
  - [Prediction WebSocket Streams](#prediction-websocket-streams)
    - [Connection](#connection)
    - [Splice and Gap Detection](#splice-and-gap-detection)
    - [Order Updates](#order-updates)
      - [Common Enums](#common-enums-2)
        - [Execution Type](#execution-type)
  - [Validation Rules](#validation-rules)

# DEX.DO REST API Specification

Status: Draft

This document defines the public REST interface required for basic spot-style trading on DEX.DO.

## Base URL

```text
https://api.dex.do.example.com
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

DEX.DO uses the following market identifiers:

- `predictionMarketAddress` is the stable market identifier across the entire lifecycle. Used in all market-specific requests. Example: `0:market-address`.
- `predictionOrderBookAddress` is the deterministic order-book address returned by `/api/v1/prediction/markets`. It is always present on any market that appears in API responses — the backend stamps it on the first reconcile, before the OrderBook contract is active on-chain. The only state where it can be null internally is the pre-reconcile window, and such markets are hidden from the API. Clients MUST use `status` to determine whether the order book is currently available for trading; a non-null `predictionOrderBookAddress` does not by itself imply the book is open.
- `marketName` is the market name. Example: `PM-2026-ELECTION`.
- `symbol` is the outcome-token symbol and is formed as `<marketName>-<OUTCOME_NAME>`. Example: `PM-2026-ELECTION-YES`.

Requests that target one order book use `predictionMarketAddress` and `symbol`.
Responses return the same identifiers where relevant.

Examples:

```text
predictionMarketAddress = 0:market-address
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
| Header | `X-DODEX-APIKEY` | STRING | YES | API key or API token issued by the DEX.DO backend. |
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

Example for `POST /api/v1/prediction/order`:

```text
canonicalQueryString = recvWindow=5000&timestamp=1710000000000
canonicalRequestBody = {"predictionMarketAddress":"0:market-address","symbol":"PM-2026-ELECTION-YES","side":"BUY","quantity":"1.500000","price":"0.615","type":"LIMIT","timeInForce":"GTC"}

signature = HMAC_SHA256(
  'recvWindow=5000&timestamp=1710000000000{"predictionMarketAddress":"0:market-address","symbol":"PM-2026-ELECTION-YES","side":"BUY","quantity":"1.500000","price":"0.615","type":"LIMIT","timeInForce":"GTC"}',
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
| `-2013` | Account not deployed. | 404 |
| `-2014` | Trading note busy with a previous order; retry shortly. | 429 |
| `-2015` | Private note already registered. | 409 |
| `-2016` | Submitted key does not control this private note. | 400 |

`-1007` means the request did not complete in time. The order may still have been accepted by the exchange. Retry `POST /api/v1/prediction/order` with the same `newOrderClientId` — the server will deduplicate so the same order is not placed twice.

`-2013` means the caller's authenticated account has no PrivateNote contract deployed at its resolved address. The credential is valid but the on-chain contract is missing; the client should offer "deploy your account" instead of retrying.

`-2014` means another order from the same account is still being processed. Retry after a short delay; the in-flight order will appear in `/api/v1/prediction/orders` shortly.

`-2015` means `POST /api/v1/accounts` was called for a PrivateNote that already has an account. Registration is one-shot per note: the existing credential is left untouched and no new one is minted. Do not retry — use the credential issued at first registration.

`-2016` means the submitted secret key does not control the note: the backend derived its public key and it did not match the note's on-chain owner. Send the secret key of the note you are registering — a key that cannot sign for the note would never produce valid trades.

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
| List prediction markets | `GET` | `/api/v1/prediction/markets` | `NONE` |
| List oracles and their available events | `GET` | `/api/v1/oracles` | `NONE` |
| Fetch prediction order book | `GET` | `/api/v1/prediction/depth` | `NONE` |
| Fetch recent prediction trades | `GET` | `/api/v1/prediction/trades` | `NONE` |
| List inference markets (tradable models) | `GET` | `/api/v1/inference/markets` | `NONE` |
| Fetch inference order book (depth) | `GET` | `/api/v1/inference/depth` | `NONE` |
| List inference orders | `GET` | `/api/v1/inference/orders` | `NONE` |
| Fetch recent inference trades | `GET` | `/api/v1/inference/trades` | `NONE` |
| Register a trading account from a PrivateNote | `POST` | `/api/v1/accounts` | `NONE` |
| Fetch account collateral balance | `GET` | `/api/v1/account` | `USER_DATA` |
| Fetch outcome balances for one market | `GET` | `/api/v1/account/balances` | `USER_DATA` |
| Buy a full set of outcome tokens with collateral | `POST` | `/api/v1/prediction/buyFullSet` | `TRADE` |
| Sell a full set of outcome tokens back into collateral | `POST` | `/api/v1/prediction/sellFullSet` | `TRADE` |
| Claim payout after market resolution | `POST` | `/api/v1/prediction/claim` | `TRADE` |
| Create single order | `POST` | `/api/v1/prediction/order` | `TRADE` |
| Cancel single order by ID | `DELETE` | `/api/v1/prediction/order` | `TRADE` |
| Create batch orders | `POST` | `/api/v1/prediction/batchOrders` | `TRADE` |
| Cancel batch orders by IDs | `DELETE` | `/api/v1/prediction/batchOrders` | `TRADE` |
| Cancel all open orders on one symbol | `DELETE` | `/api/v1/prediction/openOrders` | `TRADE` |
| Fetch orders | `GET` | `/api/v1/prediction/orders` | `USER_DATA` |
| Subscribe to user order updates | `WS` | `/ws/v1/prediction/user` | `USER_DATA` |

## Prediction Market Data

### Prediction Markets

```http
GET /api/v1/prediction/markets
```

Fetch available prediction markets, their outcomes, lifecycle phase, timings, and oracle event metadata.

A market in DEX.DO has a finite lifecycle anchored to an oracle event. The lifecycle has nine phases — see [Market Status](#market-status). Clients MUST treat `status` as an opaque enum value and not derive it from raw timings.

In `/api/v1/prediction/markets`, `event.oracles[]` contains only the oracles for that market. Use `/api/v1/oracles` to list oracles and events available for creating markets.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | NO | Return one market only. Mutually exclusive with the filter and pagination parameters below. |
| `status` | STRING | NO | Comma-separated list of statuses to include. Example: `TRADING,AWAITING_FREEZE`. |
| `quoteAsset` | STRING | NO | Filter by quote asset. Example: `USDC`. |
| `oracleName` | STRING | NO | Filter by oracle name. A market matches if its `event.oracles[]` contains this oracle name. |
| `resolvesFrom` | STRING | NO | Return only markets settled from this inference model's order book (its `inferenceOrderBookAddress` from [`/api/v1/inference/markets`](#inference-markets)). See `resolvesFrom` in the response. |
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
      "predictionMarketAddress": "0:b286...",
      "predictionOrderBookAddress": "0:c12d...",
      "marketName": "PM-2026-ELECTION",
      "status": "TRADING",
      "quoteAsset": "USDC",
      "tokenType": 1,
      "makerCommission": "-0.0003375",
      "takerCommission": "0.0004500",
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
          "maxBatchSize": 10
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
          "maxBatchSize": 10
        }
      ]
    }
  ]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `serverTime` | LONG | Unix timestamp in seconds. All timestamp fields returned by `/api/v1/prediction/markets` are unix seconds unless explicitly stated otherwise. |
| `nextCursor` | STRING \| null | Pagination cursor for the next page. `null` when `hasMore` is `false`. |
| `hasMore` | BOOLEAN | Whether more pages follow. |
| `predictionMarketAddress` | STRING | Stable market identifier. |
| `predictionOrderBookAddress` | STRING | Deterministic order-book address. Always present on markets visible to the API (the backend stamps it on the first reconcile). Trading availability depends on market `status`. |
| `marketName` | STRING | Technical market name. Not the user-facing title; see `event.eventName`. |
| `status` | ENUM | Market phase. See [Market Status](#market-status). |
| `quoteAsset` | STRING | Quote-asset symbol for display. |
| `tokenType` | INT | Numeric quote-asset token type. |
| `makerCommission` | DECIMAL | Maker fee rate applied to trades on this market, as a signed decimal string. The maker is never charged; a negative value (e.g. `"-0.0003375"`) is a **maker rebate** — the amount is **credited** to the maker, paying makers for providing liquidity. |
| `takerCommission` | DECIMAL | Taker fee rate applied to trades on this market, as a decimal string. Always non-negative (e.g. `"0.0004500"` = 0.045%) and **charged** to the taker. |
| `createdAt` | LONG | Unix seconds. Market creation timestamp. Used for sorting by recency. |
| `timings` | OBJECT \| null | See [Timings](#timings). `null` when `status == "PENDING"`. |
| `event` | OBJECT | See [Event](#event). |
| `terminal` | OBJECT \| null | See [Terminal](#terminal). `null` for non-terminal statuses. |
| `outcomes` | ARRAY | Outcome-token descriptors. See [Outcome](#outcome). |
| `resolvesFrom` | OBJECT \| null | Present only for markets settled from an inference model's reference price; `null` otherwise. See [resolvesFrom](#resolvesfrom). |

#### Common Enums

##### Market Status

A market is in exactly one of nine phases.

| Value | Description |
| --- | --- |
| `PENDING` | Market exists, but its schedule is not available yet. `timings` is `null`. Clients cannot stake or trade. This may mean oracle approval is still incomplete, or the market is approved but timings have not been set yet. |
| `UPCOMING` | Market schedule is available, but staking has not started yet. Clients cannot stake or trade until the market reaches `STAKING` or `TRADING`, respectively. |
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
| `eventId` | STRING | `0x`-prefixed uint256 event identifier. Shared by all oracle entries for the same event. |
| `eventName` | STRING \| null | User-facing event title. `null` when unavailable. |
| `description` | STRING \| null | User-facing event description. `null` when unavailable. |
| `oracles` | ARRAY of [OracleEntry](#oracleentry) | Oracle entries for this market. Empty array means no oracle entry is available for the market yet. |

###### OracleEntry

| Field | Type | Description |
| --- | --- | --- |
| `name` | STRING \| null | Oracle name from `oracles.name`. `null` if the indexer has not yet reconciled the oracle row. |
| `address` | STRING \| null | Oracle contract address. |
| `fee` | DECIMAL \| null | Oracle fee for this confirmation, as a uint128 decimal string. Different oracles can charge different fees for the same event. |

If oracle entries for the same market disagree on `eventName` or `description`, the backend returns `MarketInconsistent` (HTTP 503).

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
| `cancelReason` | ENUM \| null | `PMP_REJECTED_BY_ORACLE` or `EVENT_CANCELLED`. Present only when `kind == "CANCELLED"`; `null` otherwise. |

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
  "maxBatchSize": 10
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

##### resolvesFrom

Present only on a prediction market whose outcome is decided by a model's reference price (a numeric range event); `null` on all other markets. The numeric outcome ranges are the market's normal `outcomes`.

```json
{
  "inferenceOrderBookAddress": "0:ob-addr...",
  "model": "qwen--qwen2.5-32b--instruct",
  "metric": "WEEKLY_MEDIAN_PRICE"
}
```

| Field | Type | Description |
| --- | --- | --- |
| `inferenceOrderBookAddress` | STRING | The inference market ([`/api/v1/inference/markets`](#inference-markets)) this market settles from. |
| `model` | STRING \| null | The model `ref` (`producer--model--version`); `null` if the model identity is not yet known on chain. |
| `metric` | ENUM | The price metric used to settle. Currently `WEEKLY_MEDIAN_PRICE`. |

### Prediction Order Book

```http
GET /api/v1/prediction/depth
```

Fetch bids and asks for one symbol in one market.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `limit` | INT | NO | Number of price levels per side. Default: `100`. Max: `1000`. |

Response:

```json
{
  "predictionMarketAddress": "0:market-address",
  "symbol": "PM-2026-ELECTION-YES",
  "lastUpdateId": "76a23086a00670000000000000000000000000000000000000000000000000000000000000000000002",
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
| `lastUpdateId` | STRING | Opaque chain-order cursor. Lex-comparable: a larger string means a newer event has touched this `(predictionMarketAddress, symbol)`. Empty string when no order event has landed yet. Clients SHOULD compare for equality to detect "no change" and string-lex order to detect "moved forward"; they SHOULD NOT parse it as an integer. |

Each bid or ask item is:

```text
[price, quantity]
```

### Prediction Trades

```http
GET /api/v1/prediction/trades
```

Security: `NONE`

Fetch the most recent public trades for one symbol in one market. A trade is a
single maker↔taker match produced by the order book — the public, account-agnostic
view of the fills that surface privately as `x: "TRADE"` frames on the
[`orderUpdate`](#order-updates) WebSocket stream. No authentication is required and
no owner information is returned; the endpoint exposes only price, size, direction,
and time.

This endpoint is unaffected by market lifecycle status: trades are returned for any
market that has been reconciled at least once, including terminal phases
(`RESOLVED`, `CANCELLED`, `EXPIRED`), so the trade tape remains readable after the
book closes. It is eventually consistent — a just-matched trade may briefly lag the
fill that produced it until the indexer projects the match.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `limit` | INT | NO | Number of trades to return, newest first. Default: `20`. Max: `1000`. |

Response:

```json
[
  {
    "tradeId": "76a23086a00670000000000000000000000000000000000000000000000000000000000000000000005",
    "price": "0.615",
    "qty": "1.00",
    "quoteQty": "0.615000",
    "time": 1710000008980,
    "isBuyerMaker": true
  },
  {
    "tradeId": "76a23086a00670000000000000000000000000000000000000000000000000000000000000000000004",
    "price": "0.615",
    "qty": "0.50",
    "quoteQty": "0.307500",
    "time": 1710000004980,
    "isBuyerMaker": true
  }
]
```

The response is a bare JSON array, newest trade first, ordered by the same
server-internal chain-order key used by [`GET /api/v1/prediction/orders`](#orders) (descending).
An empty array means no trade has matched on this symbol yet.

Trade fields:

| Field | Type | Description |
| --- | --- | --- |
| `tradeId` | STRING | Opaque, lex-comparable id for one maker↔taker match. The identical value is carried by the WebSocket [`orderUpdate`](#order-updates) fill frame as field `t`, on both sides of the match — so a client can correlate a public trade with its own private fills and deduplicate across the two feeds. Treat it as an opaque token; do not parse it. |
| `price` | DECIMAL | Match (clearing) price, scaled by the outcome price precision. |
| `qty` | DECIMAL | Matched outcome-token quantity, scaled by the outcome quantity precision. |
| `quoteQty` | DECIMAL | Quote-asset notional of the match (`price × qty`), scaled by the quote asset's on-chain `decimals`. |
| `time` | LONG | On-chain match time in Unix milliseconds, truncated from the indexed microsecond timestamp — same convention as `time` / `updateTime` on [`GET /api/v1/prediction/orders`](#orders). |
| `isBuyerMaker` | BOOLEAN | `true` when the resting (maker) side was the buy order and the taker was selling; `false` when the taker was buying. Determines display direction: `true` = taker sold (downtick), `false` = taker bought (uptick). Matches Binance `isBuyerMaker` semantics. |

Each public trade is the account-agnostic view of a fill that also appears on the
private [`orderUpdate`](#order-updates) stream (`x: "TRADE"`). The fields line up
one-to-one, so a client subscribed to both feeds can match them by `tradeId` / `t`:

| `/api/v1/prediction/trades` field | `orderUpdate` fill field | Note |
| --- | --- | --- |
| `tradeId` | `t` | Same opaque match id on both feeds, and on both sides of the match. |
| `price` | `L` | Last fill price. |
| `qty` | `l` | Last fill quantity. |
| `time` | `T` | On-chain match / transaction time. |
| `isBuyerMaker` | `S` + `m` | Public trade carries the side-agnostic direction; the `orderUpdate` frame carries the recipient's own side (`S`) and maker flag (`m`). |

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `predictionMarketAddress` or `symbol` missing or blank | `-1102` | 400 |
| `limit` present but not an integer | `-1130` | 400 |
| `limit` outside `[1, 1000]` | `-1102` | 400 |
| `(predictionMarketAddress, symbol)` pair not found, or its market has not been reconciled yet | `-1121` | 404 |
| Trade data is temporarily inconsistent | `-1500` | 503 |

## Oracles

```http
GET /api/v1/oracles
```

Fetch oracles, their event lists, and events that can be used to create a market.

The response is grouped by oracle, then by event list. Each event list contains its description and the events it currently offers for market creation.

This endpoint does not return markets already created from these events. Use `/api/v1/prediction/markets` for market lifecycle, status, timings, order-book address, and market outcomes.

By default, each event list includes only events that are still available for new markets: not deleted and not past `deadline`.

Results are ordered by oracle name, then event-list index, then event deadline, then event id.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `oracleAddress` | STRING | NO | Filter by oracle address. Example: `0:oracle-address`. |
| `eventId` | STRING | NO | Return only oracles with an event list that contains this event id. The `events[]` arrays contain only the matching event. |
| `deadlineBefore` | LONG | NO | Include only events with `deadline < deadlineBefore` (unix seconds). Event lists and oracles with no remaining events are omitted. |
| `cursor` | STRING | NO | Opaque pagination cursor returned by a previous call. |
| `limit` | INT | NO | Number of oracles to return. Default: `50`. Max: `200`. |

Response:

```json
{
  "serverTime": 1710000000,
  "nextCursor": "eyJ...",
  "hasMore": false,
  "oracles": [
    {
      "name": "ElectionOracle",
      "address": "0:oracle-a",
      "eventLists": [
        {
          "index": 0,
          "address": "0:event-list-a",
          "description": "Election markets verified by ElectionOracle.",
          "events": [
            {
              "eventId": "0xabc...",
              "eventName": "2026 US Presidential Election",
              "description": "Will candidate X win?",
              "oracleFee": {
                "asset": "SHELL",
                "amount": "100"
              },
              "deadline": 1710011600,
              "trustAddress": null,
              "outcomes": [
                {
                  "outcomeId": 0,
                  "outcomeName": "NO"
                },
                {
                  "outcomeId": 1,
                  "outcomeName": "YES"
                }
              ]
            }
          ]
        }
      ]
    }
  ]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `serverTime` | LONG | Unix timestamp in seconds. All timestamp fields returned by `/api/v1/oracles` are unix seconds unless explicitly stated otherwise. |
| `nextCursor` | STRING \| null | Pagination cursor for the next page. `null` when `hasMore` is `false`. |
| `hasMore` | BOOLEAN | Whether more pages follow. |
| `oracles` | ARRAY of [OracleEntry](#oracleentry-1) | Oracles with available events. |
| `oracles[].name` | STRING | Oracle name. |
| `oracles[].address` | STRING | Oracle address. |
| `oracles[].eventLists` | ARRAY of [OracleEventList](#oracleeventlist) | Event lists owned by this oracle. |
| `oracles[].eventLists[].index` | LONG | Oracle-local event-list index. |
| `oracles[].eventLists[].address` | STRING | Event-list address. |
| `oracles[].eventLists[].description` | STRING | Human-readable event-list description. |
| `oracles[].eventLists[].events` | ARRAY of [OracleEvent](#oracleevent) | Events offered by this event list. |
| `oracles[].eventLists[].events[].eventId` | STRING | Stable event identifier. |
| `oracles[].eventLists[].events[].eventName` | STRING | Human-readable event name. |
| `oracles[].eventLists[].events[].description` | STRING \| null | Human-readable event description. `null` when unavailable. |
| `oracles[].eventLists[].events[].oracleFee.asset` | STRING | Fee asset. Current oracle contracts use `SHELL`. |
| `oracles[].eventLists[].events[].oracleFee.amount` | DECIMAL | Fee amount, encoded as a decimal string. |
| `oracles[].eventLists[].events[].deadline` | LONG | Unix seconds. Deadline for using this oracle event. |
| `oracles[].eventLists[].events[].trustAddress` | STRING \| null | Optional trusted address attached to the event. `null` when absent or unavailable. |
| `oracles[].eventLists[].events[].outcomes` | ARRAY of [OracleOutcome](#oracleoutcome) | Event outcome labels, sorted by `outcomeId`. |

### Oracle Data Objects

#### OracleEntry

```json
{
  "name": "ElectionOracle",
  "address": "0:oracle-a",
  "eventLists": [
    {
      "index": 0,
      "address": "0:event-list-a",
      "description": "Election markets verified by ElectionOracle.",
      "events": []
    }
  ]
}
```

| Field | Type | Description |
| --- | --- | --- |
| `name` | STRING | Oracle name. |
| `address` | STRING | Oracle address. |
| `eventLists` | ARRAY of [OracleEventList](#oracleeventlist) | Event lists owned by this oracle. |

#### OracleEventList

| Field | Type | Description |
| --- | --- | --- |
| `index` | LONG | Oracle-local event-list index. This is the value used with the oracle name when creating a market from one of its events. |
| `address` | STRING | Event-list address. |
| `description` | STRING | Human-readable event-list description. |
| `events` | ARRAY of [OracleEvent](#oracleevent) | Events offered by this event list. |

#### OracleEvent

| Field | Type | Description |
| --- | --- | --- |
| `eventId` | STRING | Stable event identifier. |
| `eventName` | STRING | Human-readable event name. |
| `description` | STRING \| null | Human-readable event description. `null` when unavailable. |
| `oracleFee` | OBJECT | Fee required by this oracle for the event. |
| `oracleFee.asset` | STRING | Fee asset. Current oracle contracts use `SHELL`. |
| `oracleFee.amount` | DECIMAL | Fee amount, encoded as a decimal string. |
| `deadline` | LONG | Unix seconds. Deadline for using this oracle event. |
| `trustAddress` | STRING \| null | Optional trusted address attached to the event. `null` when absent or unavailable. |
| `outcomes` | ARRAY of [OracleOutcome](#oracleoutcome) | Event outcome labels, sorted by `outcomeId`. |

#### OracleOutcome

| Field | Type | Description |
| --- | --- | --- |
| `outcomeId` | INT | Outcome id. |
| `outcomeName` | STRING | Human-readable outcome label. |

## Inference Market Data

Market data for the **private-inference market**: tradable AI models and the prediction markets settled from their prices. The unit of trade is an **inference tick** — one unit of model generation — priced **per tick in `SHELL`**. Each model has exactly one order book; there is no `symbol` dimension (unlike prediction-market depth, which is per outcome).

All four endpoints are public (`NONE`), read-only, and eventually consistent — a just-placed order or a fresh reference price may briefly lag the chain.

### Inference Markets

```http
GET /api/v1/inference/markets
```

List the tradable models — one entry per model order book.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `inferenceOrderBookAddress` | STRING | NO | Return one market only. Mutually exclusive with the filter and pagination parameters below. |
| `producer` | STRING | NO | Filter by model producer (e.g. `qwen`). |
| `status` | STRING | NO | Comma-separated statuses to include. Currently only `TRADING`. |
| `sort` | STRING | NO | Sort field. `createdAt` (default, DESC). |
| `cursor` | STRING | NO | Opaque pagination cursor from a previous call. |
| `limit` | INT | NO | Page size. Default: `50`. Max: `200`. |

Response:

```json
{
  "serverTime": 1710000000,
  "nextCursor": null,
  "hasMore": false,
  "markets": [
    {
      "inferenceOrderBookAddress": "0:ob-addr...",
      "model": {
        "producer": "qwen",
        "name": "qwen2.5-32b",
        "version": "instruct",
        "ref": "qwen--qwen2.5-32b--instruct"
      },
      "contractVersion": "4.0.30",
      "status": "TRADING",
      "quoteAsset": "SHELL",
      "makerCommission": "-0.02",
      "takerCommission": "0.025",
      "pricePrecision": 9,
      "quantityPrecision": 0,
      "tickSize": "1",
      "stepSize": "1",
      "minNotional": "1",
      "referencePrice": "1010",
      "createdAt": 1709980000
    }
  ]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `serverTime` | LONG | Unix seconds, captured once for the request. |
| `nextCursor` | STRING \| null | Cursor for the next page. `null` when `hasMore` is `false`. |
| `hasMore` | BOOLEAN | Whether more pages follow. |
| `inferenceOrderBookAddress` | STRING | Stable market id — the model's order-book address. Pass it as `?inferenceOrderBookAddress=` to fetch this one market, and as the key for [`/api/v1/inference/depth`](#inference-depth). |
| `model` | OBJECT | Model identity. `ref` is the canonical `producer--model--version`. `producer` / `name` / `version` MAY be `null` if the model identity is not yet known on chain; `ref` then carries the model hash. |
| `contractVersion` | STRING \| null | Version of the deployed order-book contract backing this market (e.g. `"4.0.30"`). Distinct from `model.version`, which is the AI model's own version. `null` when the contract version is not yet known on chain. |
| `status` | ENUM | `TRADING`. Reserved for future inactive states; clients MUST treat it as opaque. |
| `quoteAsset` | STRING | Always `SHELL`. |
| `makerCommission` | DECIMAL | Maker (**seller**) fee rate, a signed decimal string. The seller is never charged; a negative value (`"-0.02"` = −2%) is a **rebate credited to the seller** for delivering ticks cleanly. This is the rebate **cap**: the actual rebate ramps from `0` with delivered ticks and applies only on a clean, non-disputed close, so a given deal may credit less. |
| `takerCommission` | DECIMAL | Taker (**buyer**) fee rate, charged to the buyer per delivered tick. Always non-negative (`"0.025"` = 2.5%). What the buyer spends on top of the tick price. The amount not returned to the seller as a rebate is burned. |
| `pricePrecision` | INT | Decimal places for price-per-tick. |
| `quantityPrecision` | INT | Decimal places for tick quantity. Ticks are whole units, so `0`. |
| `tickSize` | DECIMAL | Minimum price-per-tick increment. |
| `stepSize` | DECIMAL | Minimum tick-quantity increment (`"1"`). |
| `minNotional` | DECIMAL | Minimum order notional in `SHELL`. |
| `referencePrice` | DECIMAL \| null | Weekly-median price per tick used to settle prediction markets. **`null`** when the book has no recent liquidity. |
| `createdAt` | LONG | Unix seconds. When the book was first seen. |

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `inferenceOrderBookAddress` together with filter/pagination params | `-1102` | 400 |
| Invalid `status` / `sort` value | `-1130` | 400 |
| Corrupted `cursor` | `-1130` | 400 |
| `inferenceOrderBookAddress` not found | `-1121` | 404 |

### Inference Depth

```http
GET /api/v1/inference/depth
```

Fetch resting bids and asks for one model's order book. Mirrors [`/api/v1/prediction/depth`](#prediction-order-book), keyed by the order-book address (no `symbol` — one book per model).

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `inferenceOrderBookAddress` | STRING | YES | The model's order-book address from [`/api/v1/inference/markets`](#inference-markets). |
| `limit` | INT | NO | Number of price levels per side. Default: `100`. Max: `1000`. |

Response:

```json
{
  "inferenceOrderBookAddress": "0:ob-addr...",
  "contractVersion": "4.0.30",
  "lastUpdateId": "76a23086a006700000000000000000000000000000000000000000000000000000000000000000007",
  "bids": [
    ["1000", "120"],
    ["990", "300"]
  ],
  "asks": [
    ["1050", "80"],
    ["1060", "210"]
  ]
}
```

Each bid or ask item is `[pricePerTick, ticks]` — price in `SHELL` and the total ticks resting at that price.

| Field | Type | Description |
| --- | --- | --- |
| `contractVersion` | STRING \| null | Version of the deployed order-book contract for this book (e.g. `"4.0.30"`). Same value as `contractVersion` in [`/api/v1/inference/markets`](#inference-markets) for the same `inferenceOrderBookAddress`. `null` when the contract version is not yet known on chain. |
| `lastUpdateId` | STRING | Opaque chain-order cursor for this book. Lex-comparable: a larger string means a newer event has touched the book. Empty string when no order has landed yet. Do not parse it as an integer. |

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `inferenceOrderBookAddress` missing or blank | `-1102` | 400 |
| `limit` present but not an integer | `-1130` | 400 |
| `inferenceOrderBookAddress` not found / not yet available | `-1121` | 404 |
| Book data temporarily inconsistent | `-1500` | 503 |

> **Prediction markets settled from a model price** are regular prediction markets, listed by [`/api/v1/prediction/markets`](#prediction-markets) — filter with `?resolvesFrom=<inferenceOrderBookAddress>` and read the per-market `resolvesFrom` block. See [Markets](#prediction-markets).

### Inference Orders

```http
GET /api/v1/inference/orders
```

Orders resting in, or historic to, one model's book — the order-level counterpart to [`/api/v1/inference/depth`](#inference-depth), which shows only aggregated price levels.

Unlike [Inference Markets](#inference-markets) and [Inference Depth](#inference-depth), which collapse a present-but-blank query parameter to "absent", this endpoint rejects one with `-1102`. A blank `tokenContract=` silently becoming "no filter" would return the whole book and read as a confident answer to a question nobody asked; the caller must omit the parameter, not send it empty.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `inferenceOrderBookAddress` | STRING | YES | The model's order-book address from [`/api/v1/inference/markets`](#inference-markets). |
| `tokenContract` | STRING | NO | Exact deal `TokenContract` address. Mutually exclusive with `note`. Refused with `-1500` (HTTP 503, retry) while the book holds a live SELL whose `TokenContract` the indexer does not know. |
| `note` | STRING | NO | Exact owning PrivateNote address. Mutually exclusive with `tokenContract`. |
| `side` | STRING | NO | `BUY` or `SELL`. |
| `status` | STRING | NO | Comma-separated: `LIVE`, `FILLED`, `CANCELLED`, `EXPIRED`. Tokens are trimmed and de-duplicated. Default: all statuses. `LIVE` means currently resting; `EXPIRED` means the book dropped it once its deadline passed. |
| `limit` | INT | NO | Page size. Default: `100`. Range: `[1, 500]`; out-of-range values are rejected, not clamped. |
| `cursor` | STRING | NO | Keyset cursor: the decimal `orderId` of the last row on the previous page, taken verbatim from a previous call's `nextCursor`. |

Response:

```json
{
  "serverTime": 1710000000,
  "lastUpdateId": "76a23086a006700000000000000000000000000000000000000000000000000000000000000000007",
  "nextCursor": "918273645",
  "hasMore": false,
  "orders": [
    {
      "orderId": "918273645",
      "inferenceOrderBookAddress": "0:ob-addr...",
      "tokenContract": "0:tc-addr...",
      "note": "0:note-addr...",
      "side": "SELL",
      "price": "1000",
      "ticks": "40",
      "ticksInitial": "120",
      "deadline": "1710003600",
      "status": "LIVE",
      "createdAt": 1709999000,
      "updatedAt": 1710000500
    }
  ]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `serverTime` | LONG | Unix seconds, captured once for the request. |
| `lastUpdateId` | STRING | Freshness token: `max(last_chain_order)` over the book, covering chain-projected events only. The reconciler mutates rows without advancing it, so two responses sharing the same `lastUpdateId` are not guaranteed identical — do not treat it as a change fingerprint. |
| `nextCursor` | STRING \| null | Decimal `orderId` of the last row on this page. Pass back verbatim as `cursor` to fetch the next page. `null` when `hasMore` is `false`. |
| `hasMore` | BOOLEAN | Whether more pages follow. |
| `orders` | ARRAY | Orders matching the filter, newest-placed first. Empty when there are no matches. |
| `orderId` | STRING | Chain-side order id, as a decimal string. |
| `inferenceOrderBookAddress` | STRING | Echoes the request's `inferenceOrderBookAddress`. |
| `tokenContract` | STRING \| null | `null` on BUY orders and on rows whose TokenContract is not known to the indexer. A query filtering on `tokenContract` is refused (see `-1500` below) while any live SELL of the book is in that state, so `null` here never hides a match. |
| `note` | STRING \| null | Owning PrivateNote address. |
| `side` | ENUM | `BUY` or `SELL`. |
| `price` | DECIMAL | Price per tick, in `SHELL`. |
| `ticks` | DECIMAL | The **resting remainder** — ticks still available at this order. Compare directly against a level in [`/api/v1/inference/depth`](#inference-depth), which uses the same name (`ticks`) for the same quantity. |
| `ticksInitial` | DECIMAL | The size the order was placed with. On chain, `InferenceOrderPlaced.ticks` is this initial size — the same field name carries a different number in the chain event than it does in this response's `ticks`. |
| `deadline` | STRING \| null | Unix seconds as a **decimal string**, reproducing the chain `uint64` verbatim (it can exceed both `i64` and JSON's exact-integer range). `null` means no deadline is known to the indexer — see the note below; it does not mean "no deadline". |
| `status` | ENUM | `LIVE`, `FILLED`, `CANCELLED`, or `EXPIRED`. `EXPIRED` means the book dropped the order once its `deadline` passed, as opposed to someone cancelling it. A past `deadline` on its own never implies `EXPIRED`: the status changes only when the chain reports the removal, so an order can read `LIVE` with a `deadline` already behind it. See the notes below. |
| `createdAt` | LONG \| null | Unix seconds. `null` when the chain timestamp was not recovered — the row is served regardless. |
| `updatedAt` | LONG \| null | Unix seconds, same convention as `createdAt`. |

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `inferenceOrderBookAddress` missing or blank | `-1102` | 400 |
| `tokenContract`, `note`, `side`, `status`, or `cursor` present but blank | `-1102` | 400 |
| `tokenContract` and `note` both supplied | `-1130` | 400 |
| `side` present and not `BUY` / `SELL` | `-1130` | 400 |
| `status` contains an unknown token | `-1130` | 400 |
| `limit` present but not an integer | `-1130` | 400 |
| `limit` outside `[1, 500]` | `-1102` | 400 |
| `cursor` present but not all decimal digits, or oversized | `-1130` | 400 |
| `inferenceOrderBookAddress` not found | `-1121` | 404 |
| A `tokenContract` query scopes live SELLs while the book holds one whose TokenContract the indexer does not know | `-1500` | 503 |

Notes:

- `LIVE` means currently resting: placed, not cancelled, not fully filled. `InferenceOrderCancelled` moves an order to `CANCELLED`; a fill moves it to `FILLED` once no ticks remain, and a SELL leaves `LIVE` on its first fill because one SELL offer is one deal.
- `status=LIVE` can transiently include an order that was placed but never rested — a `POST_ONLY` placement rejected for crossing, a failed `FOK`, or a partially matched BUY `MARKET`/`IOC` whose remainder was refunded on chain without an order-bearing event. The reconciler's probe clears these on its next sweep. The error runs in the safe direction: such a row reports its TokenContract as *in use*, never as free.
- `lastUpdateId` orders chain progress only. The reconciler mutates rows without advancing it, so two responses sharing a `lastUpdateId` are not guaranteed identical.
- A `tokenContract` query that scopes live SELLs returns `-1500` (HTTP 503) while the book holds a live SELL whose TokenContract the indexer does not know. Deliberate: an empty page would otherwise be indistinguishable from "not in use", and 503 tells the client to retry where 404 would tell it to stop.
- `deadline: null` means the indexer knows of no deadline, and it conflates two chain states the client cannot separate. `placeBuyOrder` accepts `deadline == 0` ("no deadline"), and a resting SELL always has `deadline = 0`; but a subscription always holds one on chain and never publishes it, so its row is born `null` and gains a value only when the sweep probes it. Do not let `null` read as "no deadline". Likewise `createdAt` / `updatedAt` may be `null`.
- `ticks` is the **resting remainder**; `ticksInitial` is the placed size. `InferenceOrderPlaced.ticks` on chain is the initial size, so the same field name carries different numbers in the event and in this response. The `ticks` name follows [`/api/v1/inference/depth`](#inference-depth), which already publishes `[pricePerTick, ticks]` for the ticks resting at a level — a client comparing an order against a depth level finds one name for one quantity.
- `deadline` is a **decimal string** while `createdAt`, `updatedAt`, and `serverTime` are JSON numbers, though all four are unix seconds. `deadline` is a chain `uint64` reproduced verbatim, and can exceed both `i64` and JSON's exact-integer range; the other three are database timestamps. This follows the repo-wide rule that chain-native unsigned integers (`price`, `ticks`, `lastUpdateId`) serialize as strings.
- `note` and `tokenContract` cannot be combined: `-1130`, HTTP 400. Both are present, so nothing is missing — the combination is what cannot be served, because no index pins both and the pair would scan one filter's whole history. This is a different relation from [`/api/v1/prediction/orders`](#orders), where `predictionMarketAddress` and `symbol` must be given together or not at all, and a half-specified pair is `-1102`.

### Inference Trades

```http
GET /api/v1/inference/trades
```

Security: `NONE`

Fetch the most recent public trades on one model's order book. A trade is a
single maker↔taker match produced by the `InferenceOrderBook.InferenceFilled`
event — the public, account-agnostic view of the book's fills. No
authentication is required and no owner information is returned; the
endpoint exposes only price, size, direction, and time. Mirrors
[`/api/v1/prediction/trades`](#prediction-trades), keyed by the order-book
address (no `symbol` — one book per model, as with
[Inference Depth](#inference-depth) and [Inference Orders](#inference-orders)).

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `inferenceOrderBookAddress` | STRING | YES | The model's order-book address from [`/api/v1/inference/markets`](#inference-markets). |
| `limit` | INT | NO | Number of trades to return, newest first. Default: `20`. Max: `1000`. **Out-of-range values clamp** into `[1, 1000]` rather than being rejected — unlike [`/api/v1/prediction/trades`](#prediction-trades), whose contract refuses an out-of-range `limit` with `-1102`. |

Response:

```json
[
  {
    "tradeId": "76a23086a0067000000000000000000000000000000000000000000000000000000000000000000000c",
    "price": "2.000000000",
    "qty": "3",
    "quoteQty": "6.000000000",
    "time": 1710000008980,
    "isBuyerMaker": true
  }
]
```

The response is a bare JSON array, newest trade first, ordered by the same
server-internal chain-order key used by
[`GET /api/v1/inference/orders`](#inference-orders) (descending). An empty
array (`[]`) means no trade has matched on this book yet — never `null`,
never an object wrapping an empty field.

Trade fields:

| Field | Type | Description |
| --- | --- | --- |
| `tradeId` | STRING | Opaque, lex-comparable id for one maker↔taker match — the chain order of the `InferenceFilled` event that recorded it. Shared by both sides of the match. Treat it as an opaque token; do not parse it. |
| `price` | DECIMAL | Clearing price per tick, scaled by the book's price precision. |
| `qty` | DECIMAL | Matched tick count, scaled by the book's quantity precision. |
| `quoteQty` | DECIMAL | Quote-asset notional of the match (`price × qty`), scaled by the quote asset's on-chain `decimals`. This is the notional, **not** what the buyer paid: the book charges `price + tickFee(price)` per tick, and that fee is reported separately as `InferenceExecuted.cost`, not folded into `quoteQty` here. |
| `time` | LONG | On-chain match time in Unix milliseconds. |
| `isBuyerMaker` | BOOLEAN | `true` when the resting (maker) side was the buy order and the taker was selling (downtick); `false` when the taker was buying (uptick). Matches Binance `isBuyerMaker` semantics. |

Three deliberate departures from what a client might expect, each easy to misread as a bug:

1. **No `contractVersion`.** Unlike [`/api/v1/inference/markets`](#inference-markets) and [`/api/v1/inference/depth`](#inference-depth), this response is a flat array with no envelope to carry book metadata. A client that needs the book's contract generation reads it from `/api/v1/inference/markets?inferenceOrderBookAddress=…`.
2. **No pagination beyond `limit`.** There is no `cursor` and no `hasMore`: the endpoint serves at most the `limit` (ceiling `1000`) newest matches and nothing older. A client that needs deeper trade history has no cursor to page through — none exists.
3. **`limit` clamps, it does not reject** — see the query-parameter table above. This is a deliberate divergence from `/api/v1/prediction/trades`.

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `inferenceOrderBookAddress` missing or blank | `-1102` | 400 |
| `limit` present but not an integer | `-1130` | 400 |
| `inferenceOrderBookAddress` not found, or its book has not been reconciled yet | `-1121` | 404 |
| Trade data is temporarily inconsistent | `-1500` | 503 |

## Account Endpoints

### Register Account

```http
POST /api/v1/accounts
```

Register a trading account from a PrivateNote you already control, and receive an API credential you can sign requests with right away.

This is the self-service equivalent of seeding one account: you supply the note's address and keys, the backend stores them (the secret key sealed at rest, exactly as seeded accounts are) and mints one API key with `USER_DATA` and `TRADE` permission. The endpoint is public — you have no API key yet, and possession of the note's keys is the authorization.

**The PrivateNote must already be deployed and funded on-chain.** Registration only writes backend rows; it deploys nothing. Before creating the account the backend confirms the note exists on-chain — returning `-2013` if it does not — so a registered account can trade immediately. A note that later runs out of gas still fails trading until it is refunded.

Request body:

| Field | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `pnAddress` | STRING | YES | Address of the deployed PrivateNote (`0:…`). |
| `pnPubkeyHex` | STRING | YES | Note owner public key, hex (up to 256 bits). Must be the key `pnSeckeyHex` derives. |
| `pnSeckeyHex` | STRING | YES | Note owner secret key, 32-byte hex. Held in custody, sealed under the backend key, and used to sign your trades. |
| `pnDihHex` | STRING | YES | Deposit-identifier hash of the note, hex (up to 256 bits). |

```json
{
  "pnAddress": "0:a554b6d3…",
  "pnPubkeyHex": "6a00e7c5…",
  "pnSeckeyHex": "de0a6632…",
  "pnDihHex": "8416176d…"
}
```

Response:

```json
{
  "accountId": "7c3e2f10-...",
  "pnAddress": "0:a554b6d3…",
  "apiKey": "dk_live_9fa3...",
  "apiSecret": "f1c2...",
  "permissions": ["USER_DATA", "TRADE"]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `accountId` | STRING | Identifier of the created account. |
| `pnAddress` | STRING | The registered note address, echoed back. |
| `apiKey` | STRING | Send this as the `X-DODEX-APIKEY` header on signed requests. |
| `apiSecret` | STRING | The HMAC signing secret, hex. **Returned only here** — it is stored sealed and cannot be retrieved again, so capture it now. |
| `permissions` | ARRAY | Granted permissions: `["USER_DATA", "TRADE"]`. |

Each note can be registered once. Registering a note that already has an account returns `-2015` (HTTP 409); the existing credential is left untouched and no second one is minted. Losing an `apiSecret` cannot be undone by re-registering the same note. The submitted secret key must be the note's own key — the backend checks it against the note's on-chain owner and returns `-2016` otherwise.

Errors: `-1102` (a mandatory field is missing), `-1130` (a field is malformed — bad hex, over 256 bits, an unknown body key, or `pnPubkeyHex` is not the key `pnSeckeyHex` derives), `-1500` (the backend could not read the note's on-chain state; transient — retry), `-2013` (the note is not deployed on-chain), `-2015` (the note is already registered), `-2016` (the submitted key does not control the note).

### Account Balance

```http
GET /api/v1/account
```

Security: `USER_DATA`

Fetch account collateral balances. Outcome-token holdings are scoped per market and live on a separate endpoint — see [Market Outcome Balances](#market-outcome-balances).

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
  "accountId": "0c2d1da4-3f60-4a7e-9a6d-1a3b1ec5e3d2",
  "updateTime": 1710000000000,
  "balances": [
    {
      "asset": "NACKL",
      "free": "10.000000000",
      "locked": "1.500000000"
    },
    {
      "asset": "USDC",
      "free": "25000.000000",
      "locked": "3750.000000"
    }
  ]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `accountId` | STRING | Stable account UUID. |
| `updateTime` | LONG | Server timestamp (Unix ms) captured when the request was served. |
| `balances` | ARRAY | One entry per quote-asset token type held by the trading PrivateNote, sorted by `asset` ascending. A token type with `free = 0 AND locked = 0` is still included when the PN has a row for it; clients SHOULD treat the absence of a token as "never funded". |
| `balances[].asset` | STRING | User-facing asset code (`NACKL`, `USDC`, …). |
| `balances[].free` | DECIMAL | Spendable balance, scaled by the token's on-chain `decimals`. |
| `balances[].locked` | DECIMAL | Collateral locked in open buy orders, same scaling. |

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| Authenticated account has no PrivateNote contract deployed at its resolved address | `-2013` | 404 |
| Backend could not read the trading PrivateNote state (gateway timeout, malformed reply, unknown `tokenType`, decimals out of range) | `-1500` | 503 |

### Market Outcome Balances

```http
GET /api/v1/account/balances
```

Security: `USER_DATA`

Fetch the authenticated account's outcome-token holdings for one market. The response lists every outcome of the market — outcomes the caller has never traded surface as `free = "0", lockedInOrders = "0"` so clients can render the full picker without a second lookup.

This endpoint is unaffected by market lifecycle status: balances are returned for any market that has been reconciled at least once, including terminal phases (`RESOLVED`, `CANCELLED`, `EXPIRED`) — holders still own their outcome tokens until they claim or settle.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |

Signed parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "predictionMarketAddress": "0:market-address",
  "updateTime": 1710000000000,
  "balances": [
    {
      "outcomeId": 0,
      "symbol": "PM-2026-ELECTION-NO",
      "free": "10.000000000",
      "lockedInOrders": "0.000000000"
    },
    {
      "outcomeId": 1,
      "symbol": "PM-2026-ELECTION-YES",
      "free": "5.500000000",
      "lockedInOrders": "1000.000000000"
    }
  ]
}
```

Response fields:

| Field | Type | Description |
| --- | --- | --- |
| `predictionMarketAddress` | STRING | Echoed from the request. |
| `updateTime` | LONG | Server timestamp (Unix ms) captured when the request was served. |
| `balances` | ARRAY | One entry per outcome of the market, sorted by `outcomeId` ascending. Length equals the market's `outcomes[]` length in `/api/v1/prediction/markets`. |
| `balances[].outcomeId` | INT | Stable outcome id; matches `outcomes[].outcomeId` from `/api/v1/prediction/markets`. |
| `balances[].symbol` | STRING | Outcome-token symbol; matches `outcomes[].symbol` from `/api/v1/prediction/markets`. |
| `balances[].free` | DECIMAL | Outcome tokens currently held by the trading PrivateNote across clean, debt, and coupon stake pools, scaled by the quote asset's on-chain `decimals` (same scaling as `/api/v1/account`). |
| `balances[].lockedInOrders` | DECIMAL | Outcome tokens locked in resting SELL orders on this outcome, scaled by the quote asset's on-chain `decimals`. |

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `predictionMarketAddress` missing or blank | `-1102` | 400 |
| `predictionMarketAddress` not found, or its market has not been reconciled yet | `-1121` | 404 |
| Authenticated account has no PrivateNote contract deployed at its resolved address | `-2013` | 404 |
| Backend could not read the trading PrivateNote state (gateway timeout, malformed reply, unknown token type, decimals out of range) | `-1500` | 503 |

## Prediction Position Endpoints

### Buy Full Set

```http
POST /api/v1/prediction/buyFullSet
```

Security: `TRADE`

Buys a full set of outcome tokens for one market — one outcome token of every outcome the market has — spending `collateral` from the caller's free quote-asset balance. The exact per-outcome amounts depend on the market's current pricing and are computed when the chain processes the request; any amount that does not divide evenly is refunded back to free balance. The resulting outcome-token holdings are readable from [`GET /api/v1/account/balances`](#market-outcome-balances) once the chain confirms.

Holding a full set is economically equivalent to holding the collateral: sell it back before resolution and the collateral comes back; hold it through resolution and one outcome pays out 1:1 while the others go to zero. Buying a full set is also the only way to obtain outcome tokens that can later be placed as `SELL` orders on the order book — a `SELL` on a market where the caller has never bought a set is rejected with `-2010`.

Available while the market is in `AWAITING_FREEZE` or `TRADING`. On a market sitting in `AWAITING_FREEZE`, the first successful call also activates the order book for everyone else; from the caller's side the request and response look identical to any later call.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `collateral` | DECIMAL | YES | Amount of the market's `quoteAsset` to allocate. Scaled by the quote-asset on-chain `decimals`. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "predictionMarketAddress": "0:market-address",
  "transactTime": 1710000000000
}
```

Response fields:

| Name | Type | Description |
| --- | --- | --- |
| `predictionMarketAddress` | STRING | Echoed from the request. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the operation was accepted. |

The response confirms acceptance only. The resulting collateral debit and outcome-token credits become visible through [`GET /api/v1/account`](#account-balance) and [`GET /api/v1/account/balances`](#market-outcome-balances) once the chain confirms.

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `predictionMarketAddress` or `collateral` missing | `-1102` | 400 |
| `collateral` not positive, exceeds quote-asset precision, or other body shape violation | `-1130` | 400 |
| `predictionMarketAddress` not found, or its market has not been reconciled yet | `-1121` | 404 |
| Market status is not `AWAITING_FREEZE` or `TRADING`; free quote-asset balance is below `collateral`; the chain rejected the request | `-2010` | 400 |
| Authenticated account has no PrivateNote contract deployed at its resolved address | `-2013` | 404 |
| Trading note is busy with another operation; retry shortly | `-2014` | 429 |
| Backend could not submit the transaction (gateway timeout, malformed reply) | `-1500` | 503 |

### Sell Full Set

```http
POST /api/v1/prediction/sellFullSet
```

Security: `TRADE`

Sells outcome tokens back to the market in exchange for `quoteAsset` credited to free collateral. The caller submits how many of each outcome they want to sell back; the market accepts the largest matching full set it can form from those amounts and refunds the leftover of every outcome.

Available while the market is in `TRADING` or `RESOLVING`.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `amounts` | ARRAY of DECIMAL | YES | Per-outcome amounts to sell back. Length MUST equal the market's `outcomes[]` length in `/api/v1/prediction/markets`. Element `i` corresponds to `outcomes[i].outcomeId` and is scaled by `outcomes[i].quantityPrecision`. Elements MAY be zero; at least one element MUST be non-zero. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "predictionMarketAddress": "0:market-address",
  "transactTime": 1710000000000
}
```

Response fields:

| Name | Type | Description |
| --- | --- | --- |
| `predictionMarketAddress` | STRING | Echoed from the request. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the operation was accepted. |

The response confirms acceptance only. The credited collateral and the burned outcome-token amounts become visible through [`GET /api/v1/account`](#account-balance) and [`GET /api/v1/account/balances`](#market-outcome-balances) once the chain confirms.

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `predictionMarketAddress` or `amounts` missing | `-1102` | 400 |
| `amounts` length does not equal the market's outcome count, any element negative or beyond precision, all elements zero | `-1130` | 400 |
| `predictionMarketAddress` not found, or its market has not been reconciled yet | `-1121` | 404 |
| Market status is not `TRADING` or `RESOLVING`; caller does not hold enough of some outcome to cover the requested amount; the chain rejected the request | `-2010` | 400 |
| Authenticated account has no PrivateNote contract deployed at its resolved address | `-2013` | 404 |
| Trading note is busy with another operation; retry shortly | `-2014` | 429 |
| Backend could not submit the transaction (gateway timeout, malformed reply) | `-1500` | 503 |

### Claim

```http
POST /api/v1/prediction/claim
```

Security: `TRADE`

Settles the caller's position in a terminal market — `RESOLVED` or `CANCELLED`. For a `RESOLVED` market, exchanges the winning-outcome tokens for the payout in `quoteAsset`. For a `CANCELLED` market, returns the staked collateral. The call is idempotent: repeating it on an already-settled position succeeds without changing balances.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "predictionMarketAddress": "0:market-address",
  "transactTime": 1710000000000
}
```

Response fields:

| Name | Type | Description |
| --- | --- | --- |
| `predictionMarketAddress` | STRING | Echoed from the request. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the operation was accepted. |

The response confirms acceptance only. The credited collateral becomes visible through [`GET /api/v1/account`](#account-balance) once the chain confirms.

Errors:

| Condition | Code | HTTP |
| --- | --- | --- |
| `predictionMarketAddress` missing | `-1102` | 400 |
| `predictionMarketAddress` not found, or its market has not been reconciled yet | `-1121` | 404 |
| Market status is not `RESOLVED` or `CANCELLED` | `-2010` | 400 |
| Authenticated account has no PrivateNote contract deployed at its resolved address | `-2013` | 404 |
| Trading note is busy with another operation; retry shortly | `-2014` | 429 |
| Backend could not submit the transaction (gateway timeout, malformed reply) | `-1500` | 503 |

## Prediction Trading Endpoints

### New Order

```http
POST /api/v1/prediction/order
```

Security: `TRADE`

Create a single order.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `newOrderClientId` | STRING | NO | Optional client-defined order identifier. If omitted, the API generates a random value and returns it as `clientOrderId` in the response. |
| `side` | ENUM | YES | Order side. See [Order Side](#order-side). |
| `quantity` | DECIMAL | YES | Outcome-token quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the amount of the market `quoteAsset` to spend, for example `USDC`. In that case, `quantityPrecision` and `stepSize` from `/api/v1/prediction/markets` apply to the quote-asset spend amount exactly as sent in the request. |
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
| `clientOrderId` | STRING | Either the `newOrderClientId` sent in the request, or a server-generated identifier when none was provided. Use it to look up the order in `/api/v1/prediction/orders` until the server-assigned `orderId` is available. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the order was accepted. |
| `status` | ENUM | Always [`PENDING_NEW`](#order-status) on success. |

The response confirms acceptance only. The full order state — `orderId`, fills, accepted price — becomes available through [`GET /api/v1/prediction/orders`](#orders) shortly after; look up by `clientOrderId`.

### Cancel Order

```http
DELETE /api/v1/prediction/order
```

Security: `TRADE`

Cancel a single open order by ID.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
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

The response confirms acceptance only. The final outcome — `CANCELED`, or `FILLED` if matching raced the cancel — becomes visible through [`GET /api/v1/prediction/orders`](#orders) shortly after.

### New Batch Orders

```http
POST /api/v1/prediction/batchOrders
```

Security: `TRADE`

Create multiple orders in one request. All orders in the batch are submitted to the single market symbol identified by the top-level `predictionMarketAddress` and `symbol`.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `orders` | ARRAY | YES | List of orders to create on the specified market symbol. Must contain at least one item; the maximum is the outcome's `maxBatchSize` from `/api/v1/prediction/markets`. The backend rejects an empty array before submission with `-1130 / 400`. |

Each order item:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `newOrderClientId` | STRING | NO | Optional client-defined order identifier. If omitted, the API generates a random value and returns it as `clientOrderId` in the response. Each item is generated or accepted independently; intra-batch duplicates are detected by the exchange during placement and surface as `-1130 / 400`. |
| `side` | ENUM | YES | Order side. See [Order Side](#order-side). |
| `quantity` | DECIMAL | YES | Outcome-token quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the amount of the market `quoteAsset` to spend, for example `USDC`. In that case, `quantityPrecision` and `stepSize` from `/api/v1/prediction/markets` apply to the quote-asset spend amount exactly as sent in the request. |
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
  "predictionMarketAddress": "0:market-address",
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
| `clientOrderId` | STRING | Either the `newOrderClientId` sent in the corresponding request item, or a server-generated identifier when none was provided. Use it to look up the order in `/api/v1/prediction/openOrders` until the server-assigned `orderId` is available. |
| `transactTime` | LONG | Server timestamp (Unix ms) when the batch was accepted. The same value is repeated for every item in the response. |
| `status` | ENUM | Always [`PENDING_NEW`](#order-status) on success. |

The response confirms acceptance only. For each item, the full order state — `orderId`, fills, accepted price — becomes available through [`GET /api/v1/prediction/openOrders`](#current-open-orders) shortly after; look up by `clientOrderId`.

Response shape depends on outcome: on success the body is a JSON array with one object per accepted item, in request order; on failure the body is a single standard error envelope (`{ "code": ..., "msg": ... }`), never an array.

Batch creation is atomic: if any order in the batch fails validation, the whole request is rejected and no orders are created. Validation runs item by item in request order and the first failure returns its error code. Atomicity holds on the exchange side too — even after the request is accepted, if the exchange rejects any item during placement the whole batch is reverted.

### Cancel Batch Orders

```http
DELETE /api/v1/prediction/batchOrders
```

Security: `TRADE`

Cancel multiple open orders by IDs.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `orderIds` | ARRAY | YES | List of order IDs to cancel on the specified market symbol. Must contain at least one item; the maximum is the outcome's `maxBatchSize` from `/api/v1/prediction/markets`. The backend rejects an empty array before submission with `-1130 / 400`. Duplicate `orderId` values within the array are rejected with `-1130 / 400` — each id consumes one slot in the chain's batch window, and a duplicate receipt carries no extra signal. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Request body:

```json
{
  "predictionMarketAddress": "0:market-address",
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

The response confirms acceptance only. The final outcome per id — `CANCELED`, or `FILLED` if matching raced the cancel — becomes visible through [`GET /api/v1/prediction/orders`](#orders) shortly after.

### Cancel All Open Orders On Symbol

```http
DELETE /api/v1/prediction/openOrders
```

Security: `TRADE`

Cancel all open orders on one market symbol.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
[
  {
    "predictionMarketAddress": "0:market-address",
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
GET /api/v1/prediction/orders
```

Security: `USER_DATA`

Fetch orders for the authenticated account across all lifecycle states — currently open, filled, canceled, and (once produced by the indexer) rejected. The response never includes orders owned by another account.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `predictionMarketAddress` | STRING | NO | Market address. Together with `symbol`, selects one market symbol. If both are omitted, returns orders across all markets. |
| `symbol` | STRING | NO | Outcome-token symbol. Together with `predictionMarketAddress`, selects one market symbol. If one is sent without the other, the request is invalid. |
| `status` | STRING | NO | Comma-separated list of [Order Status](#order-status) values to include. Allowed: `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `REJECTED`. Tokens are trimmed and de-duplicated. Default: include all statuses. |
| `limit` | INT | NO | Page size. Default: `100`. Range: `[1, 500]`. |
| `cursor` | STRING | NO | Opaque lex-comparable string returned by the server as `nextCursor`. Pass back verbatim to fetch the next page. An empty or whitespace-only cursor returns `-1102 / 400`. A well-formed cursor that lies past the last order returns an empty page with `nextCursor: null` — not an error. Omit for the first page. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Behavior:

- If both `predictionMarketAddress` and `symbol` are omitted, returns orders for the authenticated account across all markets.
- If both `predictionMarketAddress` and `symbol` are sent, returns orders for that one market symbol.
- If only one of `predictionMarketAddress` / `symbol` is sent, returns `-1102` with HTTP `400`.
- If `limit` is outside `[1, 500]`, returns `-1102` with HTTP `400`.
- If `limit` is present but not an integer, returns `-1130` with HTTP `400`.
- If `cursor` is empty or whitespace-only, returns `-1102` with HTTP `400`. A well-formed cursor that points past the current set of orders is not an error — the response is `{ "orders": [], "nextCursor": null }`.
- If `status` contains an unknown token, returns `-1130` with HTTP `400`.
- If the `(predictionMarketAddress, symbol)` pair does not exist, returns `-1121` with HTTP `404`.
- Empty results are returned as `{ "orders": [], "nextCursor": null }`. A page may also return `orders: []` together with a non-null `nextCursor`; clients should keep paging until `nextCursor` is `null`.
- Results are sorted by a single server-internal chain-order key, **descending** (most recently placed first). For all-market requests this ordering is global across all returned orders.
- Pagination is cursor-based on the same chain-order key. The key is set once when the order is placed and never moves for the life of the order — subsequent fills, cancels, and status transitions do not touch it — so concurrent activity between page reads cannot duplicate or skip rows.
- The endpoint is eventually consistent: a freshly placed order may briefly not appear, between the time the public `OrderPlaced` event is indexed and the time the private confirmation that carries owner attribution is indexed.

Response:

```json
{
  "orders": [
    {
      "predictionMarketAddress": "0:market-address",
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
  "nextCursor": "76a23086a00670000000000000000000000000000000000000000000000000000000000000000000003",
  "lastSq": 4289
}
```

Top-level response fields:

| Name | Type | Description |
| --- | --- | --- |
| `orders` | ARRAY | Orders matching the filter, in stable chain-order descending. Empty when there are no matches. |
| `nextCursor` | STRING \| null | Opaque lex-comparable pagination cursor. Pass back verbatim to fetch the next page; do not parse or generate. `null` when the last page has been returned. |
| `lastSq` | LONG | Largest event sequence number the server has emitted on the user-stream for the caller's account at the moment this response was assembled. Used to splice into the [`orderUpdate`](#order-updates) WebSocket stream — see [Splice and Gap Detection](#splice-and-gap-detection). `0` for an account that has never had any order events. |

Order fields:

| Name | Type | Description |
| --- | --- | --- |
| `predictionMarketAddress` | STRING | Market address. |
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
| `PENDING_NEW` | Order accepted by the exchange and not yet on the book. Will transition to `NEW` (or `PARTIALLY_FILLED` if it immediately matches) once visible in `/api/v1/prediction/orders`. |
| `NEW` | Order is open and has no fills. |
| `PARTIALLY_FILLED` | Order is open and partially filled. |
| `PENDING_CANCEL` | Cancel request accepted by the exchange but not yet applied to the book. Will transition to `CANCELED` (or `FILLED` if matching raced the cancel) once the order's stored status flips in `/api/v1/prediction/orders`. |
| `FILLED` | Order is completely filled. |
| `CANCELED` | Order was canceled by the user or system. |
| `REJECTED` | Order was rejected and was not opened. |

## Prediction WebSocket Streams

### Connection

Real-time stream of order lifecycle updates for the authenticated account. Delta-only — no snapshot is sent on subscribe. Clients reconcile current state via [`GET /api/v1/prediction/orders`](#orders) after (re)connect.

Base URL:

```text
wss://api.dex.do.example.com/ws/v1/prediction/user
```

Security: `USER_DATA`. Subscription requires the same signed envelope as private REST endpoints — `X-DODEX-APIKEY`, `timestamp`, `signature`, optional `recvWindow` — see [Signature Formation](#signature-formation). The signature payload is the canonical query string of the subscription parameters (sorted by key, excluding `signature`), HMAC-SHA256 with the API secret.

Subscription request (one frame after socket open):

```json
{
  "method": "orders.subscribe",
  "apiKey": "...",
  "timestamp": 1710000000000,
  "recvWindow": 5000,
  "signature": "..."
}
```

Unsubscribe:

```json
{ "method": "orders.unsubscribe" }
```

Connection lifecycle:

- Frame envelope convention: server→client frames use `e:` (both data `orderUpdate` and control `ping` / `pong`); client→server frames use `method:` (subscriptions and `ping` / `pong`).
- Heartbeat is application-level JSON. The server sends `{"e":"ping","E":<server_ts_ms>}` every 30s; the client MUST reply with `{"method":"pong","E":<server_ts_ms>}` (echoing the ping's `E`) within 60s, otherwise the server closes the socket with WebSocket close code `4001` ("heartbeat timeout"). `E` is set by the side issuing the ping and echoed verbatim by the responder, so the pinger can measure round-trip time against its own clock — this differs from `E` in `orderUpdate`, where `E` is the server's emission time.
- If 45s elapse without **any** frame from the server (ping, pong, or `orderUpdate`), the client MUST send `{"method":"ping","E":<client_ts_ms>}` as a liveness probe. The client SHOULD treat the socket as dead and reconnect if it does not receive `{"e":"pong","E":<client_ts_ms>}` within 60s.
- The server closes the connection after 24 hours of uptime. Clients MUST reconnect and resubscribe.

### Splice and Gap Detection

Every `orderUpdate` carries a per-account contiguous integer `sq`. For one subscription, `sq` of the next event is **exactly** `prev_sq + 1`. The first event a fresh account ever receives carries `sq = 1`. `sq` is scoped to the authenticated account — one client never observes another account's counter.

[`GET /api/v1/prediction/orders`](#orders) returns `lastSq` — the largest `sq` the server has already emitted for the caller's account at the moment the snapshot was assembled. Together with `sq` on each event, this lets clients splice a REST snapshot into the live stream and detect lost events without any server-side replay buffer.

Recommended client algorithm:

```text
on (re)connect:
  open WebSocket, subscribe, start buffering incoming events
  fetch GET /api/v1/prediction/orders → L = lastSq
  expected_next = L + 1
  discard buffered events with sq <= L
  apply remaining buffered events in sq-asc order:
    for each, require sq == expected_next, then expected_next = sq + 1
on every live event:
  if sq != expected_next: gap detected → resnapshot
  apply; expected_next = sq + 1
```

The algorithm above applies to `orderUpdate` frames only. Control frames (`ping`, `pong`) carry no `sq` and do not advance `expected_next` — clients dispatch them on `e` / `method` before running the gap check.

`sq` is monotonic over the life of the account, not the life of the subscription — a reconnect does not reset the counter. The snapshot watermark `lastSq` from `GET /api/v1/prediction/orders` is therefore directly comparable with any `sq` the client has previously stored.

### Order Updates

A single event type, `orderUpdate`, covers the full order lifecycle: acceptance, partial fill, full fill, cancel, reject, expire. Clients dispatch on the pair `x` (what just happened) × `X` (where the order is now).

Field keys are deliberately short (mostly one letter) — a bandwidth optimization. A high-frequency `orderUpdate` stream on a busy account compresses better and saves measurable per-frame bytes on the wire vs. a full-name JSON shape. The same trade-off is why Binance Spot `executionReport` uses single-letter keys; we follow that convention so the keys are aligned with it where the field exists in both APIs — same letter, same meaning, same string-vs-number convention. The single DEX.DO-specific addition is `a` (market address), which has no Binance analog because Binance identifies a market by `symbol` alone.

The short-key convention is scoped to WebSocket frames. REST endpoints keep full descriptive names (`orderId`, `executedQty`, `clientOrderId`, ...) because their payloads are read once per request, not streamed, and human readability outweighs the byte savings.

Partial vs. full fill is signaled by `X`: `PARTIALLY_FILLED` while `z < q`, `FILLED` once `z == q`. Both carry `x: "TRADE"`.

Example — order accepted into the book:

```json
{
  "e": "orderUpdate",
  "E": 1710000000123,
  "a": "0:market-address",
  "s": "PM-2026-ELECTION-YES",
  "i": "123456789",
  "c": "mm-order-0001",
  "S": "BUY",
  "o": "LIMIT",
  "f": "GTC",
  "p": "0.615",
  "q": "1.50",
  "x": "NEW",
  "X": "NEW",
  "l": "0",
  "L": "0",
  "z": "0",
  "n": "0",
  "N": null,
  "t": null,
  "m": null,
  "O": 1710000000100,
  "T": 1710000000100,
  "r": null,
  "sq": 4287
}
```

Example — partial fill:

```json
{
  "e": "orderUpdate",
  "E": 1710000005000,
  "a": "0:market-address",
  "s": "PM-2026-ELECTION-YES",
  "i": "123456789",
  "c": "mm-order-0001",
  "S": "BUY",
  "o": "LIMIT",
  "f": "GTC",
  "p": "0.615",
  "q": "1.50",
  "x": "TRADE",
  "X": "PARTIALLY_FILLED",
  "l": "0.50",
  "L": "0.615",
  "z": "0.50",
  "n": "0.000138",
  "N": "USDC",
  "t": "76a23086a00670000000000000000000000000000000000000000000000000000000000000000000004",
  "m": true,
  "O": 1710000000100,
  "T": 1710000004980,
  "r": null,
  "sq": 4288
}
```

Example — full fill:

```json
{
  "e": "orderUpdate",
  "E": 1710000009000,
  "a": "0:market-address",
  "s": "PM-2026-ELECTION-YES",
  "i": "123456789",
  "c": "mm-order-0001",
  "S": "BUY",
  "o": "LIMIT",
  "f": "GTC",
  "p": "0.615",
  "q": "1.50",
  "x": "TRADE",
  "X": "FILLED",
  "l": "1.00",
  "L": "0.615",
  "z": "1.50",
  "n": "0.000276",
  "N": "USDC",
  "t": "76a23086a00670000000000000000000000000000000000000000000000000000000000000000000005",
  "m": true,
  "O": 1710000000100,
  "T": 1710000008980,
  "r": null,
  "sq": 4289
}
```

Field reference:

| Key | Type | Description |
| --- | --- | --- |
| `e` | STRING | Event name. `"orderUpdate"` for order events; see [Connection](#connection) for `"ping"` / `"pong"` control frames. |
| `E` | LONG | Event time. Server timestamp when the frame was emitted, Unix ms. |
| `a` | STRING | Market address. DEX.DO-specific; no Binance analog. |
| `s` | STRING | Outcome-token symbol. |
| `i` | STRING | Server-assigned `orderId`. Empty string for `x: "REJECTED"` events (the chain never assigns an id to a rejected placement). |
| `c` | STRING | `clientOrderId`. Either the `newOrderClientId` from the request or the server-generated value. Empty string if the order was placed without one. |
| `S` | ENUM | Order side. See [Order Side](#order-side). |
| `o` | ENUM | Order type. See [Order Type](#order-type). |
| `f` | ENUM | Time in force. See [Time In Force](#time-in-force). |
| `p` | DECIMAL | Order limit price, scaled by the outcome price precision. |
| `q` | DECIMAL | Original order quantity, scaled by the outcome quantity precision. |
| `x` | ENUM | Execution type — what just happened. See [Execution Type](#execution-type). |
| `X` | ENUM | Order status after this event. See [Order Status](#order-status). Reuses the REST enum. |
| `l` | DECIMAL | Last fill quantity. `"0"` on non-trade events. |
| `L` | DECIMAL | Last fill price. `"0"` on non-trade events. |
| `z` | DECIMAL | Cumulative filled quantity over the life of the order. |
| `n` | DECIMAL | Commission for the last fill, as a signed decimal string. Negative values are rebates **credited** to the account (see `makerComission` on [`/api/v1/prediction/markets`](#prediction-markets)). `"0"` on non-trade events. |
| `N` | STRING \| null | Commission asset symbol. `null` on non-trade events. |
| `t` | STRING \| null | Trade id for the last fill. Identical to `tradeId` in [`GET /api/v1/prediction/trades`](#prediction-trades) for the same match, so the private fill can be correlated with the public trade tape. `null` on non-trade events. |
| `m` | BOOLEAN \| null | `true` if this fill was on the maker side, `false` for taker, `null` on non-trade events. |
| `O` | LONG | Order creation time, Unix ms. Stable across all events for the same order. |
| `T` | LONG | Transaction time — when this specific event was produced on-chain, Unix ms. |
| `r` | ENUM \| null | Rejection reason. Set when `x == "REJECTED"`; `null` otherwise. |
| `sq` | LONG | Per-account contiguous event counter. The next event for the same account is exactly `prev_sq + 1`. See [Splice and Gap Detection](#splice-and-gap-detection). |

#### Common Enums

##### Execution Type

`x` values:

| Value | Description |
| --- | --- |
| `NEW` | Order accepted into the book. `X` transitions to `NEW`. |
| `TRADE` | Order matched. `X` is `PARTIALLY_FILLED` while `z < q`, `FILLED` once `z == q`. |
| `CANCELED` | Order was canceled. `X` transitions to `CANCELED`. |
| `REJECTED` | Order was rejected and never opened. `X` transitions to `REJECTED`; `i` is empty. |
| `EXPIRED` | Order ran out under its `timeInForce` (e.g. `IOC` / `FOK` could not fill). `X` transitions to `CANCELED`. |

`X` reuses [Order Status](#order-status) — the same enum returned by `GET /api/v1/prediction/orders`.

## Validation Rules

Order creation MUST validate:

| Rule | Source |
| --- | --- |
| `predictionMarketAddress` exists | `/api/v1/prediction/markets` |
| `symbol` exists within the selected market | `/api/v1/prediction/markets` |
| The selected market has `status == "TRADING"` (any other phase rejects order placement) | `/api/v1/prediction/markets` |
| For `LIMIT` orders, `price` decimal places do not exceed `pricePrecision` | `/api/v1/prediction/markets` |
| For `LIMIT` orders, `price` is a multiple of `tickSize` | `/api/v1/prediction/markets` |
| `quantity` decimal places do not exceed `quantityPrecision` | `/api/v1/prediction/markets` |
| `quantity` is a multiple of `stepSize` | `/api/v1/prediction/markets` |
| For `LIMIT` orders, `price * quantity` is at least `minNotional` | `/api/v1/prediction/markets` |
| For `MARKET` buy orders, `quantity` in the market `quoteAsset` is at least `minNotional` | `/api/v1/prediction/markets` |
| For `MARKET` buy orders, `quantityPrecision` and `stepSize` apply to the quote-asset spend amount, not to outcome-token units | `/api/v1/prediction/markets` |
| Account has enough available balance (collateral for buys, outcome tokens for sells) | `/api/v1/account`, `/api/v1/account/balances` |
