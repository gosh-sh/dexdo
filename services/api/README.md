# dodex-api

The api is the reader side of the Dodex backend. It is a Salvo HTTP server
that serves market metadata and order-book snapshots straight from the
Supabase Postgres read-model. The api never touches GraphQL, the on-chain
state, or BOC decoding — that is the indexer's job.

## Routes

| Method | Path                | Security    | Status                                                  |
| ------ | ------------------- | ----------- | ------------------------------------------------------- |
| GET    | `/readiness`        | `NONE`      | Liveness probe; always returns `200 ok`.                |
| GET    | `/api/v1/markets`   | `NONE`      | Implemented. Lifecycle, timings, oracle event, terminal. |
| GET    | `/api/v1/depth`     | `NONE`      | Implemented. On-the-fly aggregation over `live_orders`.  |
| POST   | `/api/v1/order`     | `TRADE`     | Stub. HMAC + permission gate enforced; response body is a placeholder until the place-order pipeline lands. |

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

1. `cancelled_at` set **or** `is_cancelled = true` → `CANCELLED`. Either signal is
   sufficient: the cancellation-event projector stamps both, the reconciler
   stamps `is_cancelled` (plus a discovery-time fallback into `cancelled_at`)
   from `PMP.getDetails().isCancelled` so the API still reports the spec-required
   terminal state when the event was missed or has not been replayed yet.
2. `resolved_at` set → `RESOLVED`.
3. `stake_start` is null → `PENDING`.
4. `now >= result_end` → `EXPIRED`.
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
A miss returns `404` (`InvalidMarketOrSymbol`). A reconciled market is
guaranteed to carry `orderbook_address` (migration 0014 CHECK); a NULL or
blank value at this point is treated as `MarketInconsistent` → 503. The
legitimate empty-book case is "address stamped, no `OrderPlaced` events
yet" — the response is empty `bids` / `asks` with `lastUpdateId = 0`.

Aggregation: `SUM(amount_remaining) GROUP BY (is_buy, price)` over rows with
`status = 'OPEN' AND amount_remaining > 0`. The sort is numerical (parsed
through `BigUint`, not lexicographic) so `uint256` prices order correctly:
bids descending, asks ascending. The `limit` is applied per side after the
sort.

`lastUpdateId` is `MAX(last_event_lt)` over `live_orders` filtered to
`(orderbook_address, outcome_id)` — same scope as the depth response, so a
quiet outcome never inherits a sibling outcome's sequence number. The value
is `node.created_at` in unix seconds, populated by the OrderBook projectors.
This is enough for the spec's `bigint` contract; if a depth-diff stream
lands later, swap the source for a per-(orderbook, outcome) nonce without
touching the API contract.

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
   └── inject(AppState { repo, authenticator })
        ├── GET /readiness                       → readiness()
        ├── GET /api/v1/markets                  → get_markets()
        ├── GET /api/v1/depth                    → get_depth()
        └── subrouter (hoop = auth_hoop::authenticate)
             └── POST /api/v1/order              → create_order()  [stub]
```

`AppState` is injected through Salvo's `affix_state` and read out of the
`Depot` per request. The `MarketReadRepository` trait lives in
`crates/application/src/lib.rs`; its only production implementation is
`PostgresReadModelRepository` in `crates/infrastructure/src/postgres_repo.rs`.
The `Authenticator` trait is alongside the repo; its implementation is
`PostgresAuthenticator` in `crates/infrastructure/src/auth.rs`. There is no
stub for either — the api requires a Postgres connection.

The DTO layer in `services/api/src/lib.rs` keeps the wire format independent
of domain types. Domain enums (`MarketStatus`, `TerminalKind`, `CancelReason`)
serialise as the spec strings via small `as_str` helpers, not via serde
defaults, so renaming a domain variant cannot accidentally change the public
contract.

`ApiError` wraps `DomainError` and maps it to HTTP status + Binance-style
`{ code, msg }` body; `Unexpected` becomes `500`, `AuthRequired` /
`AuthEnvelopeIncomplete` / `InvalidSignature` / `TimestampOutsideRecvWindow`
become `401`, `UnknownOrder` becomes `404`, everything else `400`.

## Authentication

`USER_DATA` and `TRADE` requests carry an `X-DODEX-APIKEY` header and the
`timestamp` / `recvWindow` / `signature` query parameters defined in the
public HTTP contract. The hoop in `auth_hoop.rs` extracts those, hands them
to the `Authenticator` port, and either injects an `AuthContext` into the
Depot or short-circuits with the spec error body (`{code, msg}` + 401).
The full pipeline, error-code mapping, and the user/PN/api_key model are
described in [`docs/tech-specs/auth.md`](../../docs/tech-specs/auth.md).

The hoop runs only on the private subrouter, so the `NONE`-security routes
above are not authenticated.

## Local development setup

### KEK

The api refuses to start without `DODEX_KEK_HEX` (32-byte hex). It is the
master key used to encrypt `api_secret` and `pn_seckey` at rest. The
repository ships a committed `.env` with a local-only KEK; `dotenvy` loads
it on startup if it is found and does not override variables already
present in the process environment, so production deployments inject their
own KEK through their secret-management system without touching the file.

For a brand-new machine no setup is required — `cargo run -p dodex-api`
picks up `.env` from the workspace root. To override, export
`DODEX_KEK_HEX` in the shell.

### Test credentials

When `auth.seed_accounts: true` (default in `config/api.local.yaml`) the
api applies migrations and inserts ten pre-generated test accounts on
boot, idempotently. The resulting `api_key` / `api_secret` pairs are
listed in [`docs/seed_accounts.txt`](../../docs/seed_accounts.txt) — the
secrets cannot be recovered from the database afterwards, so that file
is the only place to look them up.

Set `auth.seed_accounts: false` in non-local configs.

### Test database

`tests/` integration tests rely on a throwaway Postgres brought up by
`docker-compose.test.yml`:

```sh
docker compose -f docker-compose.test.yml up -d --wait
cargo test -p dodex-api --tests -- --test-threads=1
```

`TEST_DATABASE_URL` is read from the committed `.env`; no manual export
is needed.

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

The API is restart-to-reconfigure: every request path reads values that were
captured when the process started (database pool, listen host/port, server
timeouts), so there is no SIGUSR1 reload loop here. Restart the service to
pick up edits to `config/api.<env>.yaml`. (The indexer **does** support
SIGUSR1; its background tasks read live config — see
`services/indexer/README.md`.)

## Running

```sh
cargo run -p dodex-api
```

The api needs a Postgres database. For local development, point it at the
same database the indexer writes to (the indexer applies migrations on
startup). Without ingestion data the listing will be empty but the endpoint
itself works.

Public smoke (no auth required):

```sh
curl -s 'http://localhost:8080/readiness'
curl -s 'http://localhost:8080/api/v1/markets?limit=5' | jq
curl -s 'http://localhost:8080/api/v1/markets?status=TRADING,RESOLVING&sort=createdAt' | jq
```

Auth smoke against the stub `POST /api/v1/order` with one of the seeded
keys (the `api_key` / `api_secret` pair from `docs/seed_accounts.txt` —
substitute your own if you regenerate them):

```sh
API_KEY=dk_live_test_001
API_SECRET=1de6fc5cf8899e7f1dacf449fe46c3c88854478b7fcd9dd26c664535ee589966

TS=$(date +%s%3N)
QS="recvWindow=5000&timestamp=$TS"
SIG=$(printf '%s' "$QS" | openssl dgst -sha256 -hmac "$API_SECRET" -hex | cut -d' ' -f2)

curl -s -X POST "http://localhost:8080/api/v1/order?$QS&signature=$SIG" \
    -H "X-DODEX-APIKEY: $API_KEY" \
    -H 'Content-Type: application/json'
# {"accountId":"...","status":"STUB"}
```

The HMAC is computed over the canonical query string (sorted by key,
`signature` removed) concatenated with the raw body bytes — `openssl
dgst` reads the same bytes that go on the wire.

## Tests

Unit-level (in the corresponding `src/*.rs` files):

- `crates/infrastructure/src/postgres_repo.rs` — `derive_status` covers
  every transition across the nine lifecycle phases, plus `RESOLVED`
  overriding mid-staking timings and `CANCELLED` overriding `RESOLVED`;
  `cursor_roundtrip` checks opaque cursor encode/decode parity;
  `numeric_to_hex_works` covers `event_id` re-encoding.
- `crates/infrastructure/src/auth.rs` — `canonical_query_string`,
  `check_recv_window`, `verify_hmac` primitives plus key masking.
- `crates/infrastructure/src/crypto.rs` — KEK envelope round-trip and
  tamper detection.
- `crates/infrastructure/src/seed.rs` — baked seed JSON deserialises
  cleanly and every credential parses.

Integration (gated on `TEST_DATABASE_URL`; the committed `.env` points
at the `docker-compose.test.yml` Postgres):

- `crates/infrastructure/tests/seed.rs` — fresh-DB inserts and rerun
  idempotency for the bootstrap seeder.
- `crates/infrastructure/tests/markets_status.rs`,
  `tests/depth.rs`, `tests/reprojection.rs`,
  `tests/oel_reconciler.rs` — repository and projector behavior against
  the real schema.
- `services/api/tests/auth_http.rs` — full auth pipeline through Salvo's
  in-process `TestClient`: missing envelope (`-1003`), unknown /
  USER_DATA-only credentials (`-1002`), stale timestamp (`-1021`),
  bad signature (`-1022`), `recvWindow` overshoot silent clamp, and
  the happy-path 200 + `accountId`.
- `services/api/tests/public_smoke.rs` — verifies the auth hoop is
  scoped to the private subrouter (public routes stay unauthenticated).
