-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- Backs the `?liquidity=` filter on GET /api/v1/inference/markets and the
-- GET /api/v1/inference/liquidity aggregate.
--
-- `inference_orders_open_book_idx` already leads with (orderbook_address,
-- is_buy), which is the whole key here — an InferenceOrderBook is one book per
-- model, so neither caller has an outcome dimension to narrow by. What it does
-- not do is carry `amount_remaining`: its predicate is `status = 'OPEN'` alone
-- and `price` occupies the payload slot, so every candidate row costs a heap
-- fetch to test `amount_remaining > 0` and to read the value the aggregate
-- sums. For the listing filter that is one fetch per book; for the totals it is
-- one per open order on the book.
--
-- This index drops `price` (irrelevant to both callers), pushes
-- `amount_remaining > 0` into the predicate, and carries `amount_remaining` as
-- an INCLUDE payload, so the EXISTS probe and the per-side sums are both
-- index-only.
--
-- Both callers filter on exactly `status = 'OPEN' AND amount_remaining > 0` —
-- the same pair `/api/v1/inference/depth` aggregates on — so the partial
-- predicate is implied by every query that uses this index.
create index inference_orders_liquidity_idx
    on inference_orders (orderbook_address, is_buy)
    include (amount_remaining)
    where status = 'OPEN' and amount_remaining > 0;
