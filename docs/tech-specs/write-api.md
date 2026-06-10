# Trading Write API Technical Specification

Implementation-facing requirements for the write endpoints (trading, position, and account registration). The public contract (URLs, field names, parameter rules, error shapes, response examples) lives in [api-spec.md](../api-spec.md). Postgres tables referenced below are documented column-by-column in [data-schema.md](data-schema.md). The on-chain side of order routing is in [../contract-specs/dex-events-routing.md](../contract-specs/dex-events-routing.md); authentication and the trading-PN binding are in [auth.md](auth.md). The read endpoint (`GET /orders`) that surfaces post-confirmation order state is in [read-api.md](read-api.md).

| Endpoint | Method | api-spec section |
| --- | --- | --- |
| `/api/v1/order` | POST | [New Order](../api-spec.md#new-order) |
| `/api/v1/order` | DELETE | [Cancel Order](../api-spec.md#cancel-order) |
| `/api/v1/batchOrders` | POST | [New Batch Orders](../api-spec.md#new-batch-orders) |
| `/api/v1/batchOrders` | DELETE | [Cancel Batch Orders](../api-spec.md#cancel-batch-orders) |
| `/api/v1/openOrders` | DELETE | [Cancel All Open Orders On Symbol](../api-spec.md#cancel-all-open-orders-on-symbol) |
| `/api/v1/buyFullSet` | POST | [Buy Full Set](../api-spec.md#buy-full-set) |
| `/api/v1/accounts` | POST | [Register Account](../api-spec.md#register-account) |

## Glossary

**Trading PN** — the `PrivateNote` contract bound to the caller's account. Every order this API places is signed by the trading-PN keypair and submitted as a call to `PrivateNote.placeOrder`. Resolved from the request's `AuthContext` (see [auth.md §Trading Private Note](auth.md#trading-private-note)).

**Chain sender** — the backend component that signs an external message under the trading-PN seckey and dispatches it to the Acki Nacki gateway. Defined as a `ChainOrderSender` trait in `crates/application`; the production implementation in `crates/infrastructure` wraps the `PrivateNote` ABI bindings exposed by `ackinacki-kit/contracts/src/dex/private_note.rs`.

**Optimistic submission** — `POST /api/v1/order` returns once the chain sender has acknowledged dispatch of the external message, before any on-chain confirmation. The chain-assigned `orderId` is not available at response time — it appears later when the indexer projects `OrderBook.OrderPlaced` into [`live_orders`](data-schema.md#live_orders). Clients learn the `orderId` by polling `GET /api/v1/orders` and matching on the `clientOrderId` they supplied or received.

**clientOrderId** — caller-supplied (request field `newOrderClientId`) or backend-generated identifier that correlates the response with the eventually-projected `live_orders` row. Carried by the chain as `uint128` and surfaced in every `OrderBook` event (see [dex-events-routing.md](../contract-specs/dex-events-routing.md#orderbook)). The chain enforces per-PN uniqueness across still-live coids; collisions are silently rejected (`Rejected` event, no `OrderPlaced`).

**PN busy window** — between `PrivateNote.placeOrder` and the matching `onOrderPlaced` callback, the PN's `_busy` flag is set and any further `placeOrder` is rejected on-chain with `ERR_NOTE_BUSY` (`contracts/PrivateNote.sol:1178`). Each account has exactly one trading PN, so placement against one account is serial at the chain level.

## `POST /api/v1/order`

The handler runs three phases: request parsing → market/outcome resolution and input validation → chain submission. Each phase fails closed with its own error code (see [Error mapping](#error-mapping)); a later phase only runs once the earlier one has produced a fully-typed value.

### Authorization

The HMAC auth hoop runs before the handler (see [auth.md §Authentication](auth.md#authentication)) and places the resolved `AuthContext` in the depot. The handler calls `require_auth(depot, Permission::Trade)`, the only entry point through which a protected handler obtains `AuthContext`. The helper signature carries the required permission so a new protected endpoint cannot read the caller's identity without naming an authorization requirement.

`AuthContext` carries the [`TradingPn`](auth.md#trading-private-note) struct (`pn_address`, `pn_pubkey`, `pn_dih`, decrypted `pn_seckey`). The seckey is read out only inside the chain sender; the use case sees an opaque `TradingPn` and never logs the secret bytes.

### Request parsing

Body fields are taken byte-exact from the request as transmitted; the HMAC layer has already verified the signature over those exact bytes, and re-serialization would invalidate it. Mandatory-field absence returns `MissingParameter` → 400; an unknown enum value (`side`, `type`, `timeInForce`) returns `InvalidParameter` → 400.

| Field | Type | Notes |
| --- | --- | --- |
| `marketAddress` | `MarketAddress` | Mandatory. |
| `symbol` | `Symbol` | Mandatory. |
| `newOrderClientId` | `Option<String>` | Optional; absent → backend generates (see [clientOrderId](#clientorderid-generation)). |
| `side` | `OrderSide` | Mandatory; `BUY` or `SELL`. |
| `quantity` | `String` | Mandatory; decimal. Kept as a string until precision validation. |
| `price` | `Option<String>` | Required for `LIMIT`; rejected for `MARKET`. |
| `type` | `Option<OrderType>` | Defaults to `LIMIT`. |
| `timeInForce` | `Option<TimeInForce>` | Defaults to `GTC` for `LIMIT`; ignored for `MARKET`. |

### Market and outcome resolution

Resolve `(marketAddress, symbol)` to a single row via [`markets`](data-schema.md#markets) ⨝ [`market_outcomes`](data-schema.md#market_outcomes), filtered by `m.last_reconciled_at IS NOT NULL` — the same visibility gate as [`/api/v1/markets`](read-api.md#visibility-filter). A miss surfaces as `InvalidMarketOrSymbol` → 404.

Derive `status` from the row and request `now` using the logic in [read-api.md §Status derivation](read-api.md#status-derivation). Placement is permitted only when `status == TRADING`; any other phase rejects with `OrderValidationFailed` → 400 (-2010). The status derivation and the precision/step columns come from the same SELECT, so a status flip between read and validate is not possible inside one request.

The same row supplies every value the chain submission requires:

| Source column | Bound to |
| --- | --- |
| `markets.event_id` | `placeOrder.eventId` (uint256). |
| `markets.oracle_list_hash` | `placeOrder.oracleListHash` (uint256). Stamped by the market reconciler from `PMP.getDetails().oracleListHash`. NULL on a reconciled row → `MarketInconsistent` → 503. |
| `markets.token_type` | `placeOrder.tokenType` (uint32). `markets.token_code` is the human-readable alias (e.g. `"NACKL"`); the chain expects the integer. |
| `markets.orderbook_address` | Used for response correlation; non-null on every reconciled row (CHECK pinned by migration 0014, mirrored on the read side in [read-api.md §Empty-book contract](read-api.md#empty-book-contract)). Blank → `MarketInconsistent` → 503. |
| `market_outcomes.outcome_id` | `placeOrder.outcomeId` (uint32). |
| `market_outcomes.price_precision` / `tick_size` | Price scaling and tick-size validation. |
| `market_outcomes.quantity_precision` / `step_size` | Quantity scaling and step-size validation. |
| `market_outcomes.min_notional` | Notional validation. |

### Input validation

Each [api-spec §Validation Rules](../api-spec.md#validation-rules) row maps to one check. Inputs are exact-decimal at this point; comparisons use `num-bigint::BigUint` lifted by `price_precision` / `quantity_precision`, the inverse of the lifting `/api/v1/depth` uses to render levels ([read-api.md §Aggregation](read-api.md#aggregation)). Lexicographic string comparison would silently misrank `"100"` vs `"99"`.

| api-spec rule | Failure |
| --- | --- |
| `marketAddress` / `symbol` resolve | `InvalidMarketOrSymbol` |
| Market `status == TRADING` | `OrderValidationFailed` |
| Valid `type` × `timeInForce` combination (see [Flags](#flags)) | `InvalidParameter` |
| `price` decimals ≤ `pricePrecision` (LIMIT) | `PrecisionExceeded` |
| `price` is a multiple of `tickSize` (LIMIT) | `PrecisionExceeded` |
| `quantity` decimals ≤ `quantityPrecision` | `PrecisionExceeded` |
| `quantity` is a multiple of `stepSize` | `PrecisionExceeded` |
| `price * quantity ≥ minNotional` (LIMIT) | `OrderValidationFailed` |
| `quantity ≥ minNotional` in quote (MARKET BUY) | `OrderValidationFailed` |
| MARKET BUY precision/step apply to the quote-asset amount | `PrecisionExceeded` |

The local checks duplicate the contract's own validation (`contracts/PrivateNote.sol:1179-1197`) and exist to surface a fast `-1111` / `-2010` to a misbehaving client without spending a chain round-trip on a doomed submission. The chain remains the authority.

Balance is not pre-checked. The chain enforces sufficiency on-chain (`ERR_LOW_VALUE` at `contracts/PrivateNote.sol:1219`); clients track their own available balance via `GET /api/v1/account`. The chain rejection itself surfaces synchronously through [Failure surface](#failure-surface) §2 — `BeeDexChainSender` waits for the `PrivateNote.placeOrder` execution, so an insufficient-balance reject becomes `OrderValidationFailed` → 400 / -2010 on the HTTP response rather than silent absence in `/api/v1/orders`.

### Flags

The chain takes a `uint8 flags` argument encoding order type and time-in-force (constants in `contracts/modifiers/modifiers.sol`, parameter doc in `contracts/PrivateNote.sol:1160`):

| Bit | Constant | Meaning |
| --- | --- | --- |
| `0x01` | `FLAG_IOC` | Immediate-or-cancel. |
| `0x02` | `FLAG_FOK` | Fill-or-kill. |
| `0x04` | `FLAG_MARKET` | Market order; price ignored on-chain. For BUY, `amount` is interpreted as the quote-asset spend amount. |
| `0x08` | `FLAG_POST_ONLY` | Maker-only; cancelled if it would cross. |

Mapping from the public `type` × `timeInForce`:

| `type` | `timeInForce` | `flags` |
| --- | --- | --- |
| `LIMIT` | `GTC` | `0x00` |
| `LIMIT` | `IOC` | `0x01` |
| `LIMIT` | `FOK` | `0x02` |
| `LIMIT` | `POST_ONLY` | `0x08` |
| `MARKET` | — (api-spec ignores `timeInForce` on `MARKET`) | `0x04` |

The following combinations are rejected with `InvalidParameter` → 400 before any chain submission:

- `MARKET` with `POST_ONLY` — semantically contradictory.
- `MARKET` with `GTC` or `FOK` — `MARKET` orders never rest and have IOC semantics by construction.
- Any other unmapped combination.

The mapping table lives next to the `OrderType` / `TimeInForce` domain enums so a TIF added on the public side cannot be silently dropped on the chain side.

### `clientOrderId` generation

If `newOrderClientId` is absent the handler generates a fresh value. The on-chain ABI is `uint128` (`contracts/PrivateNote.sol:1174`, `ackinacki-kit/contracts/src/dex/private_note.rs::ParamsOfPlaceOrder`) and the read-model storage type in [`live_orders.client_order_id`](data-schema.md#live_orders) is `numeric(78,0)`, both of which accept the full 128-bit range. **The public API surface is narrower: `uint64`**. The reason is a serialization-path constraint, not an ABI one — `bee_dex::Dex::place_order` reaches `ackinacki-kit::PrivateNote::place_order` which constructs the call set via `serde_json::json!(params)`. Without the `arbitrary_precision` feature (not enabled in the current `ackinacki-kit` build), `serde_json` rejects any `u128` value greater than `u64::MAX` with `"number out of range"`, which `json!` then `.unwrap()`s — panicking the worker.

Until the upstream SDK enables `arbitrary_precision`, both paths therefore enforce the u64 ceiling at the public boundary:

- **Backend-generated coid**: `(Uuid::new_v4().as_u128() as u64).to_string()` — keeps the low 64 bits of a fresh UUIDv4. 2 bits of those are the UUID variant constant, leaving 62 random bits — collision space 2^62 ≈ 4.6 × 10^18 is cosmologically safe.
- **Caller-supplied `newOrderClientId`**: validated as `u64::from_str` in the use case. Values that overflow u64 (or are non-numeric) surface as `InvalidParameter` → 400 / -1130 before reaching the chain sender.

The backend does not deduplicate coids against past requests; `OrderBook.placeOrder` enforces uniqueness across the PN's still-live coids on-chain and rejects collisions with a `Rejected` event (no `OrderPlaced`). A coid is free to reuse once the corresponding order is FILLED or CANCELLED; the API does not track this lifecycle and does not block coid reuse.

<!-- TODO(Tracking: NODE-XXXX): widen the public coid space to the full `uint128` range
once the upstream `ackinacki-kit` build enables `serde_json`'s
`arbitrary_precision` feature on its TVM-client dependency. Until
then the chain ABI is uint128 in name but uint64 in practice;
clients and SDK authors must NOT assume more than 64 bits. -->


### Chain submission

Encode and dispatch a `PrivateNote.placeOrder` external message against `trading_pn.pn_address`. ABI from `contracts/PrivateNote.sol:1163`, exposed by `ackinacki-kit/contracts/src/dex/private_note.rs::ParamsOfPlaceOrder`:

```text
placeOrder(
  eventId,         // uint256, markets.event_id
  oracleListHash,  // uint256, markets.oracle_list_hash
  tokenType,       // uint32,  markets.token_code
  outcomeId,       // uint32,  market_outcomes.outcome_id
  isBuy,           // bool,    side == BUY
  price,           // uint256, basis points (probability × FULL_PERCENT 10_000); ignored on FLAG_MARKET
  amount,          // uint128, token atoms (lifted by quote decimals; quote-asset spend on MARKET BUY)
  flags,           // uint8,   see Flags
  minAmount,       // uint128, partial-fill minimum; this API always sends 0
  epochId,         // uint64,  dark-order-book matching; this API always sends 0
  clientOrderId,   // uint128 ABI, but capped at u64 today (see §clientOrderId generation)
)
```

`minAmount` and `epochId` are constant `0` — neither is exposed by api-spec.md and neither has a per-order meaning in this version of the public API.

Two sender boundaries:

- **`ChainOrderSender` trait** (`crates/application/src/lib.rs`) — `async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError>`. Mirrors the existing [`Authenticator`](auth.md#authentication) pattern. The use case depends only on the trait; production wiring and tests inject different implementations.
- **`BeeDexChainSender` impl** (`crates/infrastructure/src/chain_sender.rs`) — wraps `bee_dex::Dex::place_order`. Re-encodes `pn_pubkey` from decimal to hex and `pn_seckey` from bytes to hex to build a `KeyPair`, parses `amount` and `client_order_id` from decimal strings to `u128`, and translates known TVM `exit_code`s back into typed `DomainError` variants (the table in [Failure surface](#failure-surface)).

`bee_dex::Dex::place_order` **waits for the chain to execute `PrivateNote.placeOrder` on the trading PN** and returns the TVM exit code on `require(...)` failure. `map_bee_dex_error` translates known PrivateNote-side codes from `contracts/modifiers/errors.sol` into typed `DomainError` variants — the HTTP caller therefore gets a synchronous, specific reject for the common rejection cases (insufficient balance, PN busy, etc.) rather than an opaque 500. The `OrderBook` side runs as an internal message after `placeOrder` returns; rejections there (`OrderBook.Rejected` for coid collision / queue overflow / ABI validation) are not visible at submission time. See [Failure surface](#failure-surface) for the full split.

### Response

A successful submission returns a deliberately minimal three-field body:

| Field | Source |
| --- | --- |
| `clientOrderId` | Echoed from the request, or the backend-generated value (see [`clientOrderId` generation](#clientorderid-generation)). |
| `transactTime` | `now_pair()` captured once at the start of the handler. |
| `status` | Always `"PENDING_NEW"` — the order has been accepted by `PrivateNote.placeOrder` (chain return of `bee_dex::Dex::place_order` succeeded) but is **not yet on the book**; `OrderBook.executeBatch` is processing the internal message and will emit `OrderPlaced` with the chain-assigned `orderId` shortly after. |

Why minimal: every other field a fully-populated order would carry (`marketAddress`, `symbol`, `side`, `type`, `timeInForce`, `price`, `origQty`) is **already in the request the client just sent** — echoing them adds bytes without adding information. Two specific fields the Binance-style shape carries (`orderId`, `executedQty`) cannot be filled honestly under optimistic submission: `orderId` is assigned by `OrderBook` after our return, and `executedQty` is always zero for a freshly-placed order. Surfacing them as `""` / `"0"` is worse than not surfacing them — it implies the order is further along the lifecycle than it actually is.

The client correlates the response with future `live_orders` rows by polling `GET /api/v1/orders` and matching by `clientOrderId` in the returned `orders[]`. The `PENDING_NEW` status flips to `NEW` once the indexer projects `OrderPlaced`.

`PENDING_NEW` is listed in [api-spec §Order Status](../api-spec.md#order-status); it's the only status `POST /api/v1/order` returns on success. Strictly additive — code that only switches on `NEW`/`PARTIALLY_FILLED`/`FILLED`/`CANCELED`/`REJECTED` continues to work because those values still arrive through `/api/v1/orders`.


### Failure surface

Three failure classes — two synchronous, one async:

1. **Pre-submit, surfaced synchronously** — request shape, market/outcome resolution, local input validation (precision/tick/step/notional). Mapped per [Error mapping](#error-mapping).

2. **PrivateNote chain-side, surfaced synchronously** — `bee_dex::Dex::place_order` awaits the chain's execution of `PrivateNote.placeOrder`, so any `require(...)` failure inside that ABI call comes back as a typed `AppError` carrying the TVM `exit_code`. `map_bee_dex_error` (in `crates/infrastructure/src/chain_sender.rs`) translates the known codes from `contracts/modifiers/errors.sol`:

   | chain `exit_code` | source | `DomainError` |
   | --- | --- | --- |
   | `102` `ERR_LOW_VALUE` | insufficient `_balance[tokenType]` (BUY) or `stake.amount[outcomeId]` (SELL) | `OrderValidationFailed` → 400 / -2010 |
   | `121` `ERR_NOTE_BUSY` | another `placeOrder` from this PN is still in flight (`_busy` not cleared) | `OrderPnBusy` → 429 / -2014 |
   | `130` `ERR_INVALID_OUTCOME_ID` | `outcome_id` from the read-model does not exist on the PMP | `MarketInconsistent` → 503 / -1500 |
   | `142` `ERR_STAKE_NOT_EXISTS` | SELL but no `splitFullSet` has run for this PN on this market | `OrderValidationFailed` → 400 / -2010 |
   | `150` `ERR_DEBT_NON_ZERO` / `151` `ERR_INVALID_STATE` | PN has outstanding debt or has been withdrawn | `OrderValidationFailed` → 400 / -2010 |
   | `160` `ERR_ORDER_TOO_SMALL` | notional below chain `minOrderNotional(tokenType)` | `OrderValidationFailed` → 400 / -2010 |
   | `163` `ERR_AMOUNT_NOT_LOT_MULTIPLE` / `164` `ERR_PRICE_NOT_TICK_MULTIPLE` | amount/price misaligned with chain lattice (implies read-model `step_size` / `tick_size` drift) | `PrecisionExceeded` → 400 / -1111 |
   | any other `tvm_exit` code | unmapped chain code | `Unexpected` → 500 / -1000, logged at `error` level for ops triage |

   The MM client therefore knows immediately why a given `POST` failed for the common cases and does not have to detect rejection through polling absence.

3. **OrderBook chain-side, surfaced asynchronously** — once `PrivateNote.placeOrder` accepts, it sends an internal message to `OrderBook.executeBatch`. That executes in a separate transaction the synchronous return cannot observe. If `OrderBook` then rejects (`OrderBook.Rejected` for coid collision against a still-live coid, queue overflow, or ABI-level validation), the indexer records the raw event but does not (today) insert a row into `live_orders`. From the HTTP caller's standpoint the `POST` returned `200 NEW`, but the order never surfaces in `/api/v1/orders` until the REJECTED follow-up ships ([read-api.md §REJECTED — future work](read-api.md#rejected--future-work)). Clients detect this class by absence: a `clientOrderId` that does not appear within a few seconds was OrderBook-rejected. This residual asynchronicity is the only case left where MM bots must implement absence-detection — typical rejections (balance, busy, validation) now surface synchronously through class 2.

Transport-level failures (gateway connection drop, malformed reply, decode error) sit outside this classification and always collapse to `Unexpected` → 500 / -1000 with the raw `AppError` logged at `error` level. Accepted orders that later get filled or cancelled by normal market activity are not failures and are surfaced through `/api/v1/orders` per [read-api.md](read-api.md).

**`-2014 OrderPnBusy` is transitional.** The current account model has exactly one trading PN per account ([auth.md §Trading Private Note](auth.md#trading-private-note)). When multi-PN trading lands (one account routing orders across several PNs in parallel), `_busy` ceases to be a per-account bottleneck and a client hitting `ERR_NOTE_BUSY` would mean an internal PN-selection bug — at that point this row collapses back into `OrderValidationFailed` / 400 and the `-2014` code is removed from the public surface. SDK authors should treat `-2014` the same as `-2010` plus a short retry hint; do not bake persistent retry logic keyed on this specific code.


### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Auth envelope / unknown api_key / bad signature / timestamp | handled upstream by [auth_hoop](auth.md#authentication) | 401 |
| Body exceeds the auth-hoop body cap | `RequestTooLarge` | 413 |
| Caller lacks `TRADE` permission | `AuthRequired` | 401 |
| Mandatory body field missing | `MissingParameter` | 400 |
| Unknown enum value or unsupported `type` × `timeInForce` combination | `InvalidParameter` | 400 |
| Market unknown or pre-reconcile | `InvalidMarketOrSymbol` | 404 |
| Reconciled market with NULL/blank `orderbook_address` or NULL `oracle_list_hash`, or chain `ERR_INVALID_OUTCOME_ID` (outcome drift) | `MarketInconsistent` | 503 |
| Market `status != TRADING`; local notional below `minNotional`; chain `ERR_LOW_VALUE` / `ERR_STAKE_NOT_EXISTS` / `ERR_DEBT_NON_ZERO` / `ERR_INVALID_STATE` / `ERR_ORDER_TOO_SMALL` | `OrderValidationFailed` | 400 |
| Local precision / tick / step violation; chain `ERR_AMOUNT_NOT_LOT_MULTIPLE` / `ERR_PRICE_NOT_TICK_MULTIPLE` | `PrecisionExceeded` | 400 |
| Chain `ERR_NOTE_BUSY` (per-PN serial enforced on-chain; another `placeOrder` still in flight) | `OrderPnBusy` | 429 |
| Handler exceeded `ServerSection.request_timeout_ms` (`config/api.<env>.yaml`) — enforced by `services/api/src/timeout_hoop.rs` | `RequestTimeout` | 504 |
| Unmapped chain `tvm_exit` code or gateway transport failure | `Unexpected` | 500 |

`RequestTimeout` (`-1007`) is enforced at two layers: the HTTP request_timeout hoop (`services/api/src/timeout_hoop.rs`) for handler-wide budgets, and the chain sender (`crates/infrastructure/src/chain_sender.rs::classify_chain_outcome`) for gateway-side hangs. `ApiConfig::validate` pins `server.request_timeout_ms > chain.place_order_timeout_ms` at boot so the HTTP timeout cannot fire while a chain submission is still in flight.

### Layering

| Layer | Responsibility |
| --- | --- |
| `crates/domain` | `OrderSide`, `OrderType`, `TimeInForce`, `OrderStatus`, the `flags` encoder, decimal-validation primitives (`parse_positive_decimal`, `lift_decimal`, `is_multiple_of`, `notional_meets_minimum`, `normalize_decimal`). Pure logic; no I/O. |
| `crates/application` | `NewOrderInput` (HTTP-shaped), `NewOrderPayload` (chain-shaped), `SubmittedOrder` (response-shaped); `ChainOrderSender` trait; `CreateOrderUseCase`. `MarketReadRepository` reused from the read side. |
| `crates/infrastructure` | `BeeDexChainSender` — thin wrapper around `bee_dex::Dex::place_order`; converts pubkey decimal → hex and seckey bytes → hex at the boundary. `PostgresReadModelRepository` reused for market lookups. |
| `services/api` | Handler wraps the use case; HMAC enforced by `auth_hoop`; permission enforced by `require_auth(Permission::Trade)`. `run()` constructs `BeeDexChainSender` from `ApiConfig.chain.gateway_endpoint` and `chain.place_order_timeout_ms`. |

The use case constructor takes trait objects, never concrete types, so the test-kit can inject fakes — see `services/api/tests/create_order_http.rs` (`FakeRepo` / `FakeAuthenticator` / `RecordingSender` triad against the full router) and `services/api/tests/common/mod.rs` (`NoopChainSender`) for the patterns. `services/api/tests/auth_http.rs` is the other direction — it drives the real `PostgresAuthenticator` through `common::setup()` to test the HMAC pipeline end-to-end.

### Idempotency and retries

The backend does not store inflight submissions and does not retry on its own. Clients that need at-least-once delivery supply a fixed `newOrderClientId` and re-`POST` on transient errors: the chain rejects the second submission with the same coid (silent `Rejected`), and `/api/v1/orders` keyed on `clientOrderId` surfaces the eventually-confirmed state of the first one.

### Concurrency

Placement against one trading PN is serial at the chain ([PN busy window](#glossary)). The API does not coordinate concurrent submissions across replicas — two `POST /api/v1/order` requests from the same account that land on different API instances are sent to the chain in whatever order the gateway receives them. The losing submission is rejected on-chain with `ERR_NOTE_BUSY`; `BeeDexChainSender` maps that synchronously to `OrderPnBusy` → 429 / -2014 (see [Failure surface](#failure-surface) §2), so the client receives an actionable retry signal on the HTTP response rather than having to detect absence in `/api/v1/orders`.

Clients that need higher per-account throughput batch multiple orders into one chain message via `POST /api/v1/batchOrders` — one `placeBatch` call covers many orders under a single `_busy` lock.

## `DELETE /api/v1/order`

The handler runs three phases: request parsing → order resolution (which folds market lookup, status derivation, and ownership into one SELECT) → chain submission. Each phase fails closed with its own error code (see [DELETE error mapping](#error-mapping-1)). The on-chain cancel itself is optimistic in the same sense as POST: `PrivateNote.cancelOrder` returns once PN has forwarded the cancel to `OrderBook.executeBatch` as an internal message — the actual removal from the book and the projection into [`live_orders`](data-schema.md#live_orders) happen asynchronously through the indexer.

### Authorization

Same hoop as POST. The handler calls `require_auth(depot, Permission::Trade)` and reads the resolved [`TradingPn`](auth.md#trading-private-note) (`pn_address`, `pn_pubkey`, `pn_dih`, decrypted `pn_seckey`) out of `AuthContext`.

### Request parsing

DELETE has no body; all named parameters arrive in the query string (the HMAC layer has already verified the canonical query string for those exact bytes).

| Field | Type | Notes |
| --- | --- | --- |
| `marketAddress` | `MarketAddress` | Mandatory. |
| `symbol` | `Symbol` | Mandatory. |
| `orderId` | `String` | Mandatory; parsed as `u64::from_str` in the use case. The on-chain ABI is `uint128`, but the `bee_dex` → `ackinacki-kit` → `serde_json::json!` path rejects values above `u64::MAX` (same `arbitrary_precision` constraint documented in [§clientOrderId generation](#clientorderid-generation)). Out-of-range or non-numeric → `InvalidParameter` → 400 / -1130. |

Mandatory-field absence returns `MissingParameter` → 400.

### Order resolution

One SELECT joins [`live_orders`](data-schema.md#live_orders) ⨝ [`markets`](data-schema.md#markets) ⨝ [`market_outcomes`](data-schema.md#market_outcomes) and applies five predicates at once:

- `markets.last_reconciled_at IS NOT NULL` — same visibility gate as POST.
- `markets.pmp_address = :marketAddress` AND `market_outcomes.symbol = :symbol`. The `pmp_address` column is the SQL spelling of the public `marketAddress` field; the alias dates from the contract-level naming.
- `live_orders.order_id = :orderId` AND `live_orders.status = 'OPEN'`.
- `live_orders.owner_pn_address = :pn_address` — pins the caller as the owner of the row.
- `live_orders.amount_remaining > 0` — same belt-and-suspenders predicate the `/api/v1/orders` read path applies to OPEN rows. A row could in principle linger as `status = 'OPEN'` with `amount_remaining = 0` in the brief window before `apply_order_filled` flips it to `FILLED`; the gate keeps that transient slice invisible to cancel.

A miss surfaces as `UnknownOrder` → 404 / -2011 with **no distinction** between "order does not exist", "order exists but belongs to another account", "order is not OPEN anymore", or "marketAddress/symbol does not match the order's actual market". This is deliberate: differentiating those cases would leak the existence (and account binding) of orders the caller does not own.

The same row supplies every value the chain submission and the response need:

| Source column | Bound to |
| --- | --- |
| `markets.event_id` | `cancelOrder.eventId` (uint256). |
| `markets.oracle_list_hash` | `cancelOrder.oracleListHash` (uint256). NULL on a reconciled row → `MarketInconsistent` → 503. |
| `markets.token_type` | `cancelOrder.tokenType` (uint32). The column is `integer` in Postgres (signed); the use case applies `u32::try_from` and a negative value (read-model corruption) surfaces as `MarketInconsistent` → 503. Same guard as POST. |
| `markets.orderbook_address` | Joined against `live_orders.orderbook_address` to scope ownership to one book. |
| `live_orders.client_order_id` | Echoed back as `clientOrderId` in the response. |

Status derivation reuses the SQL from [read-api.md §Status derivation](read-api.md#status-derivation) over the same `markets` row and the request `now`. Cancellation is permitted only when `status == TRADING`; any other phase rejects with `OrderValidationFailed` → 400 / -2010. The chain itself does not gate cancels by market status, but the read-model gate keeps the public surface symmetric with POST and prevents user-driven cancels against a draining book once the market has left TRADING.

### Chain submission

Encode and dispatch a `PrivateNote.cancelOrder` external message against `trading_pn.pn_address`. ABI from `contracts/PrivateNote.sol::cancelOrder`, exposed by `ackinacki-kit/contracts/src/dex/private_note.rs::ParamsOfCancelOrder`:

```text
cancelOrder(
  eventId,         // uint256, markets.event_id
  oracleListHash,  // uint256, markets.oracle_list_hash
  tokenType,       // uint32,  markets.token_type
  orderId,         // uint128 ABI; capped at u64 today (see Request parsing)
)
```

`PrivateNote.cancelOrder` performs no ownership or existence check on `orderId` — only the per-PN busy guard (`require(!_busy.hasValue(), ERR_NOTE_BUSY)`) and the internal forward `OrderBook.executeBatch(noOrders, [orderId])`. The OrderBook side runs in a separate transaction the synchronous return cannot observe; any reject there (queue overflow, owner mismatch, order already gone) is invisible to the HTTP caller. See [DELETE failure surface](#failure-surface-1).

Sender boundary: extend the existing `ChainOrderSender` trait in `crates/application/src/lib.rs` with `async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError>`. The production `BeeDexChainSender` impl wraps `bee_dex::Dex::cancel_order` and reuses `classify_chain_outcome` and `map_tvm_exit_code` from `crates/infrastructure/src/chain_sender.rs` — the only TVM exit code the cancel path produces is `121 ERR_NOTE_BUSY`, already mapped.

### Response

A successful submission returns the four-field body from [api-spec §Cancel Order](../api-spec.md#cancel-order):

| Field | Source |
| --- | --- |
| `orderId` | Echoed from the request. |
| `clientOrderId` | `live_orders.client_order_id` from the resolved row. Empty string when the column is NULL (order was placed without a `newOrderClientId`). |
| `transactTime` | `now_pair()` captured once at the start of the handler. |
| `status` | Always `"PENDING_CANCEL"` — `PrivateNote.cancelOrder` has accepted the request and forwarded to OrderBook, but the order has **not been removed from the book yet**. `OrderBook` will emit `OrderCancelled` once it dequeues the entry, and the indexer will flip [`live_orders.status`](data-schema.md#live_orders) to `CANCELLED` then. |

The client correlates by `orderId` against `/api/v1/orders` (the stored status flips to `CANCELED`, or to `FILLED` if matching raced the cancel).

`PENDING_CANCEL` is listed in [api-spec §Order Status](../api-spec.md#order-status); it is the only status `DELETE /api/v1/order` returns on success. Strictly additive — `NEW`/`PARTIALLY_FILLED`/`FILLED`/`CANCELED`/`REJECTED` still arrive through `/api/v1/orders` and existing client switches keep working.

### Failure surface

Three failure classes — two synchronous, one async — same shape as POST.

1. **Pre-submit, surfaced synchronously** — request shape, order resolution, status derivation. Mapped per [DELETE error mapping](#error-mapping-1).

2. **PrivateNote chain-side, surfaced synchronously** — `bee_dex::Dex::cancel_order` awaits the chain's execution of `PrivateNote.cancelOrder`. The only PN-side `require(...)` is the busy guard:

   | chain `exit_code` | source | `DomainError` |
   | --- | --- | --- |
   | `121` `ERR_NOTE_BUSY` | another op from this PN is still in flight (`_busy` not cleared) | `OrderPnBusy` → 429 / -2014 |
   | any other `tvm_exit` code | unmapped chain code | `Unexpected` → 500 / -1000, logged at `error` for ops triage |

3. **OrderBook chain-side, surfaced asynchronously** — `OrderBook.executeBatch` processes the cancel from its internal queue in a later transaction. Three outcomes are silent at HTTP-response time:
   - **Race with fill or earlier cancel** — the order is no longer on the book when the cancel dequeues; `_doCancel` returns without emitting `OrderCancelled`. `live_orders` may already be `FILLED` or `CANCELLED` from another path.
   - **Owner mismatch** — `_doCancel` silently no-ops if `o.depositHash` does not match the caller's. The pre-submit ownership lookup makes this case unreachable under normal operation; it remains possible only under read-model corruption.
   - **Queue overflow** — `OrderBook.Rejected` fires; the indexer records the raw event but does not touch `live_orders`. The order stays `OPEN`.

   An HTTP 200 `PENDING_CANCEL` is therefore not a guarantee that the cancel will land — it confirms only that `PrivateNote.cancelOrder` accepted the request. `PENDING_CANCEL` is the DELETE response token only; it is never persisted to `live_orders.status`, so polling `/api/v1/orders` continues to report `NEW` / `PARTIALLY_FILLED` until the indexer applies `OrderCancelled` or `OrderFilled` and flips the stored status to `CANCELED` or `FILLED`. Clients detect class-3 outcomes by watching for that flip on the `orderId` within a reasonable window.

Transport-level failures (gateway drop, decode error) collapse to `Unexpected` → 500 / -1000 with the raw `AppError` logged at `error`, same as POST.

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Auth envelope / unknown api_key / bad signature / timestamp | handled upstream by [auth_hoop](auth.md#authentication) | 401 |
| Caller lacks `TRADE` permission | `AuthRequired` | 401 |
| Mandatory query field missing | `MissingParameter` | 400 |
| `orderId` not numeric or overflows u64 | `InvalidParameter` | 400 |
| Reconciled market with NULL `oracle_list_hash` | `MarketInconsistent` | 503 |
| `(marketAddress, symbol, orderId)` does not resolve to an OPEN order owned by the caller (covers unknown order, wrong owner, wrong market, already closed) | `UnknownOrder` | 404 |
| Resolved market `status != TRADING` | `OrderValidationFailed` | 400 |
| Chain `ERR_NOTE_BUSY` (per-PN serial; another op still in flight) | `OrderPnBusy` | 429 |
| Handler exceeded `ServerSection.request_timeout_ms` | `RequestTimeout` | 504 |
| Unmapped chain `tvm_exit` code or gateway transport failure | `Unexpected` | 500 |

The same `request_timeout_ms > chain.cancel_order_timeout_ms` invariant POST relies on extends to cancel — the chain config gets a `cancel_order_timeout_ms` alongside `place_order_timeout_ms`, pinned at boot by `ApiConfig::validate` so the HTTP timeout cannot fire while a chain submission is still in flight.

### Layering

| Layer | Responsibility |
| --- | --- |
| `crates/domain` | Adds `OrderStatus::PendingCancel` (rendered as `"PENDING_CANCEL"` via `as_str`). No other changes — `OrderSide`, error variants, decimal helpers are unchanged. |
| `crates/application` | `CancelOrderInput` (HTTP-shaped), `CancelOrderPayload` (chain-shaped), `CancelledOrder` (response-shaped); `CancelOrderUseCase`. Extends `ChainOrderSender` with `cancel_order`. Extends `MarketReadRepository` with `resolve_for_cancel(market_address, symbol, order_id, owner_pn_address, now)` — the one-shot join described in [Order resolution](#order-resolution). |
| `crates/infrastructure` | `BeeDexChainSender::cancel_order` wraps `bee_dex::Dex::cancel_order` (pubkey/seckey re-encode reused). `PostgresReadModelRepository::resolve_for_cancel` runs the join. A miss in the predicate set surfaces as `UnknownOrder`; a NULL/blank `oracle_list_hash` on a reconciled row is logged as a `warn` and surfaced as an empty string, which the use case then translates to `MarketInconsistent`. The same split lives on the POST side — keeping it symmetric means a future tightening (e.g. moving the translation into the SELECT) touches both paths together. |
| `services/api` | `delete_order` handler attached as `Router::with_path("api/v1/order").delete(delete_order)` on the existing auth subrouter (alongside `.post(create_order)`). HMAC enforced by `auth_hoop`; permission by `require_auth(Permission::Trade)`. `run()` reuses the `BeeDexChainSender` instance already constructed for POST. |

Use case constructors take trait objects; `services/api/tests/cancel_order_http.rs` injects `FakeRepo` + `FakeAuthenticator` + a `RecordingSender` variant that records cancel payloads, matching the triad established by `create_order_http.rs`.

### Idempotency and retries

The backend stores no inflight cancel state and does not retry on its own. Duplicate DELETEs on the same `orderId` are safe at the chain level:

- If the first cancel is still in flight at PN, the second hits `ERR_NOTE_BUSY` → 429 (retry).
- If the first cancel already cleared PN but the indexer has not flipped `live_orders` yet, the second passes pre-submit, PN forwards a second `executeBatch`, OrderBook's `_doCancel` silently no-ops (the order is gone), and the indexer state remains consistent.
- Once `live_orders.status` is `CANCELLED`, the pre-submit lookup returns `UnknownOrder` and further DELETEs surface as 404 / -2011. Clients should treat `-2011` as terminal and not retry.

### Concurrency

Cancellation contends for the same per-PN `_busy` lock as placement — see [§Concurrency for POST /order](#concurrency). A DELETE that races a POST (or another DELETE) from the same account surfaces as `OrderPnBusy` → 429 / -2014 with the same retry semantics.

## `POST /api/v1/batchOrders`

The handler runs three phases: request parsing → market/outcome resolution and per-item input validation → chain submission. Layout mirrors `POST /api/v1/order`: the per-item validation chain is the same and lives in one shared helper. Each phase fails closed with its own error code (see [Batch error mapping](#error-mapping-2)).

### Authorization

Same hoop as `POST /api/v1/order`. The handler calls `require_auth(depot, Permission::Trade)` and reads the resolved [`TradingPn`](auth.md#trading-private-note) out of `AuthContext`. All items in the batch are signed by the same trading-PN keypair — the chain ABI accepts only one external message and the busy lock is per-PN regardless of batch size.

### Request parsing

Body fields are taken byte-exact from the request as transmitted; the HMAC layer has already verified the signature over those exact bytes. Mandatory-field absence (top-level or per-item) returns `MissingParameter` → 400.

Top-level body fields:

| Field | Type | Notes |
| --- | --- | --- |
| `marketAddress` | `MarketAddress` | Mandatory. |
| `symbol` | `Symbol` | Mandatory. |
| `orders` | `Vec<BatchOrderInputItem>` | Mandatory and non-empty. Maximum length equals the configured `chain.max_batch_size` (advertised as `maxBatchSize` in `/api/v1/markets`). |

Each `BatchOrderInputItem` is shaped the same as the body of `POST /api/v1/order` minus `marketAddress` / `symbol` (the chain ABI takes those once per batch, not per item). An unknown enum value (`side`, `type`, `timeInForce`) on any item returns `InvalidParameter` → 400.

### Market and outcome resolution

`(marketAddress, symbol)` is resolved once via the same `resolve_for_new_order` join `POST /api/v1/order` uses, including the visibility gate (`m.last_reconciled_at IS NOT NULL`) and the [Status derivation](read-api.md#status-derivation) clause. A miss surfaces as `InvalidMarketOrSymbol` → 404. Placement is permitted only when `status == TRADING`; any other phase rejects with `OrderValidationFailed` → 400. A reconciled market with NULL/blank `oracle_list_hash` surfaces as `MarketInconsistent` → 503 — same fail-closed invariant as POST.

The resolved row supplies the chain-level fields once for the whole batch:

| Source column | Bound to |
| --- | --- |
| `markets.event_id` | `placeBatch.eventId` (uint256). |
| `markets.oracle_list_hash` | `placeBatch.oracleListHash` (uint256). |
| `markets.token_type` | `placeBatch.tokenType` (uint32). |
| `market_outcomes.outcome_id` | Each `OrderBookOrder.outcomeId` in the batch. |
| `market_outcomes.price_precision` / `tick_size` | Per-item price scaling and tick-size validation. |
| `market_outcomes.quantity_precision` / `step_size` | Per-item quantity scaling and step-size validation. |
| `market_outcomes.min_notional` | Per-item notional validation. |

### Batch size cap

The cap is the api config knob `chain.max_batch_size` — the same value `/api/v1/markets` advertises as `maxBatchSize`, so the promise and the enforcement share one source. It manually mirrors the chain's compiled-in per-side `MAX_BATCH_SIZE` (`contracts/modifiers/modifiers.sol`, 10 today; the chain exposes no getter for it) and must not exceed it. An empty `orders[]` or one whose length exceeds the cap surfaces as `InvalidParameter` → 400 / -1130 before the chain submission.

### Per-item input validation

Per-item validation is identical to `POST /api/v1/order` — same precision / tick / step / notional / coid rules from [api-spec §Validation Rules](../api-spec.md#validation-rules). The shared helper `validate_and_encode_order_item` in `crates/application/src/lib.rs` runs the chain for both endpoints, so a future tightening (e.g. tighter step-size handling) touches one place. The helper also encodes the chain-shaped fields (`outcome_id`, `is_buy`, `price_raw`, `amount_raw`, `flags`, `client_order_id`) the dispatch needs.

The loop short-circuits on the first item-level failure: the entire request rejects with the failing item's error code; no chain message is sent. This matches the chain's atomic `placeBatch` semantics — partial submission is not a possible outcome — and avoids spending the per-PN busy window on a doomed batch. The response carries one error object regardless of how many items would have failed; the client correlates by re-reading its own request payload.

### Flags

Same encoding table as `POST /api/v1/order` — see [Flags](#flags). The chain's `OrderBookOrder.flags` field is per-item, so a single batch can mix LIMIT and MARKET orders freely as long as each item's combination is valid.

### `clientOrderId` generation

Identical to the single-order path; see [§clientOrderId generation](#clientorderid-generation). One subtlety: the chain enforces uniqueness across the PN's still-live coids **and within the batch itself** — `PrivateNote.placeBatch` walks each item's `clientOrderId` and rejects intra-batch duplicates with `ERR_INVALID_PARAMS` (129). The backend does no intra-batch dedupe — caller-supplied duplicates parse fine as u64 and reach the chain unfiltered, where the whole batch reverts as `InvalidParameter` → 400 / -1130 via that exit code. Backend-generated coids are drawn independently for each item; the 2^62-bit collision space documented for the single-order path means intra-batch collisions are cosmologically negligible.

### Chain submission

Encode and dispatch a `PrivateNote.placeBatch` external message against `trading_pn.pn_address`. ABI from `contracts/PrivateNote.sol::placeBatch`, exposed by `ackinacki-kit/contracts/src/dex/private_note.rs::ParamsOfPlaceBatch`:

```text
placeBatch(
  eventId,         // uint256, markets.event_id
  oracleListHash,  // uint256, markets.oracle_list_hash
  tokenType,       // uint32,  markets.token_type
  orders,          // OrderBookOrder[]; each item carries
                   //   outcomeId, isBuy, flags, price, amount,
                   //   minAmount, epochId, clientOrderId
  cancelIds,       // uint128[]; this endpoint always sends []
)
```

Per-item `minAmount` and `epochId` stay at 0 — same constants as `placeOrder` (neither is exposed by api-spec and neither has a per-order meaning in this version of the public API). `cancelIds` is the cancellation side of the chain's atomic batch — `placeBatch` is the single batch entry point and processes both sides in one `OrderBook.executeBatch` dispatch. `POST /api/v1/batchOrders` is placement-only and always sends `cancelIds = []`; the populated side belongs to `DELETE /api/v1/batchOrders`.

Sender boundary: the existing `ChainOrderSender` trait grows a third method `async fn submit_batch_order(&self, payload: NewBatchOrderPayload) -> Result<(), DomainError>`. The production `BeeDexChainSender` impl wraps `bee_dex::Dex::place_batch`, reuses `build_signer` for pubkey/seckey re-encoding and `classify_chain_outcome` for the timeout / exit-code translation. A dedicated `chain.place_batch_timeout_ms` config knob bounds the per-call wait; `ApiConfig::validate` pins `server.request_timeout_ms > chain.place_batch_timeout_ms` at boot so the HTTP timeout cannot fire while a batch submission is still in flight.

`bee_dex::Dex::place_batch` **waits for the chain to execute `PrivateNote.placeBatch` on the trading PN** and returns the TVM exit code on `require(...)` failure. `placeBatch` is atomic in WASM: every item is re-validated and any failed `require(...)` reverts the whole batch — none of the items land. The `OrderBook` side runs as an internal message after `placeBatch` returns; the `_pendingBatchActive` flag on the PN stays set until `onBatchComplete` arrives back from `OrderBook`, so the busy window is longer for a batch than for a single placement and a fast follow-up `placeOrder` / `placeBatch` from the same PN is more likely to hit `ERR_NOTE_BUSY` (121). Clients should rely on the same `OrderPnBusy` → 429 retry contract as the single-order path.

### Response

One `PENDING_NEW` envelope per accepted item, returned as a flat array in request order (see [api-spec §New Batch Orders](../api-spec.md#new-batch-orders)). The shape is symmetric with `POST /api/v1/order`:

| Field | Source |
| --- | --- |
| `clientOrderId` | Per-item: caller-supplied `newOrderClientId`, or the backend-generated value. |
| `transactTime` | `now_pair()` captured once at the start of the handler, repeated for every item — one chain submission, one moment of acceptance. |
| `status` | Always `"PENDING_NEW"` — same rationale as the single-order path; the chain-assigned `orderId` arrives later through `/api/v1/openOrders`. |

Why minimal: the same argument as POST /order applies item by item — every other field a fully-populated order would carry is already in the request the client just sent, and the only fields the Binance-style shape adds (`orderId`, `executedQty`) cannot be filled honestly under optimistic submission.

### Failure surface

Same three-class split as `POST /api/v1/order`:

1. **Pre-submit, surfaced synchronously** — request shape (top-level and per-item), market/outcome resolution, batch size cap, per-item validation. First failure rejects the whole request; mapped per [Batch error mapping](#error-mapping-2).

2. **PrivateNote chain-side, surfaced synchronously** — `bee_dex::Dex::place_batch` awaits PN's execution. The single-order exit codes (102 / 121 / 130 / 142 / 150 / 151 / 160 / 163 / 164) carry over verbatim; the batch path adds four:

   | chain `exit_code` | source | `DomainError` |
   | --- | --- | --- |
   | `129` `ERR_INVALID_PARAMS` | `clientOrderId` collision against the PN's `_clientOrderIds` map — covers both intra-batch duplicates AND any still-live coid from an earlier `placeOrder` on the same PN. The chain also raises 129 for `minAmount != 0` on any MARKET order, but our wire payload always sends `minAmount = 0`. | `InvalidParameter` → 400 / -1130 |
   | `161` `ERR_BATCH_TOO_LARGE` / `162` `ERR_EMPTY_BATCH` | chain-side defence-in-depth — reaching either code means the configured `chain.max_batch_size` drifted above the on-chain ceiling, not a client bug | `MarketInconsistent` → 503 / -1500 |
   | `168` `ERR_NOTIONAL_OVERFLOW` | `price * amount` overflowed uint256 inside `placeBatch` (only the chain checks for this; the read-model has no equivalent ceiling) | `OrderValidationFailed` → 400 / -2010 |

3. **OrderBook chain-side, surfaced asynchronously** — same shape as the single-order path: the internal `executeBatch` message runs in a later transaction and `OrderBook.Rejected` events (e.g. queue overflow) are visible only through the indexer.

Transport-level failures collapse to `Unexpected` → 500 / -1000, same as the single-order path.

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Auth envelope / unknown api_key / bad signature / timestamp | handled upstream by [auth_hoop](auth.md#authentication) | 401 |
| Body exceeds the auth-hoop body cap | `RequestTooLarge` | 413 |
| Caller lacks `TRADE` permission | `AuthRequired` | 401 |
| Mandatory body field missing (top-level or per-item) | `MissingParameter` | 400 |
| Unknown enum value, unsupported `type` × `timeInForce` combination, empty `orders[]`, length above `max_batch_size`, non-numeric or over-u64 `newOrderClientId`; chain `ERR_INVALID_PARAMS` (intra-batch coid collision) | `InvalidParameter` | 400 |
| Market unknown or pre-reconcile | `InvalidMarketOrSymbol` | 404 |
| Reconciled market with NULL `oracle_list_hash`, chain `ERR_INVALID_OUTCOME_ID`, or chain `ERR_BATCH_TOO_LARGE` / `ERR_EMPTY_BATCH` (read-model drift from on-chain ceiling) | `MarketInconsistent` | 503 |
| Market `status != TRADING`; per-item notional below `minNotional`; chain `ERR_LOW_VALUE` / `ERR_STAKE_NOT_EXISTS` / `ERR_DEBT_NON_ZERO` / `ERR_INVALID_STATE` / `ERR_ORDER_TOO_SMALL` / `ERR_NOTIONAL_OVERFLOW` | `OrderValidationFailed` | 400 |
| Per-item precision / tick / step violation; chain `ERR_AMOUNT_NOT_LOT_MULTIPLE` / `ERR_PRICE_NOT_TICK_MULTIPLE` | `PrecisionExceeded` | 400 |
| Chain `ERR_NOTE_BUSY` (per-PN serial; another op still in flight on this PN) | `OrderPnBusy` | 429 |
| Handler exceeded `ServerSection.request_timeout_ms` | `RequestTimeout` | 504 |
| Unmapped chain `tvm_exit` code or gateway transport failure | `Unexpected` | 500 |

### Layering

| Layer | Responsibility |
| --- | --- |
| `crates/domain` | No new types — `OrderStatus::PendingNew`, `encode_order_flags`, and the decimal helpers are all reused. |
| `crates/application` | `BatchOrderInputItem` (HTTP-shaped), `BatchOrderPayloadItem` (chain-shaped, one per item), `NewBatchOrderPayload` (chain-shaped, one per batch), `SubmittedBatchOrders`; `CreateBatchOrdersUseCase`. Extends `ChainOrderSender` with `submit_batch_order`. The shared `validate_and_encode_order_item` helper lives here. |
| `crates/infrastructure` | `BeeDexChainSender::submit_batch_order` wraps `bee_dex::Dex::place_batch` and packs each `BatchOrderPayloadItem` into an `OrderBookOrder`. `map_tvm_exit_code` learns the four batch-specific codes (129/161/162/168). `ChainSection` grows `place_batch_timeout_ms` with the same validation invariant as the existing timeouts. |
| `services/api` | `create_batch_orders` handler attached as `Router::with_path("api/v1/batchOrders").post(create_batch_orders)` on the existing auth subrouter. HMAC enforced by `auth_hoop`; permission by `require_auth(Permission::Trade)`. `run()` extends the `BeeDexChainSender` constructor with `Duration::from_millis(config.chain.place_batch_timeout_ms)`. |

Use case constructors take trait objects; `services/api/tests/create_batch_orders_http.rs` injects `FakeRepo` + `FakeAuthenticator` + a `RecordingBatchSender` variant that records batch payloads, matching the triad established by `create_order_http.rs` and `cancel_order_http.rs`.

### Idempotency and retries

The backend stores no inflight batch state and does not retry on its own. Clients that need at-least-once delivery supply explicit `newOrderClientId` values for each item and re-`POST` on transient errors: the chain rejects any item whose coid is still live as `ERR_INVALID_PARAMS`, reverting the whole batch; once the original batch is no longer in flight, `/api/v1/openOrders` keyed on `clientOrderId` surfaces the eventually-confirmed state.

### Concurrency

Same per-PN serial constraint as the single-order path — `placeBatch` takes the same `_busy` lock and holds it until `onBatchComplete` arrives. A POST `/batchOrders` racing any other placement or cancellation from the same account surfaces as `OrderPnBusy` → 429 / -2014, with the same retry semantics. Submitting an N-item batch instead of N sequential POSTs is the canonical way to get higher per-account placement throughput — one chain message, one `_busy` lock, one `onBatchComplete` callback.

## `DELETE /api/v1/batchOrders`

The handler runs three phases: request parsing → market/outcome resolution and bulk order resolution → chain submission. Layout mirrors `DELETE /api/v1/order` lifted from one order to N, with the same batch-level resolution gate `POST /api/v1/batchOrders` uses. Each phase fails closed with its own error code (see [Batch cancel error mapping](#error-mapping-3)). The on-chain cancel is optimistic in the same sense as the single-order DELETE: the chain has no standalone batch-cancel method — `PrivateNote.placeBatch` is the atomic batch entry point, and this endpoint dispatches it with an empty placements side (`orders = []`, `cancelIds` populated), forwarding one `OrderBook.executeBatch` message; per-order removal from the book and the projection into [`live_orders`](data-schema.md#live_orders) happen asynchronously through the indexer.

### Authorization

Same hoop as `POST /api/v1/batchOrders`. The handler calls `require_auth(depot, Permission::Trade)` and reads the resolved [`TradingPn`](auth.md#trading-private-note) out of `AuthContext`. The whole batch is signed by one trading-PN keypair — the chain ABI takes one external message and the busy lock is per-PN regardless of batch size.

### Request parsing

Body fields are taken byte-exact from the request as transmitted; the HMAC layer has already verified the signature over those exact bytes. An empty or malformed body short-circuits at `parse_strict_body` to `InvalidParameter` → 400 / -1130 — the helper emits `reason = empty / malformed / truncated / shape_mismatch` (one per failure mode) for ops triage, and backs `POST /order` and `POST /batchOrders` through the same path. `MissingParameter` → 400 / -1102 covers only fields absent within an otherwise-valid JSON object.

Top-level body fields:

| Field | Type | Notes |
| --- | --- | --- |
| `marketAddress` | `MarketAddress` | Mandatory. |
| `symbol` | `Symbol` | Mandatory. |
| `orderIds` | `Vec<String>` | Mandatory and non-empty. Maximum length equals the configured `chain.max_batch_size`. Each element is parsed as `u64::from_str` in the handler (`build_cancel_batch_orders_input`); out-of-range or non-numeric → `InvalidParameter` → 400 / -1130 — same `arbitrary_precision` constraint documented for the single-order path. A blank or whitespace-only element is treated as an absent slot and surfaces as `MissingParameter` → 400 / -1102, matching the single-order DELETE's handling of a blank `orderId`. |

Intra-batch duplicate `orderId` values are rejected with `InvalidParameter` → 400 / -1130 before the chain submission. Duplicates would produce two `PENDING_CANCEL` receipts for the same id (useless to the caller) and waste one slot in the chain's `MAX_BATCH_SIZE` window; the local check keeps the surface honest and the chain's `_busy` budget intact.

### Market and outcome resolution

`(marketAddress, symbol)` is resolved once via the same `resolve_for_new_order` join the placement paths use, including the visibility gate (`m.last_reconciled_at IS NOT NULL`) and the [Status derivation](read-api.md#status-derivation) clause. A miss surfaces as `InvalidMarketOrSymbol` → 404. Cancellation is permitted only when `status == TRADING`; any other phase rejects with `OrderValidationFailed` → 400 — same gate as the single-order DELETE. A reconciled market with NULL/blank `oracle_list_hash` surfaces as `MarketInconsistent` → 503.

The resolved row supplies the chain-level fields once for the whole batch:

| Source column | Bound to |
| --- | --- |
| `markets.event_id` | `placeBatch.eventId` (uint256). |
| `markets.oracle_list_hash` | `placeBatch.oracleListHash` (uint256). |
| `markets.token_type` | `placeBatch.tokenType` (uint32). |

### Batch size cap

Sourced from the api config knob `chain.max_batch_size`, same as `POST /api/v1/batchOrders`. The chain's own `MAX_BATCH_SIZE` (10 today, applied independently to the `orders` and `cancelIds` sides of `placeBatch`) is the ceiling the configured value must not exceed; serving `/api/v1/markets` from the same knob keeps the public surface aligned with the enforcement. An empty `orderIds[]` or one whose length exceeds the cap surfaces as `InvalidParameter` → 400 / -1130 before the chain submission.

### Order resolution

One SELECT joins [`live_orders`](data-schema.md#live_orders) against `markets ⨝ market_outcomes` filtered by the resolved `(pmp_address, symbol)` and applies the predicate set used by the single-order DELETE in bulk:

- Scoping to the resolved outcome's book goes through the `markets ⨝ live_orders` join on `orderbook_address`, mirrored on the single-cancel path.
- `live_orders.owner_pn_address = :pn_address` — pins the caller as the owner of every row.
- `live_orders.order_id` filtered against the input list via `lo.order_id = ANY($3::text[]::numeric[])` — the cast happens on the bind-side array (once at planning), so the indexed `numeric` column is compared without a per-row functional expression and the `(orderbook_address, order_id)` primary key remains usable for the per-id lookup. The SELECT projects `lo.order_id::text` for the application layer to parse back into `u64` and key the resolution `HashMap` on the natural chain identity — no positional `bind_idx` is round-tripped through the wire.
- `live_orders.status = 'OPEN'` AND `live_orders.amount_remaining > 0` — same belt-and-suspenders pair as `/api/v1/openOrders` and single-order DELETE; keeps the transient `OPEN`/`amount_remaining = 0` slice invisible to cancel.

The SELECT also returns each row's `live_orders.client_order_id` (echoed in the response). The use case asserts `resolution.orders.len() < input.order_ids.len()` is false; any shortfall — unknown id, wrong owner, wrong book, already closed — surfaces as `UnknownOrder` → 404 / -2011 for the whole batch, with the same deliberate opacity as single-order DELETE (differentiating those cases would leak order existence and account binding). Validation is atomic: no chain message is sent if any item is unresolved.

`MarketReadRepository::resolve_for_cancel_batch` mirrors `resolve_for_cancel`'s join shape (live_orders ⨝ markets ⨝ market_outcomes) and returns `Option<CancelBatchResolution>`. The resolution carries `event_id`, `oracle_list_hash`, `token_type`, `market_status` once (the SELECT filter `m.pmp_address = $1 AND mo.symbol = $2` pins every matched row to the same `(markets, market_outcomes)` snapshot) and `orders: HashMap<u64, OrderForCancelBatch{client_order_id}>` keyed by the chain `order_id`. The trait contract — every key in `orders` is a member of `input.order_ids[]` — is enforced by the `lo.order_id = ANY($3::text[]::numeric[])` predicate; HashMap natural uniqueness plus the `(orderbook_address, order_id)` primary key guarantees no key collisions. The use case looks each input id up against the map with `orders.remove(&id).ok_or(MarketInconsistent)` — a `None` is a trait-contract violation, never a panic — and assembles the response in `input.order_ids` order. A shortfall (matched count below input length, including the `None` case when zero rows joined) surfaces as `UnknownOrder` for the whole batch.

The bulk SELECT projects the same market-timing columns as `resolve_for_cancel`. Because every matched row shares one `(markets, market_outcomes)` snapshot, the infrastructure layer runs `compute_status` once against the head row and projects the result into `CancelBatchResolution.market_status`. The use case re-checks `resolution.market_status == Trading` after the shortfall gate and rejects with `OrderValidationFailed` otherwise. Single-cancel achieves the same atomicity in one SELECT; for the batch path the placement-shape lookup (`resolve_for_new_order`) and the bulk order resolution run in two independent statements, and without the post-SELECT re-check a reconciler commit between them could let the batch cancel reach the chain on a market that had just left `Trading`.

### Chain submission

Encode and dispatch a `PrivateNote.placeBatch` external message against `trading_pn.pn_address`, with the placements side empty — `placeBatch` is the chain's single atomic batch entry and carries both placements and cancellations. ABI from `contracts/PrivateNote.sol::placeBatch`, exposed by `ackinacki-kit/contracts/src/dex/private_note.rs::ParamsOfPlaceBatch`:

```text
placeBatch(
  eventId,         // uint256, markets.event_id
  oracleListHash,  // uint256, markets.oracle_list_hash
  tokenType,       // uint32,  markets.token_type
  orders,          // OrderBookOrder[]; this endpoint always sends []
  cancelIds,       // uint128[] ABI; each capped at u64 today (see Request parsing)
)
```

`PrivateNote.placeBatch` performs no ownership or existence check on the ids — only the per-PN busy guard (`require(!_busy.hasValue(), ERR_NOTE_BUSY)`) and the chain-side range guards (`ERR_EMPTY_BATCH`, `ERR_BATCH_TOO_LARGE`), then forwards one `OrderBook.executeBatch` message with an empty placements array and the `cancelIds` for cancellation. The PN sets `_pendingBatchActive = true` and holds `_busy` until `onBatchComplete` arrives back from OrderBook — same busy-window shape as the placement batch, longer than single-order cancel. The OrderBook side runs in a separate transaction; per-order `_doCancel` silently no-ops on orders that are no longer on the book or whose owner doesn't match. See [Batch cancel failure surface](#failure-surface-3).

Sender boundary: `ChainOrderSender::cancel_batch_order(&self, payload: CancelBatchOrderPayload) -> Result<(), DomainError>` is the cancel-batch entry on the trait. The production `BeeDexChainSender` impl wraps `bee_dex::Dex::place_batch` with `orders = []`, reuses `build_signer` and `classify_chain_outcome` from `crates/infrastructure/src/chain_sender.rs`. A dedicated `chain.cancel_batch_timeout_ms` config knob bounds the per-call wait; `ApiConfig::validate` pins `server.request_timeout_ms > chain.cancel_batch_timeout_ms` at boot so the HTTP timeout cannot fire while a submission is still in flight.

### Response

One `PENDING_CANCEL` envelope per accepted item, returned as a flat array in request order (see [api-spec §Cancel Batch Orders](../api-spec.md#cancel-batch-orders)). Shape is symmetric with `DELETE /api/v1/order`:

| Field | Source |
| --- | --- |
| `orderId` | Echoed from the request item. |
| `clientOrderId` | Per-item `live_orders.client_order_id` from the resolved row. Empty string when the column is NULL. |
| `transactTime` | `now_pair()` captured once before use-case dispatch, repeated for every item — one chain submission, one moment of acceptance. |
| `status` | Always `"PENDING_CANCEL"` — the cancel-only `PrivateNote.placeBatch` has accepted the request and forwarded to OrderBook, but no order has been removed from the book yet. Each `OrderCancelled` event surfaces individually through the indexer. |

### Failure surface

Three failure classes — two synchronous, one async — same split as `DELETE /api/v1/order` and `POST /api/v1/batchOrders`.

1. **Pre-submit, surfaced synchronously** — request shape, market/outcome resolution, batch size cap, intra-batch duplicate check, bulk order resolution. First failure rejects the whole request; no chain message is sent. Mapped per [Batch cancel error mapping](#error-mapping-3).

2. **PrivateNote chain-side, surfaced synchronously** — the cancel-only `bee_dex::Dex::place_batch` awaits PN's execution. The single-order cancel exit codes carry over; the batch path adds the two range guards already mapped on the placement side:

   | chain `exit_code` | source | `DomainError` |
   | --- | --- | --- |
   | `121` `ERR_NOTE_BUSY` | another op from this PN is still in flight (`_busy` not cleared) | `OrderPnBusy` → 429 / -2014 |
   | `161` `ERR_BATCH_TOO_LARGE` / `162` `ERR_EMPTY_BATCH` | chain-side defence-in-depth — reaching either code means the configured `chain.max_batch_size` drifted above the on-chain ceiling, not a client bug | `MarketInconsistent` → 503 / -1500 |
   | any other `tvm_exit` code | unmapped chain code | `Unexpected` → 500 / -1000, logged at `error` for ops triage |

3. **OrderBook chain-side, surfaced asynchronously** — `OrderBook.executeBatch` processes the cancels from its internal queue in a later transaction. The single-order DELETE's three async outcomes (race with fill/earlier cancel, owner mismatch under read-model corruption, queue overflow) extend to the batch case **per order**: the batch as a whole can have some ids land as `CANCELED`, some as `FILLED` (matching raced the cancel), and — under queue overflow — some remain `OPEN`. The indexer projects each outcome independently into `live_orders` (stored status `CANCELLED` / `FILLED`); clients reconcile by polling `/api/v1/orders`, which surfaces each id with the public-spec status `CANCELED` or `FILLED`.

   An HTTP 200 `PENDING_CANCEL` array is therefore not a guarantee that every cancel will land — it confirms only that `PrivateNote.placeBatch` accepted the request.

Transport-level failures (gateway drop, decode error) collapse to `Unexpected` → 500 / -1000 with the raw `AppError` logged at `error`, same as the placement and single-cancel paths.

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Auth envelope / unknown api_key / bad signature / timestamp | handled upstream by [auth_hoop](auth.md#authentication) | 401 |
| Body exceeds the auth-hoop body cap | `RequestTooLarge` | 413 |
| Caller lacks `TRADE` permission | `AuthRequired` | 401 |
| Mandatory body field missing | `MissingParameter` | 400 |
| Any `orderId` not numeric or overflows u64; empty `orderIds[]`; length above `max_batch_size`; intra-batch duplicate | `InvalidParameter` | 400 |
| Market unknown or pre-reconcile | `InvalidMarketOrSymbol` | 404 |
| Reconciled market with NULL `oracle_list_hash`; negative `markets.token_type` (`u32::try_from` rejects — same guard as single-cancel); chain `ERR_BATCH_TOO_LARGE` / `ERR_EMPTY_BATCH` (read-model drift from on-chain ceiling); bulk SELECT returns duplicate `order_id` (live_orders PK violation — read-model corruption) | `MarketInconsistent` | 503 |
| Any `orderId` does not resolve to an OPEN order owned by the caller on the resolved outcome (covers unknown order, wrong owner, wrong market, already closed) | `UnknownOrder` | 404 |
| Resolved market `status != TRADING` | `OrderValidationFailed` | 400 |
| Chain `ERR_NOTE_BUSY` (per-PN serial; another op still in flight) | `OrderPnBusy` | 429 |
| Handler exceeded `ServerSection.request_timeout_ms` | `RequestTimeout` | 504 |
| Unmapped chain `tvm_exit` code or gateway transport failure | `Unexpected` | 500 |

### Layering

| Layer | Responsibility |
| --- | --- |
| `crates/domain` | No new types — `OrderStatus::PendingCancel` is reused. |
| `crates/application` | `CancelBatchOrdersInput` (HTTP-shaped), `CancelBatchOrderPayload { items: Vec<CancelBatchPayloadItem{order_id, client_order_id}> }` (chain-shaped, one per batch), `CancelledBatchOrder` per-item (`order_id`, `client_order_id`); `CancelBatchOrdersUseCase`. Extends `ChainOrderSender` with `cancel_batch_order`. Extends `MarketReadRepository` with `resolve_for_cancel_batch(market_address, symbol, order_ids, owner_pn_address, now)` returning `Option<CancelBatchResolution{event_id, oracle_list_hash, token_type, market_status, orders: HashMap<u64, OrderForCancelBatch{client_order_id}>}>` — the use case looks each input id up via `orders.remove(&id)`. |
| `crates/infrastructure` | `BeeDexChainSender::cancel_batch_order` wraps `bee_dex::Dex::place_batch` (`orders = []`) and packs `payload.items.iter().map(|i| i.order_id as u128)` into the `cancelIds` `uint128[]` chain ABI. `PostgresReadModelRepository::resolve_for_cancel_batch` runs the bulk SELECT filtered by `lo.order_id = ANY($3::text[]::numeric[])` (cast on the bind side, preserving PK index use) and assembles the result into a `HashMap` keyed on chain `order_id`. `ChainSection` carries `cancel_batch_timeout_ms` with the same validation invariant as the other timeouts. |
| `services/api` | `delete_batch_orders` handler attached as `Router::with_path("api/v1/batchOrders").delete(delete_batch_orders)` on the existing auth subrouter (alongside `.post(create_batch_orders)`). HMAC enforced by `auth_hoop`; permission by `require_auth(Permission::Trade)`. `run()` passes `Duration::from_millis(config.chain.cancel_batch_timeout_ms)` to the `BeeDexChainSender` constructor. |

Use case constructors take trait objects; `services/api/tests/cancel_batch_orders_http.rs` injects `FakeRepo` + `FakeAuthenticator` + a `RecordingCancelBatchSender` variant that records cancel-batch payloads, matching the triad established by `create_batch_orders_http.rs`.

### Idempotency and retries

The backend stores no inflight cancel state and does not retry on its own. Duplicate `DELETE /api/v1/batchOrders` calls on overlapping `orderIds` sets are safe at the chain level by the same argument as single-order DELETE applied per id:

- If the first batch is still in flight at PN, the second hits `ERR_NOTE_BUSY` → 429 (retry).
- If the first batch already cleared PN but the indexer has not flipped `live_orders` yet, the second passes pre-submit, PN forwards a second `executeBatch`, OrderBook's per-id `_doCancel` silently no-ops on ids already gone, and the indexer state remains consistent.
- Once any id's `live_orders.status` is `CANCELLED`, pre-submit resolution returns `UnknownOrder` for the whole batch and further DELETEs surface as 404 / -2011. Clients that need a "cancel-the-rest" semantic should re-issue the call with only the still-open ids.

### Concurrency

Same per-PN serial constraint as the single-order paths and the placement batch — the cancel-only `placeBatch` takes the `_busy` lock and holds it until `onBatchComplete` arrives. A DELETE `/batchOrders` racing any other placement or cancellation from the same account surfaces as `OrderPnBusy` → 429 / -2014. Submitting one batch instead of N sequential DELETEs is the canonical way to cancel multiple orders without per-call back-pressure today — one chain message, one `_busy` lock, one `onBatchComplete` callback.

## `DELETE /api/v1/openOrders`

See [api-spec §Cancel All Open Orders On Symbol](../api-spec.md#cancel-all-open-orders-on-symbol) for the public contract.

_Implementation tech spec to be filled in._

## `POST /api/v1/buyFullSet`

Buys a full set of outcome tokens for one market by depositing `collateral` of the market's quote asset into the PMP. The chain entry point is `PrivateNote.splitFullSet`; on a market sitting in `AWAITING_FREEZE` the first successful call also activates the OrderBook, after which it stays active for all subsequent callers. From the caller's standpoint the request and response are identical to any later call against the same market.

The handler runs three phases: request parsing → market resolution + status gate → collateral validation + chain submission. Each phase fails closed with its own error code (see [Error mapping](#error-mapping-3)).

### Authorization

Same hoop as the order endpoints. The handler calls `require_auth(depot, Permission::Trade)` and reads the resolved [`TradingPn`](auth.md#trading-private-note) (`pn_address`, `pn_pubkey`, `pn_dih`, decrypted `pn_seckey`) out of `AuthContext`.

### Request parsing

| Field | Type | Notes |
| --- | --- | --- |
| `marketAddress` | `MarketAddress` | Mandatory. |
| `collateral` | `String` | Mandatory; decimal. Kept as a string until quote-asset precision validation. |

Mandatory-field absence (the field missing or trimming to the empty string) returns `MissingParameter` → 400.

### Market resolution

Resolve `marketAddress` via [`markets`](data-schema.md#markets), filtered by `m.last_reconciled_at IS NOT NULL` — same visibility gate as [`/api/v1/markets`](read-api.md#visibility-filter). No `market_outcomes` join: `splitFullSet` operates at the market level (collateral → one outcome token of every outcome), so there is no per-outcome resolution to perform. A miss surfaces as `InvalidMarketOrSymbol` → 404 / -1121.

Derive `status` from the row and the request `now` using the logic in [read-api.md §Status derivation](read-api.md#status-derivation). Per [api-spec §Buy Full Set](../api-spec.md#buy-full-set), the call is permitted only when `status ∈ { AWAITING_FREEZE, TRADING }`; every other phase rejects with `OrderValidationFailed` → 400 / -2010.

The same row supplies every value the chain submission requires:

| Source column | Bound to |
| --- | --- |
| `markets.event_id` | `splitFullSet.eventId` (uint256). |
| `markets.oracle_list_hash` | `splitFullSet.oracleListHash` (uint256). Stamped by the market reconciler from `PMP.getDetails().oracleListHash`. NULL/blank on a reconciled row → `MarketInconsistent` → 503. |
| `markets.token_type` | `splitFullSet.tokenType` (uint32). Doubles as the [`ref_tokens`](data-schema.md#ref_tokens) lookup key the use case uses to retrieve the quote asset's on-chain `decimals`. |

### Input validation

`collateral` is the quote-asset amount in human form; the use case lifts it by the quote asset's on-chain `decimals` (looked up via `ReferenceRepository::lookup_ref_token(markets.token_type)`) and submits the lifted raw `uint128` to chain.

| api-spec rule | Failure |
| --- | --- |
| `collateral` digits-only, non-negative | `InvalidParameter` |
| `collateral` decimals ≤ quote-asset `decimals` | `InvalidParameter` |
| `collateral` strictly positive after lift (`> 0`) | `InvalidParameter` |
| Lifted value fits in `u64` (see [§clientOrderId generation](#clientorderid-generation) for the upstream `serde_json::json!` ceiling) | `InvalidParameter` |
| Resolved `token_type` exists in `ref_tokens` | `MarketInconsistent` |

Two remap notes:

- Quote-asset precision violations surface as `-1130 InvalidParameter`, not `-1111 PrecisionExceeded`. Quote-asset precision is **not** part of [api-spec §Validation Rules]; the spec lumps it with "other body shape violation" in the [api-spec §Buy Full Set error table](../api-spec.md#buy-full-set).
- `token_type` missing from `ref_tokens` is read-model corruption — the initial migration seeds the canonical set, so a miss means the read-model fell out of sync with the on-chain registry.

Free-balance is not pre-checked. The chain enforces sufficiency on-chain (`ERR_LOW_VALUE` at `contracts/PrivateNote.sol`) and the synchronous chain return maps that to `OrderValidationFailed` → 400 / -2010, same shape as placement.

#### Quantum lower bound is not pre-validated

`PMP.splitFullSet` computes `t = collateral / Q` and credits `t * u_k` of each outcome (`u_k = clean_pool[k] / gcd(clean_pool[*])`, `Q = sum(u_k)`). When `collateral < Q` the integer division yields `t = 0` and `PMP.splitFullSet` reverts with `ERR_LOW_VALUE`. The revert lives in the PMP-side downstream tx that the synchronous chain return does not observe — so the `POST` returns `200`, `PrivateNote.onBounce` restores `_balance[tokenType]` from the candidate slot, and the caller is left with no outcome-token credits. This is the same async no-credit path as the other PMP-side reverts listed in [Failure surface](#failure-surface) class 3. `Q` is determined at the moment of the first post-stakeEnd split; on a market still in `AWAITING_FREEZE` no split has run yet, so the value the use case would need to compare against does not exist. The API therefore does not pre-validate the lower bound — callers must size against their own knowledge of pool composition, or accept that a too-small `collateral` lands on that async refund path.

### Chain submission

Encode and dispatch a `PrivateNote.splitFullSet` external message against `trading_pn.pn_address`. ABI from `contracts/PrivateNote.sol::splitFullSet`, exposed by `ackinacki-kit/contracts/src/dex/private_note.rs::ParamsOfSplitFullSet`:

```text
splitFullSet(
  eventId,         // uint256, markets.event_id
  oracleListHash,  // uint256, markets.oracle_list_hash
  tokenType,       // uint32,  markets.token_type
  collateral,      // uint128 ABI, capped at u64 today (same serde_json constraint as placeOrder)
)
```

Sender boundary: the `ChainOrderSender` trait in `crates/application/src/lib.rs` carries `async fn split_full_set(&self, payload: SplitFullSetPayload) -> Result<(), DomainError>`. The production `DexChainSender` impl wraps `dodex_chain::Dex::split_full_set`; the call shares `classify_chain_outcome` and `map_tvm_exit_code` with the other write-path entry points. The full PN-side reject surface for splitFullSet is enumerated in [§Failure surface](#failure-surface) below.

`dodex_chain::Dex::split_full_set` lives in `crates/chain/src/client.rs` alongside the other trader-path methods, available without the `test-helpers` feature so the prod API build links it directly.

`chain.split_full_set_timeout_ms` in `config/api.<env>.yaml` bounds the per-call wait. `ApiConfig::validate` pins `server.request_timeout_ms > chain.split_full_set_timeout_ms` at boot — same invariant the other chain timeouts carry. Elapsed surfaces as `RequestTimeout` → 504 / -1007 with the same "retry with the same id" contract as placement.

### Response

A successful submission returns the minimal two-field body from [api-spec §Buy Full Set](../api-spec.md#buy-full-set):

| Field | Source |
| --- | --- |
| `marketAddress` | Echoed from the request. |
| `transactTime` | `now_pair()` captured once at the start of the handler. |

Why minimal: the resulting collateral debit and per-outcome credits become visible through [`GET /api/v1/account`](read-api.md) and [`GET /api/v1/account/balances`](read-api.md) once the chain confirms — the synchronous response confirms acceptance only. There is no chain-assigned identifier the response could carry that the caller does not already have.

### Failure surface

Two failure classes — synchronous (pre-submit + PrivateNote chain-side) and async (the downstream `PMP.splitFullSet` execution that follows the synchronous return). The synchronous path is identical to placement's classes 1 and 2:

1. **Pre-submit** — request shape, market resolution, status gate, collateral validation. Mapped per [Error mapping](#error-mapping-3).

2. **PrivateNote chain-side, surfaced synchronously** — `dodex_chain::Dex::split_full_set` awaits the chain's execution of `PrivateNote.splitFullSet`, so any `require(...)` failure inside that ABI call comes back as a typed `AppError` carrying the TVM `exit_code`. `map_tvm_exit_code` (in `crates/infrastructure/src/chain_sender.rs`) translates the codes from `contracts/modifiers/errors.sol`. `splitFullSet` itself carries four PN-side `require(...)` invariants spanning three exit codes (`contracts/PrivateNote.sol::splitFullSet`): `121 ERR_NOTE_BUSY` (per-PN serial), `102 ERR_LOW_VALUE` (collateral non-positive *or* free `_balance[tokenType]` below `collateral` — two separate `require`s, same code), and `150 ERR_DEBT_NON_ZERO` (PN has outstanding debt). All three codes are already wired into the shared exit-code map; no new mapping required.

3. **PMP-side require failure, surfaced asynchronously via bounce** — `PrivateNote.splitFullSet` accepts the request and sends an internal message to `PMP.splitFullSet`, which executes in a separate transaction the synchronous return cannot observe. If `PMP.splitFullSet` reverts (`t = 0` for `collateral < Q`, status drift between the read-model snapshot and the on-chain phase, normalization refund still pending, market resolved/cancelled mid-flight), the message bounces back to `PrivateNote.onBounce` which restores `_balance[tokenType]` from the candidate slot — the caller ends up with the original collateral refunded and no outcome-token credits. From the HTTP standpoint the `POST` returned `200`; clients detect this class through `GET /api/v1/account/balances` showing no new outcome-token credits and the `GET /api/v1/account` `free` quote-asset row unchanged.

Transport-level failures collapse to `Unexpected` → 500 / -1000 with the raw `AppError` logged at `error`, same as placement.

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| Auth envelope / unknown api_key / bad signature / timestamp | handled upstream by [auth_hoop](auth.md#authentication) | 401 |
| Caller lacks `TRADE` permission | `AuthRequired` | 401 |
| `marketAddress` or `collateral` missing | `MissingParameter` | 400 |
| `collateral` not positive, exceeds quote-asset precision, non-numeric, or lifted above `u64::MAX` | `InvalidParameter` | 400 |
| Market unknown or pre-reconcile | `InvalidMarketOrSymbol` | 404 |
| Reconciled market with NULL/blank `oracle_list_hash`, or quote `token_type` absent from `ref_tokens` | `MarketInconsistent` | 503 |
| Resolved market `status ∉ { AWAITING_FREEZE, TRADING }`; chain `ERR_LOW_VALUE` / `ERR_DEBT_NON_ZERO` | `OrderValidationFailed` | 400 |
| Chain `ERR_NOTE_BUSY` (per-PN serial; another op still in flight) | `OrderPnBusy` | 429 |
| Handler exceeded `ServerSection.request_timeout_ms` | `RequestTimeout` | 504 |
| Unmapped chain `tvm_exit` code or gateway transport failure | `Unexpected` | 500 |

`RequestTimeout` (`-1007`) is enforced at two layers: the HTTP request_timeout hoop (`services/api/src/timeout_hoop.rs`) for handler-wide budgets, and the chain sender (`crates/infrastructure/src/chain_sender.rs::classify_chain_outcome`) for gateway-side hangs. `ApiConfig::validate` pins `server.request_timeout_ms > chain.split_full_set_timeout_ms` at boot so the HTTP timeout cannot fire while a chain submission is still in flight.

### Layering

| Layer | Responsibility |
| --- | --- |
| `crates/domain` | Reuses `parse_positive_decimal` / `lift_decimal`. No new primitives. |
| `crates/application` | `BuyFullSetInput` (HTTP-shaped), `SplitFullSetPayload` (chain-shaped), `MarketForBuyFullSet` (repo-shaped), `BuyFullSetUseCase`. `ChainOrderSender::split_full_set` and `MarketReadRepository::resolve_for_buy_full_set` carry the trait surface. Quote-asset `decimals` come from `ReferenceRepository::lookup_ref_token`, the same source the `/account` path uses. |
| `crates/infrastructure` | `DexChainSender::split_full_set` wraps `dodex_chain::Dex::split_full_set` (pubkey/seckey re-encode and `classify_chain_outcome` are shared across every entry point). `PostgresReadModelRepository::resolve_for_buy_full_set` runs a single-row `SELECT` against `markets` with the `last_reconciled_at IS NOT NULL` visibility gate. A NULL/blank `oracle_list_hash` is logged as a `warn` and surfaces as `MarketInconsistent`. |
| `services/api` | `buy_full_set` handler attached as `Router::with_path("api/v1/buyFullSet").post(buy_full_set)` on the auth subrouter. HMAC enforced by `auth_hoop`; permission by `require_auth(Permission::Trade)`. The `DexChainSender` construction in `run()` carries `chain.split_full_set_timeout_ms`. |

The use case constructor takes trait objects; `services/api/tests/buy_full_set_http.rs` injects `FakeRepo` + `FakeAuthenticator` + `RecordingSplitFullSetSender` against the full router, matching the triad established by `create_order_http.rs` and `cancel_order_http.rs`.

### Idempotency and retries

The backend does not store inflight submissions and does not retry on its own. `splitFullSet` is **not** idempotent at the chain level: repeating the call with the same `collateral` on the same `(eventId, oracleListHash, tokenType)` results in a second deposit and a second per-outcome credit. Clients that need at-least-once delivery must track acceptance through `GET /api/v1/account` (`free` quote-asset row dropping by `collateral`) and refrain from re-`POST`ing on transient errors.

### Concurrency

`splitFullSet` takes the same per-PN `_busy` lock as placement — see [§Concurrency for POST /order](#concurrency). A buyFullSet that races a placement, cancellation, or another buyFullSet from the same account surfaces as `OrderPnBusy` → 429 / -2014 with the same retry semantics.

## `POST /api/v1/accounts`

Registers a trading account from a client-supplied PrivateNote and mints its first API credential. The self-service counterpart of the operator seeder (`crates/infrastructure/src/seed.rs`, [seed-private-notes.md](../seed-private-notes.md)): one note in, one account plus one api_key out. Public — the caller has no credential yet, so possession of the note's custody keys (sent in the body) is the authorization. Always mounted; unlike the seeder it carries no config gate.

The handler runs three phases: request parsing → on-chain existence probe → credential mint and insert. Each fails closed with its own code (see the Error mapping table in this section).

### Authorization

`NONE`. The route is pushed outside the auth subrouter in `build_router`, so `auth_hoop` never runs for it — a client registering its first credential has nothing to sign with. The submitted `pnSeckeyHex` is the capability: only the note owner holds it, and the backend takes custody of it (sealed under the KEK) to sign that account's trades, exactly as the seeder does. There is no `require_auth` and no `AuthContext`.

### Request parsing

| Field | Type | Notes |
| --- | --- | --- |
| `pnAddress` | `String` | Mandatory. Deployed PrivateNote address. |
| `pnPubkeyHex` | `String` | Mandatory. Owner public key, hex (≤256 bits). |
| `pnSeckeyHex` | `String` | Mandatory. Owner secret key, 32-byte hex. |
| `pnDihHex` | `String` | Mandatory. `deposit_identifier_hash`, hex (≤256 bits). |

`RegisterAccountRequest` is parsed with `deny_unknown_fields` — manual `ToSchema` impl, the same salvo-oapi-macros constraint as `CancelBatchOrdersRequest`. An unknown key is a serde shape error → `InvalidParameter` → 400 / -1130. Each field is `Option` only so a missing or blank one surfaces as `MissingParameter` → 400 / -1102 via the handler's `non_empty(...).ok_or(...)` chain; all four are required. Field-format validation (hex, bit width, seckey length) happens in the registry, not the handler — see the Input validation section.

### On-chain existence probe

Before any DB write, `RegisterAccountUseCase` runs `PnStateReader::get_details(pnAddress)` against the GraphQL gateway — the same reader `GET /api/v1/account` uses. It exists only to confirm the note is deployed, so a registered account can trade right away:

- BOC absent → the reader returns a typed `AccountNotDeployed` → 404 / -2013, no row written.
- Any other reader failure (gateway flap, ABI/parse error) → `MarketInconsistent` → 503, matching `GetAccountUseCase`'s reader-failure posture so a transient gateway flap is retryable rather than a hard 500.
- BOC present → proceed. The returned details are discarded; only existence matters.

Running the probe first means a not-deployed note costs no DB work and a well-formed but undeployed `pnAddress` can never leave an orphan account row.

### Input validation

In `PostgresAccountRegistry` (pure `validate_note`, unit-tested without a DB):

| Rule | Failure |
| --- | --- |
| `pnPubkeyHex` valid hex, ≤256 bits | `InvalidParameter` |
| `pnDihHex` valid hex, ≤256 bits | `InvalidParameter` |
| `pnSeckeyHex` valid hex | `InvalidParameter` |
| `pnSeckeyHex` decodes to exactly 32 bytes (ed25519) | `InvalidParameter` |

Hex public-key and dih are converted to the decimal the `numeric(78,0)` columns expect via the shared `seed::hex_to_dec_uint256` (promoted to `pub(crate)`). A wrong-length seckey is rejected here rather than surfacing as a silent on-chain signature failure on the client's first trade.

### Credential mint

`api_secret` is a fresh 32 random bytes from the OS RNG (`crypto::fill_random`), **not** a KEK-derived value. Derivation (`crypto::derive_api_secret`) exists only to reproduce credentials from a notes-file index; a runtime registration has no such index, and the secret is stored sealed regardless, so random is the natural choice. `api_key` is `dk_live_` followed by 16 random bytes as hex (128-bit, unguessable). Both `pn_seckey` and `api_secret` are sealed under the KEK (`crypto::seal`) before insert — the plaintext secret exists only between mint and response.

### Persistence

One transaction in `PostgresAccountRegistry::register`:

1. INSERT into [`accounts`](data-schema.md#accounts) (label `registered`, `pn_address`, `pn_pubkey`, sealed `pn_seckey`, `pn_dih`) `ON CONFLICT DO NOTHING RETURNING id`. **No arbiter target** — `accounts` is unique on both `pn_address` and `pn_dih` (`accounts_pn_dih_key`), so a bare `DO NOTHING` suppresses a conflict on either index and returns no row. No row → roll back → `NoteAlreadyRegistered` → 409 / -2015. Insert-only: an existing note is never re-credentialed and its secret is never re-issued.
2. INSERT into [`api_keys`](data-schema.md#api_keys) (`account_id`, `api_key`, sealed `api_secret`, `{USER_DATA,TRADE}`) `ON CONFLICT (api_key) WHERE disabled_at IS NULL DO NOTHING`. Zero rows affected means the 128-bit random `api_key` collided with a live key — astronomically unlikely; fail closed with `Unexpected` → 500 (rolling back the account insert) rather than hand back a credential pointing at another account.

A concurrent identical registration blocks on the `accounts` insert until the first commits, then sees the conflict and returns `NoteAlreadyRegistered` too.

### Response

200 with the credential — the only point the plaintext secret leaves the backend:

| Field | Source |
| --- | --- |
| `accountId` | `accounts.id` of the inserted row. |
| `pnAddress` | Echoed from the request. |
| `apiKey` | The minted `dk_live_…` key. |
| `apiSecret` | The minted secret, hex. Stored only sealed; never retrievable again. |
| `permissions` | `["USER_DATA", "TRADE"]`. |

### Failure surface

1. **Pre-write** — body shape (`-1102` / `-1130`) and the chain probe (`-2013` not deployed, `-1500` gateway/parse failure). No row written.
2. **Conflict** — `pn_address` or `pn_dih` already present (`-2015`). Transaction rolled back.
3. **Internal** — sealing failure, DB error, or the `api_key` collision (`-1000`), logged at `error`, transaction rolled back.

### Error mapping

| Condition | DomainError | HTTP |
| --- | --- | --- |
| A mandatory field missing or blank | `MissingParameter` | 400 |
| Malformed field (bad hex, >256 bits, wrong seckey length, unknown body key) | `InvalidParameter` | 400 |
| PrivateNote not deployed on-chain | `AccountNotDeployed` | 404 |
| Gateway flap or PN BOC parse failure during the probe | `MarketInconsistent` | 503 |
| `pn_address` or `pn_dih` already registered | `NoteAlreadyRegistered` | 409 |
| Sealing, DB failure, or `api_key` collision | `Unexpected` | 500 |

### Layering

| Layer | Responsibility |
| --- | --- |
| `crates/domain` | New `NoteAlreadyRegistered` variant (`-2015` / 409). |
| `crates/application` | `NewAccountNote` / `RegisteredAccount`, the `AccountRegistry` port, and `RegisterAccountUseCase` (chain probe, then delegate). |
| `crates/infrastructure` | `account_registry::PostgresAccountRegistry` (validate, random + sealed credential, insert-only transaction). `crypto::fill_random`. `seed::hex_to_dec_uint256` promoted to `pub(crate)`. |
| `services/api` | `RegisterAccountRequest` / `RegisterAccountResponse` DTOs, `register_account` handler, public route `Router::with_path("api/v1/accounts").post(register_account)` outside the auth subrouter. `AppState.registry: Option<SharedRegistry>` is builder-set (`with_account_registry`) so the existing test `AppState::new` sites stay unbroken; `run()` always wires the Postgres registry. |

Tests: `register_account_http.rs` drives the full router against the real Postgres registry plus `FakePnStateReader` (success, `-2015`, `-2013`, `-1102`, `-1130`); `account_registry` unit tests cover `validate_note`; `e2e_register_account.rs` (`#[ignore]`) exercises the real `GraphqlPnStateReader` against shellnet.

### Idempotency and retries

Not idempotent, and insert-only. A retry after a successful registration returns `NoteAlreadyRegistered` (`-2015`), not a fresh credential; a client that loses its `apiSecret` cannot recover it by re-registering, because the secret is never re-issued. A retry after a transient `-1500` (gateway flap during the probe) is safe — no row is written until the probe succeeds.

### Concurrency

No per-PN `_busy` lock: registration submits no external chain message, it only reads PN state. Concurrency is bounded by the `accounts` unique indexes — two simultaneous registrations of the same note resolve to one insert plus one `-2015`, as described in the Persistence section.
