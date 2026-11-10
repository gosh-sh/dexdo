-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- One partial index per flag that can rest, backing `?flag=` on GET
-- /api/v1/inference/orders. IOC / FOK / MARKET never rest and get none.
-- Same key as inference_orders_book_side_status_idx. The read path must write
-- the bit as a literal to match (`inference_read_repo::flag_predicate`, pinned
-- by a unit test). Not CONCURRENTLY, as in 0006: writers wait, readers do not.
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
