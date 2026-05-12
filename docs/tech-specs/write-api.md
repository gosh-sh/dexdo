# Trading Write API Technical Specification

Implementation-facing requirements for the trading write endpoints. The public contract (URLs, field names, parameter rules, error shapes, response examples) lives in [api-spec.md](../api-spec.md). Postgres tables referenced live in [data-schema.md](data-schema.md). The read endpoints (markets, depth, account, open orders, all orders) are in [read-api.md](read-api.md). On-chain order routing semantics are in [../contract-specs/dex-events-routing.md](../contract-specs/dex-events-routing.md).

| Endpoint | Method | api-spec section |
| --- | --- | --- |
| `/api/v1/order` | POST | [New Order](../api-spec.md#new-order) |
| `/api/v1/order` | DELETE | [Cancel Order](../api-spec.md#cancel-order) |
| `/api/v1/batchOrders` | POST | [New Batch Orders](../api-spec.md#new-batch-orders) |
| `/api/v1/batchOrders` | DELETE | [Cancel Batch Orders](../api-spec.md#cancel-batch-orders) |
| `/api/v1/openOrders` | DELETE | [Cancel All Open Orders On Symbol](../api-spec.md#cancel-all-open-orders-on-symbol) |

_Implementation tech spec to be filled in._
