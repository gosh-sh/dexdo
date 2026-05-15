-- Adds owner-attribution, chain-time, and chain-order columns to live_orders
-- that /api/v1/openOrders depends on. Pre-MVP: empty target, no backfill.
--
-- placed_chain_order is the `msg_chain_order` of the OrderPlaced event that
-- created the row, set once by the apply_order_placed projector via
-- coalesce(...) so replay never moves it. Globally unique by gateway design,
-- so it serves as the sole sort key + cursor for /api/v1/openOrders. The
-- separate timestamp columns chain_created_at / chain_updated_at remain for
-- `time` / `updateTime` rendering in API responses and are not part of the
-- pagination key — `node.created_at` is unix-seconds and collides on a
-- shared chain second, which is acceptable for display but not for sort.

alter table live_orders
    add column owner_pn_address    text,
    add column amount_initial      numeric(78, 0) not null,
    add column chain_created_at    timestamptz,
    add column chain_updated_at    timestamptz,
    add column placed_chain_order  text not null;

-- Partial index for owner-scoped openOrders pagination. Single-column lex
-- range scan on placed_chain_order. chain_created_at / chain_updated_at
-- NOT NULL is enforced as a SQL-side heap filter in list_open_orders, not
-- as an index predicate — keeping the index independent of display-only
-- columns minimises write amplification when those columns advance.
create index if not exists live_orders_open_owner_idx
    on live_orders (owner_pn_address, placed_chain_order)
    where owner_pn_address is not null
      and status = 'OPEN'
      and amount_remaining > 0;
