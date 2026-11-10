-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- `inference_orders.flags`: the chain's `uint8 flags` as placed (IOC 0x01, FOK 0x02,
-- MARKET 0x04, POST_ONLY 0x08, TEE 0x10, AON 0x20, SUBSCRIPTION 0x40).
-- NULL = unknown, 0 = plain limit order. No backfill; the sweep fills live rows.
--
-- Kept apart from the indexes (0008) so ADD COLUMN's ACCESS EXCLUSIVE lock lasts
-- milliseconds instead of the whole index build.
alter table inference_orders add column flags smallint;
