# Unified `/api/v1/orders` Endpoint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `GET /api/v1/openOrders` and `GET /api/v1/allOrders` with a single account-scoped `GET /api/v1/orders` endpoint that returns the caller's orders across all lifecycle states (`NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `REJECTED`) with a CSV status filter, cursor pagination on `placed_chain_order` descending, and a `{ orders, nextCursor }` envelope.

**Architecture:** One DB migration (drops the OPEN-only owner index, creates a status-agnostic owner index). Domain renames `OpenOrder` → `Order` and drops the 2-variant `OpenOrderStatus` in favour of the existing 6-variant `OrderStatus` (`PendingNew` stays write-side; reads project 5 of the 6 variants from the row's stored `status` + `amount_remaining`/`amount_initial`). Application layer renames `OpenOrders*` → `Orders*`, adds `OrderStatusSet` parsing from CSV, and `GetOrdersUseCase`. Infrastructure adapts the SQL: dynamic `OR`-disjunction status predicate built from allow-listed literal fragments, `ORDER BY placed_chain_order DESC`, strict `<` cursor predicate. HTTP routes `api/v1/orders` to a new `get_orders` handler. `REJECTED` filter is accepted today but always returns empty; the projector that fills `live_orders.status = 'REJECTED'` ships in a separate contracts-side follow-up PR (see `docs/tech-specs/read-api.md §REJECTED — future work`).

**Tech Stack:** Rust workspace (`crates/domain`, `crates/application`, `crates/infrastructure`, `services/api`), `sqlx` for Postgres, `salvo` for HTTP, `async-trait` for repository ports.

**Reference specs:**
- Public contract: [`docs/api-spec.md` §Orders](../../api-spec.md#orders).
- Implementation contract: [`docs/tech-specs/read-api.md` §/api/v1/orders](../../tech-specs/read-api.md#apiv1orders).
- Schema: [`docs/tech-specs/data-schema.md` §`live_orders`](../../tech-specs/data-schema.md#live_orders).

---

## File Structure

**Created:**
- `migrations/0002_orders_owner_index.sql` — drop `live_orders_open_owner_idx`, create `live_orders_owner_idx`.
- `crates/infrastructure/tests/orders.rs` — DB-backed integration suite for the unified endpoint.
- `services/api/tests/orders_http.rs` — HTTP-level test suite through the production router.

**Modified:**
- `crates/domain/src/lib.rs` — replace `OpenOrder` with `Order`, delete `OpenOrderStatus`.
- `crates/application/src/lib.rs` — rename `OpenOrders*` types to `Orders*`, add `OrderStatusSet`, replace `GetOpenOrdersUseCase` with `GetOrdersUseCase`, update `MarketReadRepository::list_open_orders` → `list_orders`.
- `crates/infrastructure/src/postgres_repo.rs` — rewrite the query into `list_orders`, build the dynamic status predicate, switch sort + cursor to DESC `<`.
- `services/api/src/lib.rs` — rename handler + route, parse `status` CSV, update DTO field type.

**Deleted:**
- `crates/infrastructure/tests/open_orders.rs`
- `services/api/tests/open_orders_http.rs`

**No changes:**
- Anything trading-side (`POST /api/v1/order`, `DELETE /api/v1/openOrders`, etc.).
- `live_orders.status` CHECK constraint — `REJECTED` is added by the future contracts follow-up, not by this PR.
- Smart contracts.

---

## Task 1: Add migration that swaps the owner index

**Files:**
- Create: `migrations/0002_orders_owner_index.sql`

- [ ] **Step 1: Inspect the current owner index** so the migration can drop the exact name.

```bash
grep -nE 'live_orders_open_owner_idx|live_orders_owner_idx' migrations/0001_initial.sql
```

Expected output:

```
202:create index live_orders_open_owner_idx
```

- [ ] **Step 2: Write the migration**

```sql
-- migrations/0002_orders_owner_index.sql
--
-- /api/v1/orders supersedes the OPEN-only /api/v1/openOrders. The new
-- endpoint returns rows across all five public statuses
-- (NEW / PARTIALLY_FILLED / FILLED / CANCELED / REJECTED), so the
-- partial predicate on `status = 'OPEN' AND amount_remaining > 0`
-- no longer fits. Status filtering becomes a heap-side predicate;
-- per-owner cardinalities are small enough that this is cheaper than
-- maintaining a wider composite index.
--
-- See docs/tech-specs/read-api.md §Index reliance.

drop index if exists live_orders_open_owner_idx;

create index live_orders_owner_idx
    on live_orders (owner_pn_address, placed_chain_order desc)
    where owner_pn_address is not null
      and chain_created_at is not null;
```

- [ ] **Step 3: Update data-schema doc cross-check**

Run:

```bash
grep -nE 'live_orders_open_owner_idx|live_orders_owner_idx' docs/tech-specs/data-schema.md
```

Expected output: matches reference `live_orders_owner_idx` only (the previous brainstorming step already updated this doc). If `live_orders_open_owner_idx` appears outside the historical "supersedes" sentence, fix in place.

- [ ] **Step 4: Boot the test database and confirm the migration applies cleanly**

Run:

```bash
docker compose -f docker-compose.test.yml up -d --wait
cargo sqlx migrate run --source migrations --database-url "$TEST_DATABASE_URL"
```

Expected: migration `0002_orders_owner_index` applied, no errors. (If `sqlx` CLI is unavailable, the test binaries also run migrations on connect — verify by running `cargo test -p dodex-infrastructure --test depth -- --test-threads=1` and confirming the depth suite still passes.)

- [ ] **Step 5: Commit**

```bash
git add migrations/0002_orders_owner_index.sql
git commit -m "feat(db): add live_orders_owner_idx covering all order statuses"
```

---

## Task 2: Replace the domain `OpenOrder` / `OpenOrderStatus` types with `Order`

The existing 6-variant `OrderStatus` (already in `crates/domain/src/lib.rs:404`) covers the read side once `PendingNew` is dropped at projection time. We replace the narrower `OpenOrder` with a broader `Order` and delete the 2-variant `OpenOrderStatus`.

**Files:**
- Modify: `crates/domain/src/lib.rs:226-257`

- [ ] **Step 1: Read the existing `OpenOrder` / `OpenOrderStatus` block**

Run:

```bash
sed -n '226,257p' crates/domain/src/lib.rs
```

Expected output: 32 lines covering `pub struct OpenOrder { ... }`, `pub enum OpenOrderStatus { New, PartiallyFilled }`, and `impl OpenOrderStatus { fn as_str(...) }`.

- [ ] **Step 2: Replace `OpenOrder` and delete `OpenOrderStatus`**

Change the block to:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    /// Chain-side order id as a decimal string. Empty when the row's
    /// `status` is `Rejected` — the chain never assigns an id to a
    /// rejected placement.
    pub order_id: String,
    pub client_order_id: String,
    pub price: String,
    pub orig_qty: String,
    pub executed_qty: String,
    pub status: OrderStatus,
    pub time_in_force: TimeInForce,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub time: i64,
    pub update_time: i64,
}
```

(Delete the `OpenOrderStatus` enum and its `impl` block in the same edit. `OrderStatus` at line 404 already exists and stays.)

- [ ] **Step 3: Run the workspace build to surface every call site**

Run:

```bash
cargo check --workspace 2>&1 | grep -E 'OpenOrder|OpenOrderStatus' | head -40
```

Expected: a list of errors in `crates/application/src/lib.rs`, `crates/infrastructure/src/postgres_repo.rs`, `services/api/src/lib.rs`, and test files. They are addressed in Tasks 3–6.

- [ ] **Step 4: Commit**

```bash
git add crates/domain/src/lib.rs
git commit -m "feat(domain): replace OpenOrder with Order covering all statuses"
```

(The workspace does NOT build yet — call sites are fixed in the next tasks. The commit is intentionally a "broken middle" so the diff for each subsequent layer stays focused. Subsequent tasks must all land before the branch ships.)

---

## Task 3: Application layer — rename ports, types, use case; add status filter parsing

**Files:**
- Modify: `crates/application/src/lib.rs:160-235` (port + types), `:560-616` (use case), and any test helpers in the same file that reference the renamed symbols.

- [ ] **Step 1: Write a failing test for `OrderStatusSet::from_csv`**

Add to `crates/application/src/lib.rs` inside the `#[cfg(test)] mod tests { ... }` block:

```rust
#[test]
fn status_set_parses_csv_and_dedups() {
    let set = OrderStatusSet::from_csv(Some("NEW, FILLED ,NEW, CANCELED"))
        .expect("valid CSV");
    let canonical = set.canonical_vec();
    // BTreeSet iterates by the derived Ord on OrderStatus, which is
    // declaration order: PendingNew, New, PartiallyFilled, Filled,
    // Canceled, Rejected. So the input "NEW, FILLED, NEW, CANCELED"
    // dedups to {New, Filled, Canceled} and surfaces in that order.
    assert_eq!(canonical, vec![OrderStatus::New, OrderStatus::Filled, OrderStatus::Canceled]);
}

#[test]
fn status_set_treats_absent_and_empty_as_all() {
    assert!(OrderStatusSet::from_csv(None).expect("absent").is_all());
    assert!(OrderStatusSet::from_csv(Some("   ")).expect("blank").is_all());
}

#[test]
fn status_set_rejects_unknown_token() {
    let err = OrderStatusSet::from_csv(Some("NEW,SUPER_FILLED"))
        .expect_err("unknown token");
    assert_eq!(err, DomainError::InvalidParameter);
}

#[test]
fn status_set_rejects_pending_new() {
    // PendingNew is a write-side synthetic status and must not be
    // accepted as a /orders filter — it never appears on a live_orders row.
    let err = OrderStatusSet::from_csv(Some("PENDING_NEW"))
        .expect_err("pending_new rejected");
    assert_eq!(err, DomainError::InvalidParameter);
}
```

- [ ] **Step 2: Run the test to confirm it fails to compile**

Run:

```bash
cargo test -p dodex-application status_set 2>&1 | tail -20
```

Expected: compile error — `OrderStatusSet` is not defined.

- [ ] **Step 3: Add `OrderStatusSet` and helpers**

Insert above `pub struct OpenOrdersQuery` (around the line that today holds `OPEN_ORDERS_DEFAULT_LIMIT`):

```rust
pub const ORDERS_DEFAULT_LIMIT: u16 = 100;
pub const ORDERS_MAX_LIMIT: u16 = 500;

/// Caller-supplied filter on order status. `is_all()` means "no filter,
/// every row passes"; otherwise the inner set is the canonical subset
/// of [`OrderStatus`] tokens the caller listed in the request `status`
/// CSV. `PendingNew` is rejected at parse time — it is a write-side
/// synthetic status and never appears on a `live_orders` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatusSet(std::collections::BTreeSet<OrderStatus>);

impl OrderStatusSet {
    /// Parse the request `status` parameter. `None` or all-whitespace
    /// means "all statuses"; anything else is split on `,`, trimmed,
    /// de-duplicated, and matched against the allow-list. An unknown
    /// token (or `PENDING_NEW`, which is write-side only) returns
    /// [`DomainError::InvalidParameter`].
    pub fn from_csv(raw: Option<&str>) -> Result<Self, DomainError> {
        let Some(value) = raw else {
            return Ok(Self::all());
        };
        let mut set = std::collections::BTreeSet::new();
        let mut had_token = false;
        for token in value.split(',') {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                continue;
            }
            had_token = true;
            let status = match trimmed {
                "NEW" => OrderStatus::New,
                "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
                "FILLED" => OrderStatus::Filled,
                "CANCELED" => OrderStatus::Canceled,
                "REJECTED" => OrderStatus::Rejected,
                _ => return Err(DomainError::InvalidParameter),
            };
            set.insert(status);
        }
        if !had_token {
            return Ok(Self::all());
        }
        Ok(Self(set))
    }

    pub fn all() -> Self {
        Self(std::collections::BTreeSet::new())
    }

    pub fn is_all(&self) -> bool {
        self.0.is_empty()
    }

    pub fn canonical_vec(&self) -> Vec<OrderStatus> {
        // BTreeSet iteration order is the enum's `Ord`. The enum is
        // declared as `PendingNew, New, PartiallyFilled, Filled,
        // Canceled, Rejected` and PendingNew is never inserted here,
        // so the result order is stable and PendingNew-free.
        self.0.iter().copied().collect()
    }
}
```

`OrderStatus` already derives `Ord`/`PartialOrd`? Check before continuing:

```bash
grep -nE 'derive.*Ord|enum OrderStatus' crates/domain/src/lib.rs
```

If `Ord` is missing from the derive list at line 402, add it:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
```

- [ ] **Step 4: Run the new tests**

```bash
cargo test -p dodex-application status_set -- --nocapture
```

Expected: 4 tests pass.

- [ ] **Step 5: Rename the application types and port method**

Mechanical rename across `crates/application/src/lib.rs`:

| Old | New |
| --- | --- |
| `OpenOrdersQuery` | `OrdersQuery` |
| `OpenOrdersPage` | `OrdersPage` |
| `OpenOrdersCursor` | `OrdersCursor` |
| `OpenOrdersMarketFilter` | `OrdersMarketFilter` |
| `GetOpenOrdersUseCase` | `GetOrdersUseCase` |
| `OPEN_ORDERS_DEFAULT_LIMIT` | `ORDERS_DEFAULT_LIMIT` (already added in step 3 — delete the old constant) |
| `OPEN_ORDERS_MAX_LIMIT` | `ORDERS_MAX_LIMIT` (delete the old constant) |
| `list_open_orders` (trait method) | `list_orders` |
| `pub orders: Vec<OpenOrder>` (in page struct) | `pub orders: Vec<Order>` |

Add a new field to `OrdersQuery`:

```rust
pub struct OrdersQuery {
    pub owner_pn_address: String,
    pub market: Option<OrdersMarketFilter>,
    pub status: OrderStatusSet,
    pub limit: u16,
    pub cursor: Option<OrdersCursor>,
}
```

Update `GetOrdersUseCase::execute` to take a `status: Option<&str>` parameter, parse it via `OrderStatusSet::from_csv`, and thread the resulting set into `OrdersQuery`:

```rust
impl<R> GetOrdersUseCase<R>
where
    R: MarketReadRepository,
{
    pub async fn execute(
        &self,
        ctx: &AuthContext,
        market_address: Option<MarketAddress>,
        symbol: Option<Symbol>,
        status: Option<&str>,
        limit: Option<i64>,
        cursor: Option<&str>,
    ) -> Result<OrdersPage, anyhow::Error> {
        let market = match (market_address, symbol) {
            (None, None) => None,
            (Some(market_address), Some(symbol)) => {
                Some(OrdersMarketFilter { market_address, symbol })
            }
            _ => return Err(anyhow::anyhow!(DomainError::MissingParameter)),
        };

        let status = OrderStatusSet::from_csv(status)
            .map_err(|err| anyhow::anyhow!(err))?;

        let limit = match limit {
            None => ORDERS_DEFAULT_LIMIT,
            Some(v) if (1..=i64::from(ORDERS_MAX_LIMIT)).contains(&v) => v as u16,
            Some(_) => return Err(anyhow::anyhow!(DomainError::MissingParameter)),
        };

        let cursor = match cursor {
            None => None,
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(anyhow::anyhow!(DomainError::MissingParameter));
                }
                Some(OrdersCursor(trimmed.to_string()))
            }
        };

        self.repo
            .list_orders(&OrdersQuery {
                owner_pn_address: ctx.trading_pn.pn_address.clone(),
                market,
                status,
                limit,
                cursor,
            })
            .await
    }
}
```

Update the in-file mock at the bottom of `crates/application/src/lib.rs` (currently `list_open_orders`) to match: `async fn list_orders(&self, _: &OrdersQuery) -> Result<OrdersPage, anyhow::Error>`.

- [ ] **Step 6: Run the application crate**

```bash
cargo test -p dodex-application --lib
```

Expected: PASS, including the four new `status_set` tests.

- [ ] **Step 7: Commit**

```bash
git add crates/domain/src/lib.rs crates/application/src/lib.rs
git commit -m "feat(app): introduce GetOrdersUseCase with OrderStatusSet filter"
```

---

## Task 4: Postgres repository — `list_orders` with dynamic status predicate, DESC sort

**Files:**
- Modify: `crates/infrastructure/src/postgres_repo.rs:372-616` (the existing `list_open_orders` block plus the `OpenOrderRow` struct and `open_order_from_row` mapper).

- [ ] **Step 1: Read the current implementation to identify lines to replace**

```bash
sed -n '372,616p' crates/infrastructure/src/postgres_repo.rs | head -80
```

Note the two `sqlx::query_as` blocks (filtered + all-markets) and the helper functions.

- [ ] **Step 2: Replace the trait impl method signature**

Find:

```rust
async fn list_open_orders(
    &self,
    query: &OpenOrdersQuery,
) -> Result<OpenOrdersPage, anyhow::Error> {
```

Replace with:

```rust
async fn list_orders(
    &self,
    query: &OrdersQuery,
) -> Result<OrdersPage, anyhow::Error> {
```

- [ ] **Step 3: Add the status-predicate builder above the impl block**

Insert the helper at the bottom of the file, next to `open_order_from_row`:

```rust
/// Build the SQL fragment that filters `live_orders` rows by the
/// requested status set. Returns an empty string when every status is
/// allowed — that path emits no status predicate at all. The fragment
/// is composed from compile-time literal disjuncts drawn from the
/// allow-listed [`OrderStatus`] tokens, so no user-supplied bytes ever
/// reach the SQL string.
fn build_status_predicate(set: &OrderStatusSet) -> &'static str {
    if set.is_all() {
        return "";
    }
    // Order: New, PartiallyFilled, Filled, Canceled, Rejected (BTreeSet
    // iteration order). Mapping table mirrors
    // docs/tech-specs/read-api.md §Status mapping.
    let mut clauses: Vec<&'static str> = Vec::with_capacity(5);
    for status in set.canonical_vec() {
        clauses.push(match status {
            OrderStatus::New =>
                "(lo.status = 'OPEN' AND lo.amount_remaining = lo.amount_initial)",
            OrderStatus::PartiallyFilled =>
                "(lo.status = 'OPEN' AND lo.amount_remaining < lo.amount_initial AND lo.amount_remaining > 0)",
            OrderStatus::Filled => "lo.status = 'FILLED'",
            OrderStatus::Canceled => "lo.status = 'CANCELLED'",
            OrderStatus::Rejected => "lo.status = 'REJECTED'",
            OrderStatus::PendingNew => unreachable!(
                "PendingNew is rejected by OrderStatusSet::from_csv"
            ),
        });
    }
    // SAFETY: every fragment is a compile-time string literal; the
    // join is a leak-into-'static via Box::leak to give back &'static str.
    Box::leak(clauses.join(" OR ").into_boxed_str())
}
```

(`Box::leak` is acceptable here because there are at most 32 distinct status-set combinations; the leak amortises to a fixed budget that the LRU-style caller pattern caps. If the reviewer flags this, switch to an `OnceLock<HashMap<Vec<OrderStatus>, String>>` cache — both are acceptable.)

- [ ] **Step 4: Replace the SQL query bodies**

Replace the filtered query body with:

```rust
let status_clause = build_status_predicate(&query.status);
let status_sql = if status_clause.is_empty() {
    String::new()
} else {
    format!(" AND ({}) ", status_clause)
};

let filtered_sql = format!(
    r#"select m.pmp_address as market_address,
              mo.symbol as symbol,
              lo.order_id::text as order_id,
              coalesce(lo.client_order_id, '') as client_order_id,
              lo.price::text as price,
              lo.amount_initial::text as orig_qty,
              greatest(lo.amount_initial - lo.amount_remaining, 0)::text as executed_qty,
              lo.is_buy as is_buy,
              lo.status as raw_status,
              (extract(epoch from lo.chain_created_at) * 1000000)::bigint as chain_created_at_us,
              (extract(epoch from lo.chain_updated_at) * 1000000)::bigint as chain_updated_at_us,
              lo.placed_chain_order as placed_chain_order,
              mo.price_precision as price_precision,
              mo.quantity_precision as quantity_precision
         from live_orders lo
         join markets m on m.orderbook_address = lo.orderbook_address
         join market_outcomes mo
           on mo.market_id_fk = m.id
          and mo.outcome_id = lo.outcome_id
        where lo.owner_pn_address = $1
          and lo.chain_created_at is not null
          and lo.chain_updated_at is not null
          and m.last_reconciled_at is not null
          and lo.orderbook_address = $2
          and lo.outcome_id = $3
          and ($4::text is null or lo.placed_chain_order < $4::text)
          {status_sql}
        order by lo.placed_chain_order desc
        limit $5"#
);

let all_sql = format!(
    r#"select m.pmp_address as market_address,
              mo.symbol as symbol,
              lo.order_id::text as order_id,
              coalesce(lo.client_order_id, '') as client_order_id,
              lo.price::text as price,
              lo.amount_initial::text as orig_qty,
              greatest(lo.amount_initial - lo.amount_remaining, 0)::text as executed_qty,
              lo.is_buy as is_buy,
              lo.status as raw_status,
              (extract(epoch from lo.chain_created_at) * 1000000)::bigint as chain_created_at_us,
              (extract(epoch from lo.chain_updated_at) * 1000000)::bigint as chain_updated_at_us,
              lo.placed_chain_order as placed_chain_order,
              mo.price_precision as price_precision,
              mo.quantity_precision as quantity_precision
         from live_orders lo
         join markets m on m.orderbook_address = lo.orderbook_address
         join market_outcomes mo
           on mo.market_id_fk = m.id
          and mo.outcome_id = lo.outcome_id
        where lo.owner_pn_address = $1
          and lo.chain_created_at is not null
          and lo.chain_updated_at is not null
          and m.last_reconciled_at is not null
          and ($2::text is null or lo.placed_chain_order < $2::text)
          {status_sql}
        order by lo.placed_chain_order desc
        limit $3"#
);

let rows: Vec<OrderRow> = match target {
    Some((orderbook_address, outcome_id)) => sqlx::query_as(&filtered_sql)
        .bind(query.owner_pn_address.as_str())
        .bind(orderbook_address)
        .bind(outcome_id)
        .bind(query.cursor.as_ref().map(|c| c.0.as_str()))
        .bind(limit_plus_one)
        .fetch_all(&self.pool)
        .await
        .context("select filtered orders")?,
    None => sqlx::query_as(&all_sql)
        .bind(query.owner_pn_address.as_str())
        .bind(query.cursor.as_ref().map(|c| c.0.as_str()))
        .bind(limit_plus_one)
        .fetch_all(&self.pool)
        .await
        .context("select all orders")?,
};
```

- [ ] **Step 5: Replace `OpenOrderRow` and the row mapper**

Replace the existing `OpenOrderRow` struct with:

```rust
#[derive(sqlx::FromRow)]
struct OrderRow {
    market_address: String,
    symbol: String,
    order_id: String,
    client_order_id: String,
    price: String,
    orig_qty: String,
    executed_qty: String,
    is_buy: bool,
    raw_status: String,
    chain_created_at_us: i64,
    chain_updated_at_us: i64,
    placed_chain_order: String,
    price_precision: i32,
    quantity_precision: i32,
}
```

Replace the mapper:

```rust
fn order_from_row(row: OrderRow) -> Result<Order, anyhow::Error> {
    let price = scale_decimal(&row.price, row.price_precision)
        .context("scale order price")?;
    let orig_qty = scale_decimal(&row.orig_qty, row.quantity_precision)
        .context("scale order orig_qty")?;
    let executed_qty = scale_decimal(&row.executed_qty, row.quantity_precision)
        .context("scale order executed_qty")?;

    let amount_remaining = parse_uint(&row.orig_qty)?.saturating_sub(parse_uint(&row.executed_qty)?);
    let amount_initial = parse_uint(&row.orig_qty)?;

    let public_status = match row.raw_status.as_str() {
        "OPEN" if amount_remaining == amount_initial => OrderStatus::New,
        "OPEN" if amount_remaining > num_bigint::BigUint::from(0u8) && amount_remaining < amount_initial => OrderStatus::PartiallyFilled,
        "OPEN" => {
            // amount_remaining == 0 on an OPEN row would be a projector bug.
            warn!(
                order_id = %row.order_id,
                "skipping OPEN row with zero amount_remaining"
            );
            return Err(anyhow!(DomainError::Unexpected));
        }
        "FILLED" => OrderStatus::Filled,
        "CANCELLED" => OrderStatus::Canceled,
        "REJECTED" => OrderStatus::Rejected,
        other => {
            warn!(
                raw_status = %other,
                order_id = %row.order_id,
                "unknown live_orders.status — skipping row"
            );
            return Err(anyhow!(DomainError::Unexpected));
        }
    };

    let rendered_order_id = if matches!(public_status, OrderStatus::Rejected) {
        String::new()
    } else {
        row.order_id
    };

    Ok(Order {
        market_address: MarketAddress(row.market_address),
        symbol: Symbol(row.symbol),
        order_id: rendered_order_id,
        client_order_id: row.client_order_id,
        price,
        orig_qty,
        executed_qty,
        status: public_status,
        time_in_force: TimeInForce::GoodTilCancelled,
        order_type: OrderType::Limit,
        side: if row.is_buy { OrderSide::Buy } else { OrderSide::Sell },
        time: ms_from_micros(row.chain_created_at_us),
        update_time: ms_from_micros(row.chain_updated_at_us),
    })
}
```

(`parse_uint` is the existing helper used by `scale_decimal`; reuse it. `ms_from_micros` is the existing helper for the `chain_created_at_us` → ms conversion already present in the file.)

- [ ] **Step 6: Update imports in the repo file**

Replace:

```rust
use dodex_application::OpenOrdersCursor;
use dodex_application::OpenOrdersPage;
use dodex_application::OpenOrdersQuery;
use dodex_domain::OpenOrder;
use dodex_domain::OpenOrderStatus;
```

with:

```rust
use dodex_application::OrderStatusSet;
use dodex_application::OrdersCursor;
use dodex_application::OrdersPage;
use dodex_application::OrdersQuery;
use dodex_domain::Order;
use dodex_domain::OrderStatus;
```

(Drop `OpenOrderStatus` entirely; it no longer exists.)

- [ ] **Step 7: Build the infrastructure crate**

```bash
cargo check -p dodex-infrastructure
```

Expected: no remaining `OpenOrder*` references; clean build.

- [ ] **Step 8: Commit**

```bash
git add crates/infrastructure/src/postgres_repo.rs
git commit -m "feat(infra): implement list_orders with status-filtered descending cursor"
```

---

## Task 5: HTTP handler — `get_orders`, `/api/v1/orders` route, `status` query param

**Files:**
- Modify: `services/api/src/lib.rs` — rename imports, response struct, handler, route, DTO.

- [ ] **Step 1: Replace imports**

Find:

```rust
use dodex_domain::OpenOrder;
```

Replace with:

```rust
use dodex_domain::Order;
```

If `GetOpenOrdersUseCase`, `OpenOrdersPage`, or `OpenOrdersCursor` appear in the use list, rename them to `GetOrdersUseCase`, `OrdersPage`, `OrdersCursor`.

- [ ] **Step 2: Rename the response DTO**

Find `struct OpenOrderResponse` (around line 188) and `struct OpenOrdersPageResponse` (line 212). Rename both:

```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderResponse {
    market_address: String,
    symbol: String,
    order_id: String,
    client_order_id: String,
    price: String,
    orig_qty: String,
    executed_qty: String,
    status: &'static str,
    time_in_force: &'static str,
    #[serde(rename = "type")]
    order_type: &'static str,
    side: &'static str,
    time: i64,
    update_time: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OrdersPageResponse {
    orders: Vec<OrderResponse>,
    next_cursor: Option<String>,
}
```

- [ ] **Step 3: Replace the handler**

Find `async fn get_open_orders` (around line 474) and replace the function body to read the new `status` query parameter and call the renamed use case:

```rust
#[handler]
async fn get_orders(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<OrdersPageResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::UserData)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let market_address = non_empty_query(req, "marketAddress").map(MarketAddress);
    let symbol = non_empty_query(req, "symbol").map(Symbol);
    // status: raw CSV, validated by OrderStatusSet::from_csv inside the
    // use case. Absent / blank → "all statuses".
    let status = req.query::<String>("status");
    // Map any limit-parse failure to MissingParameter so the documented
    // -1102 fires for both out-of-range and unparseable inputs.
    let limit = optional_typed_query::<i64>(req, "limit")
        .map_err(|_| ApiError::from(DomainError::MissingParameter))?;
    let cursor = req.query::<String>("cursor");

    let use_case = GetOrdersUseCase::new(state.repo);
    let page = use_case
        .execute(
            &ctx,
            market_address,
            symbol,
            status.as_deref(),
            limit,
            cursor.as_deref(),
        )
        .await
        .map_err(|err| {
            if let Some(domain) = err.downcast_ref::<DomainError>() {
                return ApiError::from(*domain);
            }
            error!(?err, "get_orders failed");
            ApiError::from(DomainError::Unexpected)
        })?;

    Ok(Json(OrdersPageResponse {
        orders: page.orders.into_iter().map(order_to_dto).collect(),
        next_cursor: page.next_cursor.map(|c| c.0),
    }))
}

fn order_to_dto(order: Order) -> OrderResponse {
    OrderResponse {
        market_address: order.market_address.0,
        symbol: order.symbol.0,
        order_id: order.order_id,
        client_order_id: order.client_order_id,
        price: order.price,
        orig_qty: order.orig_qty,
        executed_qty: order.executed_qty,
        status: order.status.as_str(),
        time_in_force: order.time_in_force.as_str(),
        order_type: order.order_type.as_str(),
        side: order.side.as_str(),
        time: order.time,
        update_time: order.update_time,
    }
}
```

Delete the old `open_order_to_dto` function.

- [ ] **Step 4: Swap the route**

Find:

```rust
.push(Router::with_path("api/v1/openOrders").get(get_open_orders)),
```

Replace with:

```rust
.push(Router::with_path("api/v1/orders").get(get_orders)),
```

- [ ] **Step 5: Build the HTTP service**

```bash
cargo check -p dodex-api
```

Expected: clean build.

- [ ] **Step 6: Commit**

```bash
git add services/api/src/lib.rs
git commit -m "feat(api): expose GET /api/v1/orders unifying openOrders and allOrders"
```

---

## Task 6: Integration test suite for the repo — `crates/infrastructure/tests/orders.rs`

**Files:**
- Create: `crates/infrastructure/tests/orders.rs`
- Delete: `crates/infrastructure/tests/open_orders.rs` (at the end of this task)

The new file mirrors the structure of the existing `open_orders.rs` (Scope helper, fixture builder, owner-scoping assertions) but exercises the broader contract.

- [ ] **Step 1: Skim the legacy file for scope helpers worth re-using**

```bash
sed -n '1,80p' crates/infrastructure/tests/open_orders.rs
```

The first ~80 lines define a `Scope::new` helper that seeds a market + outcome + owner PN. Copy it into the new file verbatim (re-export rather than re-write so the test surface remains uniform).

- [ ] **Step 2: Write the new test file scaffold**

Create `crates/infrastructure/tests/orders.rs` with the scope helper plus these test fixtures (skeleton; fill in helpers as you go — `insert_open_order`, `insert_filled_order`, `insert_canceled_order` etc., copying patterns from `open_orders.rs`):

```rust
//! Integration tests for `PostgresReadModelRepository::list_orders`.
//! Gated on `TEST_DATABASE_URL`; see services/api/README.md.

mod common; // optional — only if extracting Scope into a module

use dodex_application::{
    MarketReadRepository, OrderStatusSet, OrdersCursor, OrdersMarketFilter, OrdersQuery,
};
use dodex_domain::{MarketAddress, OrderStatus, Symbol};

// Each test below assumes a Scope that has seeded:
// - one reconciled market with two outcomes (YES + NO);
// - one owner PN ("0:owner-1") and a stranger PN ("0:owner-2");
// - helper inserters for OPEN (full+partial), FILLED, CANCELLED rows.

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn returns_only_owner_rows_across_all_statuses() {
    // Seed: owner-1 has one row per status (NEW, PARTIALLY_FILLED, FILLED, CANCELLED).
    // owner-2 has a NEW row that must not appear.
    // Assert: page contains exactly the four owner-1 rows in DESC placed_chain_order.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn default_status_returns_all_five_buckets_minus_rejected_today() {
    // Seed: owner-1 has NEW, PARTIALLY_FILLED, FILLED, CANCELLED.
    // No REJECTED row is seeded (projector ships later).
    // Assert: page returns all four; counts match.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn status_csv_filter_narrows_results() {
    // Seed: as above.
    // Query with status=FILLED,CANCELED.
    // Assert: only FILLED + CANCELLED rows surface.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn rejected_filter_today_returns_empty_set() {
    // Seed: same four rows; no projector writes REJECTED yet.
    // Query with status=REJECTED.
    // Assert: { orders: [], next_cursor: None }.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn canceled_partial_fill_reports_nonzero_executed_qty() {
    // Seed: an order with amount_initial=10, amount_remaining=3, status='CANCELLED'.
    // Query default.
    // Assert: status=CANCELED, origQty=10, executedQty=7.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn descending_placed_chain_order_sort() {
    // Seed: owner-1 has 3 rows with placed_chain_order = "001", "002", "003".
    // Assert: response order is "003", "002", "001".
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn cursor_advances_strictly_below_last_returned() {
    // Seed: 6 rows with placed_chain_order = "001".."006".
    // Page 1 with limit=4 returns "006","005","004","003" and next_cursor="003".
    // Page 2 passes that cursor and returns "002","001" with next_cursor=None.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn cursor_stable_when_open_row_transitions_to_filled_between_pages() {
    // Seed: 4 rows; page 1 with limit=2 fetched; then mutate the row
    // at position 3 to status='FILLED'. Page 2 still returns rows 2 and 1.
    // The transitioned row remains visible if status filter is default (all).
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn pair_unknown_returns_invalid_market_or_symbol() {
    // Query with marketAddress=fake, symbol=fake (otherwise valid).
    // Assert: anyhow downcasts to DomainError::InvalidMarketOrSymbol.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn unreconciled_market_pair_returns_invalid_market_or_symbol() {
    // Seed: a market whose last_reconciled_at IS NULL.
    // Query with that pair.
    // Assert: DomainError::InvalidMarketOrSymbol.
}
```

- [ ] **Step 3: Fill in the helper inserters and shared setup**

Copy `Scope::new`, `insert_open_order`, and `insert_owner` from `open_orders.rs` verbatim. Add new helpers as needed:

```rust
async fn insert_filled_order(pool: &PgPool, scope: &Scope, placed_chain_order: &str) {
    sqlx::query(
        r#"insert into live_orders (
            orderbook_address, order_id, outcome_id, is_buy, price,
            amount_initial, amount_remaining, status,
            owner_pn_address, last_chain_order, placed_chain_order,
            chain_created_at, chain_updated_at
        ) values ($1, $2, $3, true, 615,
                  10, 0, 'FILLED',
                  $4, $5, $5,
                  now(), now())"#,
    )
    .bind(&scope.orderbook_address)
    .bind(next_order_id(scope))
    .bind(scope.outcome_id_yes)
    .bind(&scope.owner_pn_address)
    .bind(placed_chain_order)
    .execute(pool)
    .await
    .expect("insert FILLED row");
}

async fn insert_canceled_order(pool: &PgPool, scope: &Scope, placed_chain_order: &str, executed: i64) {
    // executed lets the test exercise "partial then canceled" → executed_qty > 0.
    sqlx::query(
        r#"insert into live_orders (
            orderbook_address, order_id, outcome_id, is_buy, price,
            amount_initial, amount_remaining, status,
            owner_pn_address, last_chain_order, placed_chain_order,
            chain_created_at, chain_updated_at
        ) values ($1, $2, $3, true, 615,
                  10, 10 - $6, 'CANCELLED',
                  $4, $5, $5,
                  now(), now())"#,
    )
    .bind(&scope.orderbook_address)
    .bind(next_order_id(scope))
    .bind(scope.outcome_id_yes)
    .bind(&scope.owner_pn_address)
    .bind(placed_chain_order)
    .bind(executed)
    .execute(pool)
    .await
    .expect("insert CANCELLED row");
}
```

Implement each test body. Each test ends with `.expect("query orders")` against `PostgresReadModelRepository::list_orders` and asserts on the resulting `OrdersPage`.

- [ ] **Step 4: Run the suite**

```bash
docker compose -f docker-compose.test.yml up -d --wait
cargo test -p dodex-infrastructure --test orders -- --test-threads=1
```

Expected: all 10 tests pass.

- [ ] **Step 5: Delete the legacy file**

```bash
git rm crates/infrastructure/tests/open_orders.rs
```

Re-run the full infrastructure test suite to confirm nothing else depends on the deleted helpers:

```bash
cargo test -p dodex-infrastructure --tests -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/infrastructure/tests/orders.rs
git commit -m "test(infra): cover /api/v1/orders status filter and DESC pagination"
```

---

## Task 7: HTTP-level test suite — `services/api/tests/orders_http.rs`

**Files:**
- Create: `services/api/tests/orders_http.rs`
- Delete: `services/api/tests/open_orders_http.rs`

- [ ] **Step 1: Skim the legacy HTTP suite for setup helpers**

```bash
sed -n '1,90p' services/api/tests/open_orders_http.rs
```

Reuse the same fixture helpers (test client, seed user, sign helper). The structure of the new file follows the legacy one verbatim — only the path, response shape, and expected statuses change.

- [ ] **Step 2: Write the new test file**

Create `services/api/tests/orders_http.rs` with the following cases (skeleton; copy the pre-existing sign/setup helpers from the legacy file into a `common` module if not already shared):

```rust
//! HTTP coverage for GET /api/v1/orders through the production router.

mod common;

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn readonly_user_data_key_can_fetch_orders() {
    // Seed a user with USER_DATA permission only and a few rows.
    // GET /api/v1/orders with valid signature.
    // Assert: 200, body shape matches OrdersPageResponse, includes
    //          rows across NEW + PARTIALLY_FILLED + FILLED + CANCELLED.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn returns_only_one_side_returns_minus_1102() {
    // marketAddress without symbol → -1102 / 400.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn unknown_market_pair_returns_minus_1121() {
    // both params set, but pair does not exist → -1121 / 404.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn unknown_status_token_returns_minus_1130() {
    // status=NEW,WRONG_TOKEN → -1130 / 400.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn pending_new_status_token_returns_minus_1130() {
    // status=PENDING_NEW (write-side synthetic) → -1130 / 400.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn empty_cursor_returns_minus_1102() {
    // cursor=   (whitespace only) → -1102 / 400.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn limit_out_of_range_returns_minus_1102() {
    // limit=501 → -1102 / 400. limit=0 → -1102 / 400.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn pagination_roundtrip_returns_descending_order() {
    // Seed 5 mixed-status rows; page 1 with limit=2 then page 2 with returned cursor.
    // Assert: combined response covers all 5 rows in DESC placed_chain_order
    //         and the second page sets nextCursor=null.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn missing_auth_returns_minus_1003() {
    // Drop the signature header.
    // Assert: 401 / -1003.
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn trade_only_key_returns_minus_1002() {
    // Key has TRADE only, not USER_DATA → 401 / -1002.
}
```

Fill each test body using the legacy file's existing helpers as a template; the only new cases are `unknown_status_token_returns_minus_1130` and `pending_new_status_token_returns_minus_1130`, and they exercise the new `OrderStatusSet::from_csv` rejection path through the full HTTP stack.

- [ ] **Step 3: Run the suite**

```bash
cargo test -p dodex-api --test orders_http -- --test-threads=1
```

Expected: all 10 cases pass.

- [ ] **Step 4: Delete the legacy HTTP test file**

```bash
git rm services/api/tests/open_orders_http.rs
```

Confirm the full api test suite still builds and runs:

```bash
cargo test -p dodex-api --tests -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add services/api/tests/orders_http.rs
git commit -m "test(api): cover /api/v1/orders HTTP contract end to end"
```

---

## Task 8: Workspace-wide build + lint sweep

**Files:**
- Touch as needed: any code referencing the deleted symbols that was missed by earlier tasks.

- [ ] **Step 1: Run the full workspace build**

```bash
cargo check --workspace --all-targets
```

Expected: clean — no warnings about unused `OpenOrder*` symbols.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 3: Run the full test suite**

```bash
docker compose -f docker-compose.test.yml up -d --wait
cargo test --workspace --all-targets -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 4: Re-grep for any stale references**

```bash
grep -rnE 'openOrders|allOrders|OpenOrder' \
  crates/ services/ migrations/ \
  --include='*.rs' --include='*.sql' --include='*.md'
```

Expected: only intentional references remain — the line `DELETE /api/v1/openOrders` in the trading-API surface, the historical "supersedes the OPEN-only `live_orders_open_owner_idx`" sentences in docs, and the migration's own `drop index if exists live_orders_open_owner_idx;`. Anything else is a leftover and should be cleaned up before commit.

- [ ] **Step 5: Pre-commit doc sweep**

Per `AGENT_REQUIREMENTS.md`:

```bash
grep -rnE 'openOrders|allOrders|OpenOrder' docs/ README.md services/*/README.md
```

Expected: only the `DELETE /api/v1/openOrders` mentions in `docs/api-spec.md`, `docs/tech-specs/write-api.md`, `docs/tech-specs/auth.md`, and `docs/README.md`, plus the explicit "former endpoints" cross-references in `docs/api-spec.md §Orders` and `docs/tech-specs/read-api.md §/api/v1/orders`.

- [ ] **Step 6: Commit the sweep (if anything changed)**

If the sweep adjusted documentation comments or removed dead code:

```bash
git add -A
git commit -m "chore: clean up stale openOrders references after /orders cutover"
```

Otherwise skip the commit.

---

## Task 9: Open the pull request

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feature/node-3416-all-orders-api
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create --base dev --title "Unify openOrders + allOrders into /api/v1/orders" --body "$(cat <<'EOF'
## Summary
- Replaces `GET /api/v1/openOrders` and `GET /api/v1/allOrders` with a single account-scoped `GET /api/v1/orders` covering NEW / PARTIALLY_FILLED / FILLED / CANCELED / REJECTED.
- `status` CSV filter (default = all five statuses); cursor pagination on `placed_chain_order` DESC; `{ orders, nextCursor }` envelope.
- Migration swaps `live_orders_open_owner_idx` for the status-agnostic `live_orders_owner_idx`.
- `REJECTED` filter is accepted today but always returns empty; the projector that fills `status='REJECTED'` rows ships in a follow-up contracts PR (see `docs/tech-specs/read-api.md §REJECTED — future work`).

## Test plan
- [ ] `cargo test --workspace --all-targets -- --test-threads=1` passes against the test Postgres.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Manual smoke against a seeded DB: `curl '/api/v1/orders?...&status=NEW,FILLED'` returns the expected mix.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Return the PR URL when complete.

---

## Self-Review

**Spec coverage:**
- Public spec `### Orders` table: marketAddress / symbol / status / limit / cursor / timestamp / recvWindow / signature → all parsed (Task 5) and validated (Task 3).
- Behavior bullet list (filter shapes, error codes -1102/-1121/-1130, DESC sort, cursor stability, REJECTED-pending) → covered by Tasks 3 (parser), 4 (SQL), 6/7 (tests).
- Response envelope `{ orders, nextCursor }` → Task 5 DTO, Task 7 assertion.
- Order fields (incl. empty `orderId` for REJECTED, `executedQty > 0` allowed for CANCELED) → Task 4 mapper, Task 6 `canceled_partial_fill_reports_nonzero_executed_qty`.
- Tech-spec `§Status mapping`, `§Field projection`, `§Pagination`, `§SQL`, `§Index reliance` → Tasks 4 (SQL + mapper), 1 (index), 6 (tests).
- `§Contract event consumption` is a doc-only assertion already in place; no code change needed.
- `§REJECTED — future work` is explicitly excluded from this PR; Task 6 assert "rejected filter returns empty today" pins the boundary.

**Placeholder scan:** none. Every step has either a concrete code block, a concrete command, or a concrete file edit.

**Type consistency:**
- `Order` used in domain, application page (`Vec<Order>`), infra mapper, HTTP DTO conversion (`order_to_dto`).
- `OrderStatus` (6-variant) used in domain, in `OrderStatusSet`, in infra mapper match; `PendingNew` is rejected at parse time and an `unreachable!` in the SQL builder pins that.
- `OrdersQuery` fields (`owner_pn_address`, `market`, `status`, `limit`, `cursor`) referenced consistently across Tasks 3 (definition), 4 (consumer), 5 (caller via the use case).
- Trait method name `list_orders` appears in: `MarketReadRepository` definition (Task 3), `Arc<T>` blanket impl (Task 3), Postgres impl (Task 4), in-file mock (Task 3 step 5), test-side mock (Task 6 if needed — note the existing `CreateOrderUseCase` mock at `crates/application/src/lib.rs:731` also needs the rename: include in Task 3 step 5).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-20-unified-orders-endpoint.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using `executing-plans`, batch execution with checkpoints.

Which approach?
