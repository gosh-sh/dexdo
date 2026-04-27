- [Dodex REST API Specification](#dodex-rest-api-specification)
  - [Reference Style](#reference-style)
  - [Base URL](#base-url)
  - [Data Types](#data-types)
  - [Symbol Model](#symbol-model)
  - [Security Types](#security-types)
    - [Signature Formation](#signature-formation)
  - [Common Enums](#common-enums)
    - [Order Side](#order-side)
    - [Order Type](#order-type)
    - [Time In Force](#time-in-force)
    - [Order Status](#order-status)
  - [Error Response](#error-response)
  - [Endpoint Summary](#endpoint-summary)
  - [Market Data Endpoints](#market-data-endpoints)
    - [Markets](#markets)
    - [Order Book](#order-book)
  - [Account Endpoints](#account-endpoints)
    - [Account Balance](#account-balance)
  - [Trading Endpoints](#trading-endpoints)
    - [New Order](#new-order)
    - [Cancel Order](#cancel-order)
    - [New Batch Orders](#new-batch-orders)
    - [Cancel Batch Orders](#cancel-batch-orders)
    - [Cancel All Open Orders On Symbol](#cancel-all-open-orders-on-symbol)
    - [Current Open Orders](#current-open-orders)
    - [Closed And Canceled Orders](#closed-and-canceled-orders)
  - [Validation Rules](#validation-rules)
  - [Minimal Trading Scope](#minimal-trading-scope)

# Dodex REST API Specification

Status: Draft

This document defines the minimal REST API required for basic spot-style trading on Dodex.
The API shape follows the Binance Spot REST style where useful, but intentionally removes
advanced order types, margin, OCO, strategy parameters, iceberg orders, STP, trailing stops,
and other non-basic trading features.

## Reference Style

- Binance Spot REST general API information:
  https://developers.binance.com/docs/binance-spot-api-docs/rest-api/general-api-information
- Binance Spot REST market data endpoints:
  https://developers.binance.com/docs/binance-spot-api-docs/rest-api/market-data-endpoints
- Binance Spot REST trading endpoints:
  https://developers.binance.com/docs/binance-spot-api-docs/rest-api/trading-endpoints
- Binance Spot REST account endpoints:
  https://developers.binance.com/docs/binance-spot-api-docs/rest-api/account-endpoints

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

- `marketAddress` is the unique on-chain address of the market and is used in requests. Example: `0:market-address`.
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

## Common Enums

### Order Side

| Value | Description |
| --- | --- |
| `BUY` | Buy the selected outcome token using the market quote asset. |
| `SELL` | Sell the selected outcome token for the market quote asset. |

### Order Type

| Value | Description |
| --- | --- |
| `LIMIT` | Limit order. Unfilled quantity remains in the order book. |
| `MARKET` | Market order. Matches available liquidity and does not rest in the order book. |

### Time In Force

`timeInForce` values:

| Value | Description |
| --- | --- |
| `GTC` | Good Til Canceled. An order will be on the book unless the order is canceled. |
| `IOC` | Immediate Or Cancel. An order will try to fill the order as much as it can before the order expires. |
| `FOK` | Fill or Kill. An order will expire if the full order cannot be filled upon execution. |
| `POST_ONLY` | Post Only. An order must rest on the book and will be canceled if it would immediately match. |

### Order Status

| Value | Description |
| --- | --- |
| `NEW` | Order is open and has no fills. |
| `PARTIALLY_FILLED` | Order is open and partially filled. |
| `FILLED` | Order is completely filled. |
| `CANCELED` | Order was canceled by the user or system. |
| `REJECTED` | Order was rejected and was not opened. |

## Error Response

All errors use the same response format.

```json
{
  "code": -1121,
  "msg": "Invalid market or symbol."
}
```

Recommended common error codes:

| Code | Message |
| --- | --- |
| `-1000` | Unknown error. |
| `-1002` | Authentication required. |
| `-1021` | Timestamp outside recvWindow. |
| `-1022` | Invalid signature. |
| `-1102` | Mandatory parameter was not sent. |
| `-1111` | Precision is over the maximum defined for this asset. |
| `-1121` | Invalid market or symbol. |
| `-2010` | Order would immediately fail validation. |
| `-2011` | Unknown order. |

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
| Fetch open orders | `GET` | `/api/v1/openOrders` | `USER_DATA` |
| Fetch closed and canceled orders | `GET` | `/api/v1/allOrders` | `USER_DATA` |

## Market Data Endpoints

### Markets

```http
GET /api/v1/markets
```

Fetch available prediction markets and their outcomes.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | NO | Return one market only. |

Response:

```json
{
  "serverTime": 1710000000000,
  "markets": [
    {
      "quoteAsset": "USDC",
      "marketAddress": "0:market-address",
      "marketName": "PM-2026-ELECTION",
      "status": "TRADING",
      "outcomes": [
        {
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
  "lastUpdateId": 1027024,
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
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `quantity` | DECIMAL | YES | Outcome-token quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the amount of the market `quoteAsset` to spend, for example `USDC`. In that case, `quantityPrecision` and `stepSize` from `/api/v1/markets` apply to the quote-asset spend amount exactly as sent in the request. |
| `price` | DECIMAL | NO | Required for `LIMIT` orders. Must follow `tickSize`. |
| `type` | ENUM | NO | Supported values: `LIMIT`, `MARKET`. Default: `LIMIT`. |
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
  "marketAddress": "0:market-address",
  "symbol": "PM-2026-ELECTION-YES",
  "orderId": "123456789",
  "clientOrderId": "mm-order-0001",
  "transactTime": 1710000000000,
  "price": "0.615",
  "origQty": "1.500000",
  "executedQty": "0.000000",
  "status": "NEW",
  "timeInForce": "GTC",
  "type": "LIMIT",
  "side": "BUY"
}
```

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
```

### New Batch Orders

```http
POST /api/v1/batchOrders
```

Security: `TRADE`

Create multiple orders in one request.
All orders in the batch are submitted to the single market symbol identified by the top-level `marketAddress` and `symbol`.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. Example: `0:market-address`. |
| `symbol` | STRING | YES | Outcome-token symbol. Example: `PM-2026-ELECTION-YES`. |
| `orders` | ARRAY | YES | List of orders to create on the specified market symbol. Max: `20`. |

Each order item:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `newOrderClientId` | STRING | NO | Optional client-defined order identifier. If omitted, the API generates a random value and returns it as `clientOrderId` in the response. |
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `quantity` | DECIMAL | YES | Outcome-token quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the amount of the market `quoteAsset` to spend, for example `USDC`. In that case, `quantityPrecision` and `stepSize` from `/api/v1/markets` apply to the quote-asset spend amount exactly as sent in the request. |
| `price` | DECIMAL | NO | Required for `LIMIT` orders. Must follow `tickSize`. |
| `type` | ENUM | NO | Supported values: `LIMIT`, `MARKET`. Default: `LIMIT`. |
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
      "newOrderClientId": "mm-order-0001",
      "side": "BUY",
      "quantity": "1.500000",
      "price": "0.615",
      "type": "LIMIT",
      "timeInForce": "GTC"
    },
    {
      "newOrderClientId": "mm-order-0002",
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
    "marketAddress": "0:market-address",
    "symbol": "PM-2026-ELECTION-YES",
    "orderId": "123456789",
    "clientOrderId": "mm-order-0001",
    "transactTime": 1710000000000,
    "price": "0.615",
    "origQty": "1.500000",
    "executedQty": "0.000000",
    "status": "NEW",
    "timeInForce": "GTC",
    "type": "LIMIT",
    "side": "BUY"
  },
  {
    "marketAddress": "0:market-address",
    "symbol": "PM-2026-ELECTION-YES",
    "orderId": "123456790",
    "clientOrderId": "mm-order-0002",
    "transactTime": 1710000000000,
    "price": "0.620",
    "origQty": "0.750000",
    "executedQty": "0.000000",
    "status": "NEW",
    "timeInForce": "POST_ONLY",
    "type": "LIMIT",
    "side": "SELL"
  }
]
```

Batch creation is atomic at the API validation level: if any order is invalid, the whole request
is rejected and no orders are created.

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
| `orderIds` | ARRAY | YES | List of order IDs to cancel. Max: `20`. |

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
  },
  {
    "marketAddress": "0:market-address",
    "symbol": "PM-2026-ELECTION-YES",
    "orderId": "123456790",
    "price": "0.620",
    "origQty": "0.750000",
    "executedQty": "0.000000",
    "status": "CANCELED",
    "timeInForce": "POST_ONLY",
    "type": "LIMIT",
    "side": "SELL",
    "time": 1710000000000,
    "updateTime": 1710000010000
  }
]
```

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

### Current Open Orders

```http
GET /api/v1/openOrders
```

Security: `USER_DATA`

Fetch currently open orders.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | NO | Market address. Together with `symbol`, selects one market symbol. If both are omitted, returns open orders for all markets. |
| `symbol` | STRING | NO | Outcome-token symbol. Together with `marketAddress`, selects one market symbol. If one is sent without the other, the request is invalid. |
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
    "executedQty": "0.500000",
    "status": "PARTIALLY_FILLED",
    "timeInForce": "GTC",
    "type": "LIMIT",
    "side": "BUY",
    "time": 1710000000000,
    "updateTime": 1710000001000
  }
]
```

### Closed And Canceled Orders

```http
GET /api/v1/allOrders
```

Security: `USER_DATA`

Fetch filled and canceled order history.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | NO | Market address. Together with `symbol`, selects one market symbol. If both are omitted, returns history for all markets. |
| `symbol` | STRING | NO | Outcome-token symbol. Together with `marketAddress`, selects one market symbol. If one is sent without the other, the request is invalid. |
| `status` | ENUM | NO | Filter by status: `FILLED`, `CANCELED`, or `REJECTED`. |
| `startTime` | LONG | NO | Start time in milliseconds. |
| `endTime` | LONG | NO | End time in milliseconds. |
| `limit` | INT | NO | Number of orders. Default: `100`. Max: `1000`. |
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
    "executedQty": "1.500000",
    "status": "FILLED",
    "timeInForce": "GTC",
    "type": "LIMIT",
    "side": "BUY",
    "time": 1710000000000,
    "updateTime": 1710000100000
  }
]
```

## Validation Rules

Order creation MUST validate:

| Rule | Source |
| --- | --- |
| `marketAddress` exists | `/api/v1/markets` |
| `symbol` exists within the selected market | `/api/v1/markets` |
| The selected market has `status = TRADING` | `/api/v1/markets` |
| For `LIMIT` orders, `price` decimal places do not exceed `pricePrecision` | `/api/v1/markets` |
| For `LIMIT` orders, `price` is a multiple of `tickSize` | `/api/v1/markets` |
| `quantity` decimal places do not exceed `quantityPrecision` | `/api/v1/markets` |
| `quantity` is a multiple of `stepSize` | `/api/v1/markets` |
| For `LIMIT` orders, `price * quantity` is at least `minNotional` | `/api/v1/markets` |
| For `MARKET` buy orders, `quantity` in the market `quoteAsset` is at least `minNotional` | `/api/v1/markets` |
| For `MARKET` buy orders, `quantityPrecision` and `stepSize` apply to the quote-asset spend amount, not to outcome-token units | `/api/v1/markets` |
| Account has enough available balance | `/api/v1/account` |

## Minimal Trading Scope

Supported in this API version:

- Limit and market orders.
- Buy and sell sides only.
- `GTC`, `IOC`, `FOK`, and `POST_ONLY` for limit orders.
- Public market data.
- Account balances.
- Open order and closed order history.
- Single and batch order creation.
- Single, batch, and symbol-wide order cancellation.

Not Included in this API version:

- WebSocket for fills.
- Maker and taker fees per market. This can be exposed either in `/api/v1/markets` as `makerFee` and `takerFee`, or via a separate `/api/v1/fees` endpoint.
