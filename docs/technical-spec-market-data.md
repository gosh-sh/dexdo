# DODEX Technical Specification

## Market Data Endpoints

### GET /api/v1/markets

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

Implementation:

1. Enumerate oracles via `RootOracle.getOracleAddress(name)` or by listening to `OracleDeployed` events.
2. For each Oracle, list its `OracleEventList` instances via `Oracle.getEventListAddress(index)`.
3. On each EventList, read the `_events` public mapping -> returns `{event_id -> EventInfo{event_name, oracle_fee, deadline, outcomeNames, describe, count, trustAddr}}`.
4. For each `(event_id, oracle_list_hash, token_type)` triple, compute the OB address with `DexLib.computeOrderBookAddress(...)` and call `OrderBook.getDetails()` for per-market metadata: `nextOrderId`, `orderCount`, `totalMakerFees`, `totalTakerFees`.
5. Join with precision constants from `modifiers.sol`:
   * `minOrderAmount(token_type)` - min order size (10 NACKL / 100 Shell / 1 USDC)
   * `lotSize(token_type)` - amount quantisation (0.01 token, decimals-adjusted)
   * `TICK_SIZE = 10` - price quantisation (10 bps)
   * No upper bound on price (outcome tokens may trade > 1 collateral unit).
6. Build the public market-data symbol for each outcome as:

```text
symbol = PMP._name + _outcomeNames[i] + _token_type.toEnumVariant
```

Where token-type variants are:
- `1 -> NACKL`
- `2 -> SHELL`
- `3 -> USDC`

### GET /api/v1/depth

Fetch bids and asks for one symbol in one market.

Query parameters:

| Name | Type | Mandatory | Description |
| --- | --- | --- | --- |
| `marketAddress` | STRING | YES | Market address. |
| `symbol` | STRING | YES | Outcome-token symbol. |
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

Implementation:

1. Compute OB address via `DexLib.computeOrderBookAddress(PrivateNoteCode, orderBookCode, event_id, ohash, token_type)`.
2. Fetch `OrderBook._state` (public bytes) via a raw account query.
3. Decode offchain - format: `next_order_id(16) + num_orders(4) + N × 126-byte order records`. Group by `outcomeId × isBuy`, sort by price, sum amount per level.

For a single order: `OrderBook.getOrder(uint128 orderId)` getter.
