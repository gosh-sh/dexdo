-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- Backs the time-window scans of GET /api/v1/inference/stats (rolling 24 h, 28 / 60 days).
-- Partial: a row with NULL chain_time is never counted. See
-- docs/tech-specs/data-schema.md#inference_trades.
create index inference_trades_chain_time_idx
    on inference_trades (chain_time)
    where chain_time is not null;
