# dodex-api

The api is the reader side of the Dodex backend. It is a Salvo HTTP server
that serves market metadata and order-book snapshots straight from the
Supabase Postgres read-model. The api never touches GraphQL, the on-chain
state, or BOC decoding — that is the indexer's job.

## Routes

| Method | Path                | Status                                                  |
| ------ | ------------------- | ------------------------------------------------------- |
| GET    | `/readiness`        | Liveness probe; always returns `200 ok`.                |
| GET    | `/api/v1/markets`   | Implemented. Lifecycle, timings, oracle event, terminal. |
| GET    | `/api/v1/depth`     | Implemented. On-the-fly aggregation over `live_orders`.  |

## `GET /api/v1/markets`

Single-market lookup or filtered, sorted, cursor-paginated listing. See
[docs/api-spec.md#markets](../../docs/api-spec.md#markets) for the public
contract; this section describes the implementation.

Query parameters:

- `marketAddress` — when present, returns one market only. Mutually exclusive
  with every filter and pagination parameter below; combining them returns
  `400`.
- `status` — comma-separated subset of the nine lifecycle phases (`PENDING`,
  `UPCOMING`, `STAKING`, `AWAITING_FREEZE`, `TRADING`, `RESOLVING`, `RESOLVED`,
  `CANCELLED`, `EXPIRED`).
- `quoteAsset` — exact match against `markets.token_code` (`USDC`, `NACKL`, …).
- `oracleName` — exact match against the joined `oracles.name`.
- `closingBefore` — unix seconds; keeps markets with `result_end < value`.
- `sort` — `resultStart` (default, ASC) or `createdAt` (DESC).
- `cursor` — opaque cursor returned by a previous page.
- `limit` — page size, default 50, capped at 200.

Response shape: `serverTime` (unix seconds), `nextCursor`, `hasMore`, and an
array of markets. Every market carries `marketAddress`, `orderBookAddress`,
`marketName`, `status`, `quoteAsset`, `tokenType`, `createdAt`,
`timings | null` (null only for `PENDING`), `event` (oracle metadata),
`terminal | null` (set for `RESOLVED` / `CANCELLED` / `EXPIRED`), and
`outcomes` (each with stable `outcomeId`).

### Status derivation

The api computes `status` in Rust against a single `now = serverTime` so the
response is internally consistent. Order of checks (first match wins):

1. `cancelled_at` set → `CANCELLED`.
2. `resolved_at` set → `RESOLVED`.
3. `stake_start` is null → `PENDING`.
4. `now > result_end` → `EXPIRED`.
5. `now >= result_start` → `RESOLVING`.
6. `frozen_at` set → `TRADING`.
7. `now >= stake_end` → `AWAITING_FREEZE`.
8. `now >= stake_start` → `STAKING`.
9. otherwise → `UPCOMING`.

The same expression is mirrored in SQL as a `CASE` so `status=…` filters can
be pushed down without re-fetching pages.

### Pagination

Cursor encodes `(sort_key, id)` as URL-safe base64 and is keyset-stable: the
SQL adds a strict tuple comparison (`>` for `resultStart` ASC, `<` for
`createdAt` DESC) and pulls `limit + 1` rows. The extra row, when present, is
dropped from the page and signals `hasMore = true`; the cursor is taken from
the last kept row.

### Read query

`PostgresReadModelRepository::list_markets` issues two queries: one selects
the markets page (LEFT JOIN through `oracle_events ⨝ oracle_event_lists ⨝
oracles` keyed on `confirmed_pmp_address = m.pmp_address`); a second bulk
query (`market_outcomes WHERE market_id_fk = ANY($1)`) loads outcomes for the
returned ids. `markets.last_reconciled_at IS NOT NULL` is always part of the
WHERE so half-populated rows (just-deployed markets the reconciler has not
visited yet) never reach clients.

### Identifier formats

`event_id` in Postgres is `numeric(78,0)`; the api re-encodes it as
`0x` + 64 zero-padded hex chars (uint256), matching the on-chain shape.

## `GET /api/v1/depth`

Returns bids and asks for one outcome of one market, aggregated on the fly
from `live_orders`. Required query params: `marketAddress`, `symbol`.
Optional `limit` (default 100, max 1000).

Resolution: `marketAddress` + `symbol` → `(orderbook_address, outcome_id)`
through `markets ⨝ market_outcomes`, with `last_reconciled_at IS NOT NULL`.
A miss returns `404` (`InvalidMarketOrSymbol`). When the market exists but
its OrderBook has not been deployed yet (no `orderbook_address`), the
response is an empty book with `lastUpdateId = 0`.

Aggregation: `SUM(amount_remaining) GROUP BY (is_buy, price)` over rows with
`status = 'OPEN' AND amount_remaining > 0`. The sort is numerical (parsed
through `BigUint`, not lexicographic) so `uint256` prices order correctly:
bids descending, asks ascending. The `limit` is applied per side after the
sort.

`lastUpdateId` is `MAX(last_event_lt)` across the orderbook (currently
`node.created_at` in unix seconds, populated by the OrderBook projectors).
This is enough for the spec's `bigint` contract; if a depth-diff stream
lands later, swap the source for a per-orderbook nonce without touching
the API contract.

Behind the scenes the indexer projects three OrderBook events into
`live_orders`:

- `OrderPlaced` — upsert a row in `OPEN`.
- `OrderFilled` — decrement `amount_remaining`; flip to `FILLED` at zero.
- `OrderCancelled` — flip to `CANCELLED`, zero `amount_remaining`.

`PartialFill` / `FullyFilled` / `Queued` / `Rejected` / `CallbackBounced`
are observability-only and intentionally skipped.

## Architecture

```text
Salvo Router
   └── inject(AppState { repo: Arc<dyn MarketReadRepository> })
        ├── GET /readiness                 → readiness()
        ├── GET /api/v1/markets             → get_markets()
        └── GET /api/v1/depth               → get_depth()
```

`AppState` is injected through Salvo's `affix_state` and read out of the
`Depot` per request. The repository trait lives in
`crates/application/src/lib.rs`; its only production implementation is
`PostgresReadModelRepository` in `crates/infrastructure/src/postgres_repo.rs`.
There is no stub repository — the api requires a Postgres connection.

DTO layer in `services/api/src/main.rs` keeps the wire format independent of
the domain types. Domain enums (`MarketStatus`, `TerminalKind`, `CancelReason`)
serialise as the spec strings via small `as_str` helpers, not via serde
defaults, so renaming a domain variant cannot accidentally change the public
contract.

`ApiError` wraps `DomainError` and maps it to HTTP status + Binance-style
`{ code, msg }` body; `Unexpected` becomes `500`, `AuthRequired` /
`InvalidSignature` / `TimestampOutsideRecvWindow` become `401`,
`UnknownOrder` becomes `404`, everything else `400`.

## Configuration

YAML at `config/api.<env>.yaml`. Override path with
`APP_CONFIG=/path/to/file.yaml`. Schema is shared with the indexer's
`CommonSection` plus a `server` block; `serde(deny_unknown_fields)` rejects
any unknown key:

```yaml
app:
  env: local
  log_level: info

server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000

database:
  url: postgres://postgres:postgres@localhost:5432/dodex
  max_connections: 10
  min_connections: 1
  connect_timeout_ms: 3000
```

Send `SIGUSR1` to the running process to reload the config; the Postgres pool
is rebuilt only if its parameters changed.

## Running

```sh
cargo run -p dodex-api
```

The api needs a Postgres database. For local development, point it at the
same database the indexer writes to (the indexer applies migrations on
startup). Without ingestion data the listing will be empty but the endpoint
itself works.

Smoke check:

```sh
curl -s 'http://localhost:8080/readiness'
curl -s 'http://localhost:8080/api/v1/markets?limit=5' | jq
curl -s 'http://localhost:8080/api/v1/markets?status=TRADING,RESOLVING&sort=createdAt' | jq
```

## Tests

Unit-level, all in `crates/infrastructure/src/postgres_repo.rs`:

- `derive_status` covers every transition across the nine lifecycle phases,
  plus `RESOLVED` overriding mid-staking timings and `CANCELLED` overriding
  `RESOLVED`.
- `cursor_roundtrip` — opaque cursor encode/decode parity.
- `numeric_to_hex_works` — `event_id` re-encoding.
