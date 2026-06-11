-- Append-only public trade tape behind GET /api/v1/trades. One row per
-- maker↔taker match, written by the OrderBook.OrderFilled projector on the
-- taker-side event only (isTaker = true); the maker-side event mutates
-- live_orders but writes no trades row, so a match is recorded exactly once.
-- Rows are immutable once written, except a first-write-wins fill of a NULL
-- chain_time on replay; never deleted. See docs/tech-specs/data-schema.md#trades.
create table trades (
    -- Taker-side OrderFilled event's chain-order key (gateway msg_chain_order,
    -- copied from raw_events.chain_order). Globally unique per match and
    -- lex-sortable: the sole sort key and identity for the tape (DESC).
    trade_id text primary key,
    orderbook_address text not null,
    outcome_id integer not null,
    -- Clearing price from OrderFilled.clearingPrice, raw basis points.
    price numeric(78, 0) not null,
    -- Matched quantity from OrderFilled.filledAmount, raw token atoms.
    qty numeric(78, 0) not null,
    -- Trade direction: taker selling ⇒ buyer is the maker ⇒ true.
    is_buyer_maker boolean not null,
    -- On-chain block time of the taker event (raw_events.created_at_chain).
    -- NULL when the gateway omitted created_at; such rows are filtered out of
    -- the read query, matching live_orders / /api/v1/orders.
    chain_time timestamptz,
    -- Indexer ingestion wall-clock (bookkeeping).
    created_at timestamptz not null default now()
);

-- Backs the newest-first per-outcome read (ORDER BY trade_id DESC LIMIT $limit)
-- as an index range scan.
create index trades_tape_idx on trades (orderbook_address, outcome_id, trade_id desc);
