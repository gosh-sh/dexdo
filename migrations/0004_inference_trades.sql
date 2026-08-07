-- Append-only public trade tape behind GET /api/v1/inference/trades. One row per
-- maker<->taker match, written by the InferenceOrderBook.InferenceFilled projector.
-- Unlike the prediction `trades` tape there is no taker-side gate: the inference book
-- emits ONE `InferenceFilled` per match (carrying both leg ids), so the event itself is
-- already one-per-match. There is no outcome dimension either — an InferenceOrderBook is
-- one book per model. Rows are immutable once written, except a first-write-wins fill of
-- a NULL chain_time on replay; never deleted.
-- See docs/tech-specs/data-schema.md#inference_trades.
create table inference_trades (
    -- The `InferenceFilled` event's chain-order key (gateway msg_chain_order, copied from
    -- raw_events.chain_order). Globally unique per match and lex-sortable: the sole sort
    -- key and identity for the tape (DESC).
    trade_id text primary key,
    orderbook_address text not null,
    -- Clearing price from InferenceFilled.clearingPrice, raw quote-asset base units per tick.
    price numeric(78, 0) not null,
    -- Matched tick count from InferenceFilled.ticks, raw.
    qty numeric(78, 0) not null,
    -- Trade direction: the resting (maker) leg was the BUY => true. Not carried by the
    -- event; resolved from inference_orders.is_buy of the maker leg at projection time.
    is_buyer_maker boolean not null,
    -- On-chain block time of the Filled event (raw_events.created_at_chain). NULL when the
    -- gateway omitted created_at; such rows are filtered out of the read query, matching
    -- inference_orders / the prediction tape.
    chain_time timestamptz,
    -- Indexer ingestion wall-clock (bookkeeping).
    created_at timestamptz not null default now()
);

-- Backs the newest-first per-book read (ORDER BY trade_id DESC LIMIT $limit) as an index
-- range scan.
create index inference_trades_tape_idx on inference_trades (orderbook_address, trade_id desc);
