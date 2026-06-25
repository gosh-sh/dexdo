-- True only when the capture loop's most recent drain returned has_next_page=false.
-- The inference reconciler reads it as sweep catch-up gate (i): a phantom cancel
-- must not fire while the gateway still has older pages ahead of the cursor.
alter table indexer_cursors add column at_head boolean not null default false;
