- [Dodex REST API Specification](#dodex-rest-api-specification)
  - [Reference Style](#reference-style)
  - [Base URL](#base-url)
  - [Data Types](#data-types)
  - [Security Types](#security-types)
    - [Signature Formation](#signature-formation)
  - [Common Enums](#common-enums)
    - [Order Side](#order-side)
    - [Order Type](#order-type)
    - [Time In Force](#time-in-force)
    - [Order Status](#order-status)
  - [Error Response](#error-response)
  - [Common Objects](#common-objects)
    - [Order](#order)
  - [Endpoint Summary](#endpoint-summary)
  - [Market Data Endpoints](#market-data-endpoints)
    - [Exchange Information](#exchange-information)
    - [Ticker](#ticker)
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
| `STRING` | UTF-8 string | `"ETHUSDC"` |
| `DECIMAL` | Decimal number encoded as string | `"2500.12"` |
| `LONG` | Integer timestamp or ID | `1710000000000` |
| `ARRAY` | JSON array | `[]` |

All asset amounts and prices MUST be returned as strings to avoid floating point precision loss.

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
canonicalRequestBody = {"symbol":"ETHUSDC","side":"BUY","quantity":"1.500000","price":"2500.00"}

signature = HMAC_SHA256(
  'recvWindow=5000&timestamp=1710000000000{"symbol":"ETHUSDC","side":"BUY","quantity":"1.500000","price":"2500.00"}',
  apiSecret
)
```

## Common Enums

### Order Side

| Value | Description |
| --- | --- |
| `BUY` | Buy base asset using quote asset. |
| `SELL` | Sell base asset for quote asset. |

### Order Type

| Value | Description |
| --- | --- |
| `LIMIT` | Limit order. This is the only supported order type in this API version. |

### Time In Force

| Value | Description |
| --- | --- |
| `GTC` | Good till canceled. This is the default and only required mode for v1. |

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

## Common Objects

### Order

```json
{
  "symbol": "ETHUSDC",
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
```

Fields:

| Name | Type | Description |
| --- | --- | --- |
| `symbol` | STRING | Market symbol. |
| `orderId` | STRING | Dodex order ID. |
| `price` | DECIMAL | Limit price. |
| `origQty` | DECIMAL | Original base asset quantity. |
| `executedQty` | DECIMAL | Filled base asset quantity. |
| `status` | ENUM | Order status. |
| `timeInForce` | ENUM | Time in force. |
| `type` | ENUM | Order type. |
| `side` | ENUM | Order side. |
| `time` | LONG | Creation time in milliseconds. |
| `updateTime` | LONG | Last update time in milliseconds. |

## Endpoint Summary

| Function | Method | Path | Security |
| --- | --- | --- | --- |
| List markets and limits | `GET` | `/api/v1/exchangeInfo` | `NONE` |
| Fetch ticker | `GET` | `/api/v1/ticker/24hr` | `NONE` |
| Fetch order book | `GET` | `/api/v1/depth` | `NONE` |
| Fetch account balance | `GET` | `/api/v1/account` | `USER_DATA` |
| Create single limit order | `POST` | `/api/v1/order` | `TRADE` |
| Cancel single order by ID | `DELETE` | `/api/v1/order` | `TRADE` |
| Create batch orders | `POST` | `/api/v1/batchOrders` | `TRADE` |
| Cancel batch orders by IDs | `DELETE` | `/api/v1/batchOrders` | `TRADE` |
| Cancel all open orders on pair | `DELETE` | `/api/v1/openOrders` | `TRADE` |
| Fetch open orders | `GET` | `/api/v1/openOrders` | `USER_DATA` |
| Fetch closed and canceled orders | `GET` | `/api/v1/allOrders` | `USER_DATA` |

## Market Data Endpoints

### Exchange Information

```http
GET /api/v1/exchangeInfo
```

Security: `NONE`

Fetch available markets, pair metadata, asset precision, price precision, amount precision,
tick size, amount step size, and minimum order notional.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | NO | Return metadata for one symbol only. Example: `ETHUSDC`. |

Response:

```json
{
  "timezone": "UTC",
  "serverTime": 1710000000000,
  "symbols": [
    {
      "symbol": "ETHUSDC",
      "status": "TRADING",
      "baseAsset": "ETH",
      "quoteAsset": "USDC",
      "baseAssetPrecision": 18,
      "quoteAssetPrecision": 6,
      "pricePrecision": 18,
      "quantityPrecision": 18,
      "tickSize": "0.000000000000000001",
      "stepSize": "0.000001",
      "minNotional": "10.00"
    }
  ]
}
```

### Ticker

```http
GET /api/v1/ticker/24hr
```

Security: `NONE`

Fetch last price, best bid, best ask, and 24h volume.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Market symbol. Example: `ETHUSDC`. |

Response:

```json
{
  "symbol": "ETHUSDC",
  "lastPrice": "2501.25",
  "bidPrice": "2501.20",
  "askPrice": "2501.30",
  "volume": "12345.678900",
  "quoteVolume": "30900123.45",
  "openTime": 1709913600000,
  "closeTime": 1710000000000
}
```

### Order Book

```http
GET /api/v1/depth
```

Security: `NONE`

Fetch order book bids and asks with requested depth.

Parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Market symbol. Example: `ETHUSDC`. |
| `limit` | INT | NO | Number of price levels per side. Default: `100`. Max: `1000`. |

Response:

```json
{
  "lastUpdateId": 1027024,
  "bids": [
    ["2501.20", "4.125000"],
    ["2501.10", "0.750000"]
  ],
  "asks": [
    ["2501.30", "2.000000"],
    ["2501.40", "1.500000"]
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

Create a single limit order.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Market symbol. Example: `ETHUSDC`. |
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `quantity` | DECIMAL | YES | Base asset quantity. Must follow `stepSize`. |
| `price` | DECIMAL | YES | Limit price. Must follow `tickSize`. |
| `type` | ENUM | NO | Only `LIMIT` is supported. Default: `LIMIT`. |
| `timeInForce` | ENUM | NO | Only `GTC` is required for v1. Default: `GTC`. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "symbol": "ETHUSDC",
  "orderId": "123456789",
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
| `symbol` | STRING | YES | Market symbol. Example: `ETHUSDC`. |
| `orderId` | STRING | YES | Order ID to cancel. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
{
  "symbol": "ETHUSDC",
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

Create multiple limit orders in one request.

Body parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `orders` | ARRAY | YES | List of orders to create. Max: `20`. |

Each order item:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Market symbol. Example: `ETHUSDC`. |
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `quantity` | DECIMAL | YES | Base asset quantity. Must follow `stepSize`. |
| `price` | DECIMAL | YES | Limit price. Must follow `tickSize`. |

Signed query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Request body:

```json
{
  "orders": [
    {
      "symbol": "ETHUSDC",
      "side": "BUY",
      "quantity": "1.500000",
      "price": "2500.00"
    },
    {
      "symbol": "ETHUSDC",
      "side": "SELL",
      "quantity": "0.750000",
      "price": "2600.00"
    }
  ]
}
```

Response:

```json
[
  {
    "symbol": "ETHUSDC",
    "orderId": "123456789",
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
    "symbol": "ETHUSDC",
    "orderId": "123456790",
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
| `symbol` | STRING | YES | Market symbol. Example: `ETHUSDC`. |
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
  "symbol": "ETHUSDC",
  "orderIds": ["123456789", "123456790"]
}
```

Response:

```json
[
  {
    "symbol": "ETHUSDC",
    "orderId": "123456789",
    "status": "CANCELED",
    "updateTime": 1710000010000
  },
  {
    "symbol": "ETHUSDC",
    "orderId": "123456790",
    "status": "CANCELED",
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
| `symbol` | STRING | YES | Market symbol. Example: `ETHUSDC`. |
| `timestamp` | LONG | YES | Unix timestamp in milliseconds. |
| `recvWindow` | LONG | NO | Request validity window in milliseconds. |
| `signature` | STRING | YES | Hex HMAC SHA256 signature generated from `canonicalQueryString + canonicalRequestBody` using the API secret. |

Response:

```json
[
  {
    "symbol": "ETHUSDC",
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
    "symbol": "ETHUSDC",
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
    "symbol": "ETHUSDC",
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
| `symbol` exists and has `status = TRADING` | `/api/v1/exchangeInfo` |
| `price` decimal places do not exceed `pricePrecision` | `/api/v1/exchangeInfo` |
| `price` is a multiple of `tickSize` | `/api/v1/exchangeInfo` |
| `quantity` decimal places do not exceed `quantityPrecision` | `/api/v1/exchangeInfo` |
| `quantity` is a multiple of `stepSize` | `/api/v1/exchangeInfo` |
| `price * quantity` is at least `minNotional` | `/api/v1/exchangeInfo` |
| Account has enough available balance | `/api/v1/account` |

## Minimal Trading Scope

Supported in this API version:

- Limit orders only.
- Buy and sell sides only.
- GTC time in force only.
- Public market data.
- Account balances.
- Open order and closed order history.
- Single and batch order creation.
- Single, batch, and pair-wide order cancellation.

Not included in this API version:

- Market orders.
- Stop orders.
- OCO orders.
- Iceberg orders.
- Trailing orders.
- Post-only orders.
- Margin or borrow endpoints.
- Deposits and withdrawals.
- WebSocket streams.
- User data stream listen keys.
