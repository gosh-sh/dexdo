-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- Backs the `?liquidity=` filter on GET /api/v1/inference/markets: a
-- correlated EXISTS probe per candidate book for a matchable resting order.
--
-- `inference_orders_open_book_idx` already leads with (orderbook_address,
-- is_buy), which is the whole key here — an InferenceOrderBook is one book per
-- model, so the probe has no outcome dimension to narrow by. What it does not
-- do is carry `amount_remaining` or `deadline`: its predicate is
-- `status = 'OPEN'` alone, so every candidate row costs a heap fetch to test
-- `amount_remaining > 0` and the deadline.
--
-- This index drops `price` (the EXISTS probe never reads it), pushes
-- `amount_remaining > 0` into the predicate, and carries `amount_remaining`
-- and `deadline` as INCLUDE payload, so the probe is index-only.
--
-- It does not cover the price-ordered readers of the same resting set — the
-- levels and whole-side totals on GET /api/v1/inference/depth and the
-- bestBid/bestAsk laterals on /inference/markets — because they group or sort
-- by `price`, which this index does not hold.
--
-- `deadline` is payload rather than predicate because the test is against the
-- request clock (`deadline IS NULL OR deadline > $now`), which no index
-- predicate may reference. Carrying the column keeps that comparison on the
-- index tuple instead of sending every candidate row to the heap.
--
-- The probe filters on exactly `status = 'OPEN' AND amount_remaining > 0`
-- plus that deadline test — the same definition of resting
-- `/api/v1/inference/depth` aggregates on — so the partial predicate is
-- implied by every query that uses this index.
create index inference_orders_liquidity_idx
    on inference_orders (orderbook_address, is_buy)
    include (amount_remaining, deadline)
    where status = 'OPEN' and amount_remaining > 0;
