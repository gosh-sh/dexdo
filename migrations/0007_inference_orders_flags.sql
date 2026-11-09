-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- Order flags on the inference read model.
--
-- `flags` is the chain's `uint8 flags` as the order was placed: written from
-- `InferenceOrderPlaced.flags`, or, for a live row that predates this
-- migration, from `InferenceOrderBook.getOrder` by the reconciler's sweep.
-- Bits: IOC 0x01, FOK 0x02, MARKET 0x04, POST_ONLY 0x08, TEE 0x10, AON 0x20,
-- SUBSCRIPTION 0x40. The book refuses any other bit (SUPPORTED_FLAGS = 0x7F).
--
-- Nullable on purpose. NULL means "unknown", never "no flags": 0 is a plain
-- limit order. Existing rows start NULL and nothing backfills them; the sweep
-- fills those still in the book.
alter table inference_orders add column flags smallint;

-- One partial index per bit backs `?flag=` on GET /api/v1/inference/orders.
-- Each has the key of `inference_orders_book_side_status_idx`, so a page
-- branch that pins (orderbook_address, is_buy, status) reads only the rows
-- carrying the bit, already in cursor order, and stops at its LIMIT.
--
-- The read path writes the bit as a literal (`(flags & 32) <> 0`), never a
-- bind parameter: Postgres uses a partial index only when it can prove the
-- query's WHERE implies the index predicate, and `$n` hides the constant from
-- that proof. `inference_read_repo::flag_predicate` is pinned to these lines
-- by a unit test.
--
-- A plain order (flags = 0) and an unknown one (NULL) enter none of them, so
-- the write cost tracks how many orders carry flags at all.
--
-- Not CONCURRENTLY, as in 0006: the indexer applies migrations at startup,
-- before it captures or projects anything.
create index inference_orders_flag_ioc_idx
    on inference_orders (orderbook_address, is_buy, status, order_id desc)
    where (flags & 1) <> 0;
create index inference_orders_flag_fok_idx
    on inference_orders (orderbook_address, is_buy, status, order_id desc)
    where (flags & 2) <> 0;
create index inference_orders_flag_market_idx
    on inference_orders (orderbook_address, is_buy, status, order_id desc)
    where (flags & 4) <> 0;
create index inference_orders_flag_post_only_idx
    on inference_orders (orderbook_address, is_buy, status, order_id desc)
    where (flags & 8) <> 0;
create index inference_orders_flag_tee_idx
    on inference_orders (orderbook_address, is_buy, status, order_id desc)
    where (flags & 16) <> 0;
create index inference_orders_flag_aon_idx
    on inference_orders (orderbook_address, is_buy, status, order_id desc)
    where (flags & 32) <> 0;
create index inference_orders_flag_subscription_idx
    on inference_orders (orderbook_address, is_buy, status, order_id desc)
    where (flags & 64) <> 0;
