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
| `STRING` | UTF-8 string | `"PM-2026-ELECTION-NO-USDC"` |
| `DECIMAL` | Decimal number encoded as string | `"0.615"` |
| `LONG` | Integer timestamp or ID | `1710000000000` |
| `INT` | JSON integer | `5` |
| `BOOLEAN` | JSON boolean | `true` |
| `ARRAY` | JSON array | `[]` |
| `OBJECT` | JSON object | `{}` |

All asset amounts and human-facing prices MUST be encoded as strings to avoid
floating-point precision loss.

## Symbol Model

A trading symbol represents one outcome token inside one prediction market.

Each prediction market exposes separate outcome symbols, for example:

```text
PM-2026-ELECTION-NO-USDC
PM-2026-ELECTION-YES-USDC
```

symbol = PMP._name+_outcomeNames[i]+_token_type.toEnumVariant # 1 - NACKL, 2 - SHELL, 3 - USDC


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
- `canonicalRequestBody` is the minified JSON body for requests with a body, or an empty string.
- The signature is HMAC SHA256 using the API secret.

Formula:

```text
signature = HMAC_SHA256(canonicalQueryString + canonicalRequestBody, apiSecret)
```

Example:

```text
canonicalQueryString = recvWindow=5000&timestamp=1710000000000
canonicalRequestBody = {"symbol":"PM-2026-ELECTION-YES-USDC","side":"BUY","quantity":"1.500000","price":"2500.00"}

signature = HMAC_SHA256(
  'recvWindow=5000&timestamp=1710000000000{"symbol":"PM-2026-ELECTION-YES-USDC","side":"BUY","quantity":"1.500000","price":"2500.00"}',
  apiSecret
)
```

## Common Enums

### Order Side

| Value | Description |
| --- | --- |
| `BUY` | Buy the symbol's outcome token using collateral. |
| `SELL` | Sell the symbol's outcome token for collateral. |

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
  "msg": "Invalid symbol."
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
| `-1121` | Invalid symbol. |
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
| Cancel all open orders on pair | `DELETE` | `/api/v1/openOrders` | `TRADE` |
| Fetch open orders | `GET` | `/api/v1/openOrders` | `USER_DATA` |
| Fetch closed and canceled orders | `GET` | `/api/v1/allOrders` | `USER_DATA` |

## Market Data Endpoints

### Markets

```http
GET /api/v1/markets
```

Fetch available prediction markets and their outcome symbols.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketId` | STRING | NO | Return one market only. |

Response:

```json
{
  "serverTime": 1710000000000,
  "markets": [
    {
      "marketId": "PM-2026-ELECTION",
      "name": "2026 Election",
      "status": "TRADING",
      "quoteAsset": "USDC",
      "marketAddress": "0:market-address",
      "outcomes": [
        {
          "outcomeId": 0,
          "outcomeName": "NO",
          "symbol": "PM-2026-ELECTION-NO-USDC",
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
          "symbol": "PM-2026-ELECTION-YES-USDC",
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

Fetch bids and asks for one outcome symbol.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Outcome symbol. |
| `limit` | INT | NO | Number of price levels per side. Default: `100`. Max: `1000`. |

Response:

```json
{
  "symbol": "PM-2026-ELECTION-YES-USDC",
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

Fetch account balances.

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
      "asset": "ETH",
      "free": "10.000000",
      "locked": "1.500000"
    },
    {
      "asset": "USDC",
      "free": "25000.00",
      "locked": "3750.00"
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
| `symbol` | STRING | YES | Market symbol. Example: `PM-2026-ELECTION-YES-USDC`. |
| `newOrderClientId` | STRING | NO | Optional client-defined order identifier. If omitted, the API generates a random value and returns it as `clientOrderId` in the response. |
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `quantity` | DECIMAL | YES | Base asset quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the collateral amount to spend. |
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
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "orderId": "123456789",
  "clientOrderId": "mm-order-0001",
  "transactTime": 1710000000000,
  "price": "2500.00",
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
| `symbol` | STRING | YES | Market symbol. Example: `PM-2026-ELECTION-YES-USDC`. |
| `orderId` | STRING | YES | Order ID to cancel. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "orderId": "123456789",
  "price": "2500.00",
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
All orders in the batch are submitted to the single order book identified by the top-level `symbol`.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Market symbol. Example: `PM-2026-ELECTION-YES-USDC`. |
| `orders` | ARRAY | YES | List of orders to create on the specified market. Max: `20`. |

Each order item:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `newOrderClientId` | STRING | NO | Optional client-defined order identifier. If omitted, the API generates a random value and returns it as `clientOrderId` in the response. |
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `quantity` | DECIMAL | YES | Base asset quantity. Must follow `stepSize`. For `MARKET` buy orders this field represents the collateral amount to spend. |
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
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "orders": [
    {
      "newOrderClientId": "mm-order-0001",
      "side": "BUY",
      "quantity": "1.500000",
      "price": "2500.00",
      "type": "LIMIT",
      "timeInForce": "GTC"
    },
    {
      "newOrderClientId": "mm-order-0002",
      "side": "SELL",
      "quantity": "0.750000",
      "price": "2600.00",
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
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "orderId": "123456789",
    "clientOrderId": "mm-order-0001",
    "transactTime": 1710000000000,
    "price": "2500.00",
    "origQty": "1.500000",
    "executedQty": "0.000000",
    "status": "NEW",
    "timeInForce": "GTC",
    "type": "LIMIT",
    "side": "BUY"
  },
  {
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "orderId": "123456790",
    "clientOrderId": "mm-order-0002",
    "transactTime": 1710000000000,
    "price": "2600.00",
    "origQty": "0.750000",
    "executedQty": "0.000000",
    "status": "NEW",
    "timeInForce": "GTC",
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
| `symbol` | STRING | YES | Market symbol. Example: `PM-2026-ELECTION-YES-USDC`. |
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
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "orderIds": ["123456789", "123456790"]
}
```

Response:

```json
[
  {
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "orderId": "123456789",
    "price": "2500.00",
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
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "orderId": "123456790",
    "price": "2600.00",
    "origQty": "0.750000",
    "executedQty": "0.000000",
    "status": "CANCELED",
    "timeInForce": "GTC",
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

Cancel all open orders on a market pair.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Market symbol. Example: `PM-2026-ELECTION-YES-USDC`. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
[
  {
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "orderId": "123456789",
    "status": "CANCELED",
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
| `symbol` | STRING | NO | Market symbol. If omitted, returns open orders for all symbols. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
[
  {
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "orderId": "123456789",
    "price": "2500.00",
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
| `symbol` | STRING | NO | Market symbol. If omitted, returns history for all symbols. |
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
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "orderId": "123456789",
    "price": "2500.00",
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
| `symbol` exists and has `status = TRADING` | `/api/v1/markets` |
| `price` decimal places do not exceed `pricePrecision` | `/api/v1/markets` |
| `price` is a multiple of `tickSize` | `/api/v1/markets` |
| `quantity` decimal places do not exceed `quantityPrecision` | `/api/v1/markets` |
| `quantity` is a multiple of `stepSize` | `/api/v1/markets` |
| For `LIMIT` orders, `price * quantity` is at least `minNotional`; for `MARKET` buy orders, `quantity` is at least `minNotional` | `/api/v1/markets` |
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
- Single, batch, and pair-wide order cancellation.

Not included in this API version:

- Stop orders.
- OCO orders.
- Iceberg orders.
- Trailing orders.
- Post-only orders.
- Margin or borrow endpoints.
- Deposits and withdrawals.
- WebSocket streams.
- User data stream listen keys.
