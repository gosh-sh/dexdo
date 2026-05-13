# Trading API — Read Side Technical Specification

Implementation-facing requirements for the read endpoints of the trading API. The public contract (URLs, field names, parameter rules, error shapes, response examples) lives in [api-spec.md](../../api-spec.md). Postgres tables referenced below are documented column-by-column in [data-schema.md](../data-schema.md). The write side — how those tables get populated from chain events — is in [market-data-indexer.md](../market-data-indexer.md). Write-side trading endpoints (`POST /order`, `DELETE /order`, batch variants) are in [write-api.md](write-api.md).

Endpoints covered by this document:

| Endpoint | Status |
| --- | --- |
| `GET /api/v1/openOrders` | this revision |
| `GET /api/v1/allOrders` | not implemented yet |

## Glossary

**Read-model** — Postgres tables prepared for API reads. The indexer builds these tables from chain events; the API reads them instead of querying contracts directly.

**Owner attribution** — the binding between a chain-side order (`orderbook_address`, `order_id`) and the trading PrivateNote address that placed it. `OrderBook.OrderPlaced` does not carry the owner; attribution arrives separately via `PrivateNote.OrderPlacedConfirmed` and is stored in `live_orders.owner_pn_address`.

**Trading PN** — the trading PrivateNote address of an authenticated account. Resolved from the API key by the existing auth hoop and exposed as `ctx.trading_pn.pn_address`.

**Chain time** — `raw_events.created_at_chain` of the event that produced a state transition. Used for response `time` and `updateTime` so they are stable under indexer backlog.

## `/api/v1/openOrders`

### Source data

The endpoint reads exclusively from [`live_orders`](../data-schema.md#live_orders). A row contributes to the response iff all hold:

- `owner_pn_address = ctx.trading_pn.pn_address` — caller is the owner.
- `status = 'OPEN'` — neither `FILLED` nor `CANCELLED`.
- `amount_remaining > 0` — defence-in-depth; an `OPEN` row with zero remainder would be a projector bug.
- The parent market in [`markets`](../data-schema.md#markets) has `last_reconciled_at IS NOT NULL` — pre-reconcile markets are hidden symmetrically with `/api/v1/markets`.

The query joins through `markets` and [`market_outcomes`](../data-schema.md#market_outcomes) to recover the public identifiers `pmp_address` and `symbol` for each row, plus `price_precision` / `quantity_precision` for scaling. See [§ SQL](#sql) for the two query variants.

### Filter resolution

The endpoint accepts three filter shapes (see the public spec for client-facing wording):

| Inputs | Behaviour |
| --- | --- |
| neither `marketAddress` nor `symbol` | all-markets query, owner-scoped. |
| both present | resolve `(marketAddress, symbol)` to `(orderbook_address, outcome_id)` via `markets ⨝ market_outcomes`. If the pair is missing or its market is not reconciled → `DomainError::InvalidMarketOrSymbol` → `-1121` / 404. |
| exactly one present | `DomainError::MissingParameter` → `-1102` / 400. |

The pair-resolution lookup is a separate SQL round-trip that runs before the main query so the unknown-pair case can be distinguished cleanly from "owner has no orders here". Resolution is bound by `last_reconciled_at IS NOT NULL` so a pair that exists in `markets` but has never reconciled is reported the same way as a pair that does not exist.

### Open-order status mapping

The public `status` enum is derived from row state, not stored:

| Row state | Public `status` |
| --- | --- |
| `executed_qty == 0` (i.e., `amount_remaining == amount_initial`) | `NEW` |
| `executed_qty > 0` | `PARTIALLY_FILLED` |

`FILLED`, `CANCELED`, `REJECTED` rows never reach this code path because they fail the `status = 'OPEN' AND amount_remaining > 0` predicate.

`origQty = scale(amount_initial)`, `executedQty = scale(amount_initial - amount_remaining)`, `price = scale(price)`. Scaling uses `market_outcomes.price_precision` / `quantity_precision`. `timeInForce` is always `GTC`, `type` is always `LIMIT` in v1 (no other combinations are produced by the order-placement path).

### Time fields

`time` and `updateTime` come from `live_orders.chain_created_at` / `chain_updated_at`, not from DB bookkeeping columns. Rationale: DB `created_at` / `updated_at` drift from real chain time during indexer backlog, which is observable to clients and would make pagination cursors non-monotonic across replays.

### Pagination

Cursor-based on `(chain_created_at, order_id, orderbook_address)` with strict `>` comparison. The sort key is monotonic:

- `chain_created_at` never moves backward for a given row (`OrderPlaced` sets it once; subsequent events only update `chain_updated_at`).
- `order_id` is unique per `orderbook_address` and stable for the life of the row.
- `orderbook_address` is the unique tie-breaker for the all-markets variant. `(chain_created_at, order_id)` alone is not globally unique because `order_id` numbering is per-orderbook; two open orders on different books with the same chain second and identical `order_id` would otherwise have the strict `>` predicate filter out the tied row on the next page.

Consequence: between two paginated reads, an order that closes simply disappears from later pages; no duplication or skipping is possible.

#### Cursor encoding

The cursor is base64url-encoded JSON:

```json
{"t": <chain_created_at_us:i64>, "o": "<order_id:string>", "b": "<orderbook_address:string>"}
```

`t` is unix-microseconds — matching `live_orders.chain_created_at`'s native `timestamptz` precision. The API renders `time` / `updateTime` as milliseconds by truncating `us / 1_000` at the response boundary; using milliseconds inside the cursor would round past sub-millisecond chain timestamps and let the strict-`>` next-page predicate return the boundary row again. `o` is the decimal string form of the chain-side order id (matches the public `orderId`). `b` is the orderbook contract address that carried the order (the internal `live_orders.orderbook_address`); it is part of the cursor only to disambiguate ties and is never returned to the client outside the opaque cursor blob.

Decoding is strict: invalid base64, unparseable JSON, missing field, wrong field type, non-decimal `o`, empty `b`, or a `t` outside `[0, 8e18]` → `DomainError::MissingParameter` → `-1102` / 400. A well-formed cursor whose `(t, o, b)` triple lies past the last currently-open row is not an error; the SQL `WHERE (chain_created_at, order_id, orderbook_address) > (to_timestamp($t::bigint / 1_000_000.0), $o::numeric, $b::text)` simply returns zero rows and `next_cursor` is `null`.

#### Page-size protocol

- `limit` defaults to `100` when omitted.
- Valid range is `[1, 500]`. Out-of-range → `-1102` / 400.
- The SQL fetches `LIMIT $limit + 1`. If `$limit + 1` rows return, the last row is dropped from the response and `next_cursor` is built from the row that *remains* at position `$limit` (the last kept row); otherwise `next_cursor` is `null`. The `+1` lookahead is the only way the server distinguishes "exactly `$limit` rows left" from "more available", and building the cursor from the last kept row ensures the sentinel row reappears as the first row of the next page (strict `>` predicate against a fully-included row, never against one that was hidden).

### Auth & permissions

`USER_DATA`. Handled by the existing auth hoop:

- `-1003` / 401 for missing or unparseable envelope (`X-DODEX-APIKEY`, `timestamp`, `signature`, `recvWindow`).
- `-1002` / 401 for unknown / disabled key.
- `-2015` / 403 for a key without the `USER_DATA` permission.

The handler reads `ctx` via `require_auth(depot, Permission::UserData)` and uses `ctx.trading_pn.pn_address` as the `owner_pn_address` filter. No additional permission logic.

### Error mapping

| Condition | `DomainError` | API code | HTTP |
| --- | --- | --- | --- |
| Exactly one of `marketAddress` / `symbol` present | `MissingParameter` | `-1102` | 400 |
| `limit` out of `[1, 500]` | `MissingParameter` | `-1102` | 400 |
| `cursor` fails to decode | `MissingParameter` | `-1102` | 400 |
| Pair not found, or its market is unreconciled | `InvalidMarketOrSymbol` | `-1121` | 404 |
| Missing / invalid signature / API key / timestamp | upstream auth | `-1003` | 401 |
| Missing `USER_DATA` permission | upstream auth | `-2015` | 403 |
| Unexpected (DB / decode / etc.) | `Unexpected` | `-1000` | 500 |

Cursor-decode failures collapse into `-1102` deliberately — the cursor format is server-internal.

### SQL

Both variants share the projection list and the index-aligned predicate `owner_pn_address = $1 AND status = 'OPEN' AND amount_remaining > 0 AND chain_created_at IS NOT NULL AND chain_updated_at IS NOT NULL`, which matches the partial index `live_orders_open_owner_idx`. The all-markets variant is selected when no `(orderbook_address, outcome_id)` was resolved; the filtered variant appends the per-outcome predicate. The chain-timestamp non-NULL clauses defend against a rare ingestion path where the GraphQL gateway omits `created_at` on an edge — such rows have `chain_created_at` NULL after projection and must not surface in the endpoint (the response decoder would otherwise fail to map NULL into `i64`).

Common projection (pseudo-SQL — full text lives in the implementation):

```sql
select m.pmp_address                                                       as market_address,
       mo.symbol                                                           as symbol,
       lo.order_id::text                                                   as order_id,
       coalesce(lo.client_order_id, '')                                    as client_order_id,
       lo.price::text                                                      as price,
       lo.amount_initial::text                                             as orig_qty,
       greatest(lo.amount_initial - lo.amount_remaining, 0)::text          as executed_qty,
       lo.is_buy                                                           as is_buy,
       (extract(epoch from lo.chain_created_at) * 1000000)::bigint         as chain_created_at_us,
       (extract(epoch from lo.chain_updated_at) * 1000000)::bigint         as chain_updated_at_us,
       mo.price_precision                                                  as price_precision,
       mo.quantity_precision                                               as quantity_precision
  from live_orders lo
  join markets m         on m.orderbook_address = lo.orderbook_address
  join market_outcomes mo on mo.market_id_fk = m.id and mo.outcome_id = lo.outcome_id
 where lo.owner_pn_address = $1
   and lo.status = 'OPEN'
   and lo.amount_remaining > 0
   and lo.chain_created_at is not null
   and lo.chain_updated_at is not null
   and m.last_reconciled_at is not null
   /* filtered variant only: */
   /* and lo.orderbook_address = $2 and lo.outcome_id = $3 */
   /* if cursor present: */
   /* and (lo.chain_created_at, lo.order_id, lo.orderbook_address)
          > (to_timestamp($t::bigint / 1_000_000.0), $o::numeric, $b::text) */
 order by lo.chain_created_at asc, lo.order_id asc, lo.orderbook_address asc
 limit $limit + 1;
```

The filtered variant pre-resolves `(orderbook_address, outcome_id)` via a separate query against `markets ⨝ market_outcomes`. That query is also gated by `last_reconciled_at IS NOT NULL`.

### Index reliance

The owner-scoped path relies on the partial index added by migration `0018_live_orders_owner_open_orders.sql`:

```sql
create index if not exists live_orders_open_owner_idx
    on live_orders (owner_pn_address, chain_created_at, order_id)
    where owner_pn_address is not null
      and status = 'OPEN'
      and amount_remaining > 0
      and chain_created_at is not null
      and chain_updated_at is not null;
```

The index is partial so it contains only rows the endpoint can return. Cursor lookups become a direct range scan on the index. The filtered variant adds `orderbook_address = $X AND outcome_id = $Y` as a heap filter on top of the index range; cardinalities per-owner per-pair are expected in the tens, so a heap filter is cheap relative to maintaining a wider composite index.

### Code touch-points

| Layer | File | Change |
| --- | --- | --- |
| Domain | `crates/domain/src/lib.rs` | `OpenOrder`, `OpenOrderStatus`, `TimeInForce`, `OrderType`, `OrderSide` (unchanged from current WIP). |
| Application | `crates/application/src/lib.rs` | `OpenOrdersQuery { owner_pn_address, market, limit, cursor }`, `OpenOrdersCursor { chain_created_at_us, order_id, orderbook_address }`, `OpenOrdersPage { orders, next_cursor }`. `MarketReadRepository::list_open_orders` returns `OpenOrdersPage`. `GetOpenOrdersUseCase::execute(ctx, market_address, symbol, limit, cursor)` validates the pairing, the limit range, and decodes the cursor. |
| Infrastructure | `crates/infrastructure/src/postgres_repo.rs` | Two SQL variants (filtered / all-markets) with the cursor predicate and `LIMIT $limit + 1`. Cursor base64url JSON codec. `open_order_from_row` reads `chain_created_at` / `chain_updated_at`. |
| Indexer | `crates/infrastructure/src/projectors.rs` | `apply_order_placed` writes `chain_created_at = node_unix_seconds(node)` on insert with `least(...)` on conflict; `chain_updated_at = greatest(...)`. `apply_order_filled` and `apply_order_cancelled` advance `chain_updated_at = greatest(chain_updated_at, $node_ts)`. `apply_order_placed_confirmed` guards with `owner_pn_address IS NULL` for idempotency; on zero rows updated, distinguish "row missing → Deferred" from "already attributed → Applied" via one follow-up `select`. Does not advance `chain_updated_at`. |
| HTTP | `services/api/src/lib.rs` | `get_open_orders` reads `limit` (`req.query::<u16>`) and `cursor` (`non_empty_query`); passes them into the use case; returns `Json<OpenOrdersPageResponse>` with `{ orders: [...], nextCursor }` (camelCase). Router registration unchanged. |
| Schema | `migrations/0018_live_orders_owner_open_orders.sql` | Adds `owner_pn_address`, `amount_initial`, `chain_created_at`, `chain_updated_at`; backfills `amount_initial`; creates the partial index above. |
| Docs | `docs/tech-specs/data-schema.md`, `docs/tech-specs/market-data-indexer.md` | New columns and index documented; `PrivateNote.OrderPlacedConfirmed` row added to the projection table; note that `chain_updated_at` drives API `updateTime`. |

### Visibility / eventual consistency

Between `OrderBook.OrderPlaced` and `PrivateNote.OrderPlacedConfirmed`, the row exists in `live_orders` with `owner_pn_address = NULL`. The partial index excludes NULL owners, so the row contributes to public depth but cannot appear in `/api/v1/openOrders`. The confirmation event projector attaches the owner; if it arrives first, it is deferred and replayed once the OrderBook row exists (existing `Deferred → Applied` reprojection mechanism). This window is exposed to clients as an eventual-consistency note in `api-spec.md`; no other mitigation is provided in v1.

### Test coverage

Three integration suites, all gated on `TEST_DATABASE_URL`:

- `crates/infrastructure/tests/open_orders.rs` — owner scoping, sorting, scaling, the three filter shapes, cursor advance, cursor stability under concurrent fills, `limit` defaults and bounds, invalid cursor.
- `crates/infrastructure/tests/reprojection.rs` — deferred replay of `OrderPlacedConfirmed`, idempotency when already attributed, chain timestamps written by `OrderPlaced`.
- `services/api/tests/open_orders_http.rs` — happy path through the production router with the wrapped response, the three error codes (`-1102`, `-1121`, auth), and the pagination round-trip.

## `/api/v1/allOrders` (placeholder)

Not implemented yet. When added it will reuse `live_orders` plus the closed/filled rows the same table already retains (`status IN ('FILLED', 'CANCELLED')`) and a separate index. Time and status filters from the public spec will translate to additional predicates on `chain_updated_at` and `status`.
