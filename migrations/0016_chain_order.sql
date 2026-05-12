-- Introduce strict chain-order ordering for indexed events. The GraphQL
-- gateway returns `msg_chain_order` as a global lex-sortable string; the
-- indexer now stores it on every `raw_events` row and replays/projects
-- in that order rather than by chain timestamp (which can collide within
-- one second and drift across shards). `live_orders.last_event_lt` is
-- replaced with `last_chain_order`, surfaced as `lastUpdateId` in
-- `/api/v1/depth` (api-spec.md: now `STRING`).
--
-- Requires reindex: existing rows in `raw_events`/`live_orders` predate
-- the new column and cannot be backfilled — `chain_order` lives on the
-- chain message, not in any local data. Truncate the affected tables
-- (plus `indexer_cursors` so the indexer resumes from genesis) before
-- the new NOT NULL constraints land. This is a one-time reindex window;
-- the project is pre-prod (api-spec.md: "Status: Draft").
truncate raw_events, live_orders, indexer_cursors;

alter table raw_events
    add column chain_order text not null;

create index raw_events_chain_order_idx on raw_events (chain_order);

alter table live_orders
    drop column last_event_lt,
    add column last_chain_order text not null;
