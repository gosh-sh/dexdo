-- `indexer_cursors.last_seen_lt` was declared in migration 0001 as a
-- diagnostic placeholder but never read or written by any code path. Drop
-- it: the deferred chain-order refactor (see project memo
-- `event_chain_order_gap`) will introduce a proper per-row `chain_order`
-- column on `raw_events`, not a per-stream high-water mark on cursors, so
-- this column has no future use either.
alter table indexer_cursors drop column if exists last_seen_lt;
