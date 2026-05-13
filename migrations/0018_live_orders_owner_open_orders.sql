-- Pre-production-safe migration. The `add column ... not null default 0`
-- below rewrites every row on Postgres < 11 and takes an `AccessExclusiveLock`
-- on the table while the rewrite runs; the unqualified `UPDATE` further down
-- and the non-`CONCURRENTLY` `CREATE INDEX` each take their own table-level
-- locks. None of this is an issue while `live_orders` is empty (the project
-- has no deployed indexer yet — see review thread on this commit). When the
-- indexer ships against a database with live order data, this migration must
-- be replayed via a follow-up 0019 that adds the column with no default,
-- runs the backfill in batches, and creates the index `CONCURRENTLY`.

alter table live_orders
    add column owner_pn_address  text,
    add column amount_initial    numeric(78, 0) not null default 0,
    add column chain_created_at  timestamptz,
    add column chain_updated_at  timestamptz;

update live_orders
   set amount_initial = amount_remaining
 where amount_initial = 0;

-- The query's full ORDER BY / cursor tuple is
-- `(chain_created_at, order_id, orderbook_address)`. `orderbook_address` is
-- intentionally not part of the index — it appears only as a heap-filter
-- tie-breaker that disambiguates rows sharing both leading columns. Per-owner
-- cardinality at any single `(chain_created_at, order_id)` is expected to be 1
-- (or rare 2), so the trailing sort runs in-memory over a tiny set; widening
-- the index would cost write amplification with no read win.
create index if not exists live_orders_open_owner_idx
    on live_orders (owner_pn_address, chain_created_at, order_id)
    where owner_pn_address is not null
      and status = 'OPEN'
      and amount_remaining > 0
      and chain_created_at is not null
      and chain_updated_at is not null;
