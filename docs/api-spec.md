# Dodex API Specification

Status: Draft MVP

This document defines the minimal private API needed for integration.

All trading writes are on-chain actions. The API prepares the data needed to
build a transaction message, but the client forms and sends the final blockchain
message outside this API.

## Table Of Contents

- [Base URL](#base-url)
- [Authentication](#authentication)
- [Integration Configuration](#integration-configuration)
- [Data Types](#data-types)
- [Symbol Model](#symbol-model)
- [Price And Amount Encoding](#price-and-amount-encoding)
- [On-Chain Message Flow](#on-chain-message-flow)
- [Common Enums](#common-enums)
- [Error Response](#error-response)
- [Endpoint Summary](#endpoint-summary)
- [Market Data Endpoints](#market-data-endpoints)
- [Account Endpoints](#account-endpoints)
- [On-Chain Trading Endpoints](#on-chain-trading-endpoints)
- [Order Read Endpoints](#order-read-endpoints)

## Base URL

```text
https://api.dodex.example.com
```

All request and response bodies use JSON.

```http
Content-Type: application/json
```

## Authentication

All API endpoints are private in the MVP version and require one API token.
Each API token is bound to one or more `privateNoteAddresses`, which are the
account IDs for this API.

| Location | Name | Type | Mandatory | Description |
| --- | --- | --- | --- | --- |
| Header | `Authorization` | STRING | YES | `Bearer <token>` |

No per-endpoint security classes are defined in the MVP.

## Integration Configuration

During integration, the integrator provides the `privateNoteAddresses` that
should be used as trading accounts. The API service returns one API token for
that address list.

Runtime API requests do not pass an account ID. The account is selected by the
API token.

Read endpoints aggregate data across all `privateNoteAddresses` bound to the
token. `*/prepare` endpoints use `defaultPrivateNoteAddress` as the target
`PrivateNote` in the MVP.

Token configuration fields:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `token` | STRING | YES | Generated API token returned to the integrator. |
| `privateNoteAddresses` | ARRAY | YES | PrivateNote addresses used as API account IDs. |
| `defaultPrivateNoteAddress` | STRING | YES | PrivateNote address used by `*/prepare` endpoints. |
| `metadata` | OBJECT | NO | Integration metadata for this token. |
| `status` | STRING | YES | Token status. MVP value: `ACTIVE`. |
| `createdAt` | LONG | YES | Token creation timestamp. |

Example token metadata:

```json
{
  "token": "dodex_live_generated_token",
  "privateNoteAddresses": ["0:private-note-address"],
  "defaultPrivateNoteAddress": "0:private-note-address",
  "metadata": {
    "name": "primary-mm",
    "description": "Primary market-making account"
  },
  "status": "ACTIVE",
  "createdAt": 1710000000000
}
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

`outcomeId` is part of the symbol identity. For binary markets, `NO` uses
`outcomeId = 0` and `YES` uses `outcomeId = 1`. API requests use `symbol`; they
do not pass `outcomeId` as a separate trading parameter in the MVP.

## Price And Amount Encoding

TBD. This section is intentionally left for a later pass. Open questions and
rough notes are tracked in `TODO.md`.

## On-Chain Message Flow

For on-chain write endpoints:

1. Client calls an API `*/prepare` endpoint.
2. API returns the target `PrivateNote` call data and an unsigned message payload.
3. Client uses `tvm-sdk` to form/sign the final message.
4. Client sends the final message to:

```text
https://mainnet.ackinacki.org
```

This is intentionally a short placeholder. Exact `tvm-sdk` message-building and
node request details will be filled in later.

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
| `IOC` | Limit immediate-or-cancel order. Any unfilled quantity is canceled. |
| `FOK` | Limit fill-or-kill order. The full quantity must be filled or the order is canceled. |
| `POST_ONLY` | Limit post-only order. The order is canceled if it would immediately match. |

## Error Response

All REST errors use the same response format.

```json
{
  "code": -1121,
  "msg": "Invalid symbol."
}
```

Recommended MVP error codes:

| Code | Message |
| --- | --- |
| `-1000` | Unknown error. |
| `-1002` | Authentication required. |
| `-1102` | Mandatory parameter was not sent. |
| `-1121` | Invalid symbol. |
| `-2010` | Request failed local checks. |
| `-2011` | Unknown order. |

## Endpoint Summary

| Function | Method | Path |
| --- | --- | --- |
| List markets | `GET` | `/api/v1/markets` |
| Fetch order book | `GET` | `/api/v1/depth` |
| Fetch account balance | `GET` | `/api/v1/account` |
| Prepare single order | `POST` | `/api/v1/order/prepare` |
| Prepare single cancel | `POST` | `/api/v1/order/cancel/prepare` |
| Prepare batch orders | `POST` | `/api/v1/batchOrders/prepare` |
| Prepare batch cancel | `POST` | `/api/v1/batchOrders/cancel/prepare` |
| Fetch orders in order book | `GET` | `/api/v1/openOrders` |

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

Fetch account balances. If the API token is bound to multiple
`privateNoteAddresses`, balances are aggregated by asset.

Query parameters:

No endpoint-specific query parameters.

Response:

```json
{
  "balances": [
    {
      "asset": "USDC",
      "free": "25000.00",
      "lockedInOrders": "3750.00"
    }
  ]
}
```

## On-Chain Trading Endpoints

The endpoints below return prepared on-chain message data only. They do not send
messages to the blockchain and do not track execution status.

In every `*/prepare` response, `method` is the target `PrivateNote` method and
`params` contains the exact contract arguments for that method. Contract integer
values wider than 32 bits are encoded as decimal strings in JSON.

### Prepare New Order

```http
POST /api/v1/order/prepare
```

Prepare a single order placement for one outcome symbol.

Request body fields:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Outcome symbol. |
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `orderType` | ENUM | YES | See [Order Type](#order-type). |
| `quantity` | DECIMAL | YES | Outcome-token quantity. |
| `price` | DECIMAL | NO | Limit price. Required for `LIMIT`, `IOC`, `FOK`, and `POST_ONLY`; omitted for `MARKET`. |
| `minQuantity` | DECIMAL | NO | Minimum fill quantity. Default: `"0"`. |
| `epochId` | STRING | NO | Epoch id. |

Request body:

```json
{
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "side": "BUY",
  "orderType": "LIMIT",
  "quantity": "100.00",
  "price": "0.615",
  "minQuantity": "0",
  "epochId": "42"
}
```

Response:

```json
{
  "chain": "ackinacki-mainnet",
  "nodeEndpoint": "https://mainnet.ackinacki.org",
  "privateNoteAddress": "0:private-note-address",
  "method": "placeOrder",
  "params": {
    "event_id": "12345678901234567890",
    "oracle_list_hash": "98765432109876543210",
    "token_type": 3,
    "outcomeId": 1,
    "isBuy": true,
    "price": "6150",
    "amount": "100000000",
    "flags": 0,
    "minAmount": "0",
    "epochId": "42"
  },
  "unsignedMessage": "base64-or-chain-specific-payload"
}
```

`params` contract method:

```solidity
PrivateNote.placeOrder(
  uint256 event_id,
  uint256 oracle_list_hash,
  uint32 token_type,
  uint32 outcomeId,
  bool isBuy,
  uint256 price,
  uint128 amount,
  uint8 flags,
  uint128 minAmount,
  uint64 epochId
)
```

### Prepare Cancel Order

```http
POST /api/v1/order/cancel/prepare
```

Prepare cancellation of one order that is currently in the order book.

Request body fields:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Outcome symbol. |
| `orderId` | STRING | YES | Order ID to cancel. |

Request body:

```json
{
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "orderId": "123456789"
}
```

Response:

```json
{
  "chain": "ackinacki-mainnet",
  "nodeEndpoint": "https://mainnet.ackinacki.org",
  "privateNoteAddress": "0:private-note-address",
  "method": "cancelOrder",
  "params": {
    "event_id": "12345678901234567890",
    "oracle_list_hash": "98765432109876543210",
    "token_type": 3,
    "orderId": "123456789"
  },
  "unsignedMessage": "base64-or-chain-specific-payload"
}
```

`params` contract method:

```solidity
PrivateNote.cancelOrder(
  uint256 event_id,
  uint256 oracle_list_hash,
  uint32 token_type,
  uint128 orderId
)
```

### Prepare New Batch Orders

```http
POST /api/v1/batchOrders/prepare
```

Prepare placement of multiple orders for one outcome symbol.

Request body fields:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Outcome symbol. All orders use this symbol. |
| `orders` | ARRAY | YES | List of orders to create. Min: `1`. Max: `5`. |

Each order item:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `side` | ENUM | YES | `BUY` or `SELL`. |
| `orderType` | ENUM | YES | See [Order Type](#order-type). |
| `quantity` | DECIMAL | YES | Outcome-token quantity. |
| `price` | DECIMAL | NO | Limit price. Required for `LIMIT`, `IOC`, `FOK`, and `POST_ONLY`; omitted for `MARKET`. |
| `minQuantity` | DECIMAL | NO | Minimum fill quantity. Default: `"0"`. |
| `epochId` | STRING | NO | Epoch id. |

Request body:

```json
{
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "orders": [
    {
      "side": "BUY",
      "orderType": "LIMIT",
      "quantity": "100.00",
      "price": "0.615",
      "epochId": "42"
    },
    {
      "side": "SELL",
      "orderType": "LIMIT",
      "quantity": "50.00",
      "price": "0.620",
      "epochId": "42"
    }
  ]
}
```

Response:

```json
{
  "chain": "ackinacki-mainnet",
  "nodeEndpoint": "https://mainnet.ackinacki.org",
  "privateNoteAddress": "0:private-note-address",
  "method": "placeBatch",
  "params": {
    "event_id": "12345678901234567890",
    "oracle_list_hash": "98765432109876543210",
    "token_type": 3,
    "orders": [
      {
        "outcomeId": 1,
        "isBuy": true,
        "flags": 0,
        "price": "6150",
        "amount": "100000000",
        "minAmount": "0",
        "epochId": "42"
      },
      {
        "outcomeId": 1,
        "isBuy": false,
        "flags": 0,
        "price": "6200",
        "amount": "50000000",
        "minAmount": "0",
        "epochId": "42"
      }
    ]
  },
  "unsignedMessage": "base64-or-chain-specific-payload"
}
```

`params` contract method:

```solidity
PrivateNote.placeBatch(
  uint256 event_id,
  uint256 oracle_list_hash,
  uint32 token_type,
  OrderBook.PlaceParams[] orders
)
```

`orders` item contract type:

```solidity
OrderBook.PlaceParams(
  uint32 outcomeId,
  bool isBuy,
  uint8 flags,
  uint256 price,
  uint128 amount,
  uint128 minAmount,
  uint64 epochId
)
```

### Prepare Cancel Batch Orders

```http
POST /api/v1/batchOrders/cancel/prepare
```

Prepare cancellation of multiple orders that are currently in the order book.

Request body fields:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | YES | Outcome symbol. |
| `orderIds` | ARRAY | YES | List of order IDs to cancel. Min: `1`. Max: `5`. |

Request body:

```json
{
  "symbol": "PM-2026-ELECTION-YES-USDC",
  "orderIds": ["123456789", "123456790"]
}
```

Response:

```json
{
  "chain": "ackinacki-mainnet",
  "nodeEndpoint": "https://mainnet.ackinacki.org",
  "privateNoteAddress": "0:private-note-address",
  "method": "cancelBatch",
  "params": {
    "event_id": "12345678901234567890",
    "oracle_list_hash": "98765432109876543210",
    "token_type": 3,
    "orderIds": ["123456789", "123456790"]
  },
  "unsignedMessage": "base64-or-chain-specific-payload"
}
```

`params` contract method:

```solidity
PrivateNote.cancelBatch(
  uint256 event_id,
  uint256 oracle_list_hash,
  uint32 token_type,
  uint128[] orderIds
)
```

## Order Read Endpoints

### Orders In Order Book

```http
GET /api/v1/openOrders
```

Fetch orders that are currently in the order book. If the API token is bound to
multiple `privateNoteAddresses`, orders are returned across all bound addresses.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `symbol` | STRING | NO | Outcome symbol. If omitted, returns orders for all symbols. |

Response:

```json
[
  {
    "symbol": "PM-2026-ELECTION-YES-USDC",
    "outcomeId": 1,
    "orderId": "123456789",
    "side": "BUY",
    "orderType": "LIMIT",
    "price": "0.615",
    "quantity": "100.00",
    "epochId": "42"
  }
]
```
