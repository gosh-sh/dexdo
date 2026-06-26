-- True only when the capture loop's most recent drain returned has_next_page=false.
-- The inference reconciler reads it as the `at_head` sweep catch-up gate: a phantom
-- cancel must not fire while the gateway still has older pages ahead of the cursor.
alter table indexer_cursors add column at_head boolean not null default false;
