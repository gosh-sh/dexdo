-- max_batch_size is backend policy (the api's mirror of the chain's
-- compiled-in MAX_BATCH_SIZE), not chain state: the chain exposes no
-- getter for it, so the reconciler could only ever write a constant.
-- It now lives in api config (`chain.max_batch_size`) — enforced by the
-- batch use cases and advertised in /api/v1/markets from that single
-- source. Deploy the indexer that no longer writes this column BEFORE
-- applying this migration.
alter table market_outcomes drop column max_batch_size;
