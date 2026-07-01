-- Consolidated initial schema for the DEX.DO read-model and indexer.
-- Source of truth for table/field intent is docs/tech-specs/data-schema.md.

create extension if not exists pgcrypto;

create type auth_permission as enum ('USER_DATA', 'TRADE');

create table ref_tokens (
    token_type integer primary key,
    token_code text not null unique,
    decimals integer not null,
    min_notional numeric(78, 0) not null,
    lot_size numeric(78, 0) not null,
    tick_size_bps numeric(78, 0) not null,
    price_precision integer not null,
    quantity_precision integer not null,
    enabled boolean not null default true,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table raw_events (
    id bigserial primary key,
    msg_id text not null unique,
    created_at_chain timestamptz,
    src_address text,
    dst_address text,
    event_type text,
    body_json jsonb not null default '{}'::jsonb,
    processed_at timestamptz,
    created_at timestamptz not null default now(),
    decoded jsonb,
    chain_order text not null
);

create index raw_events_event_type_idx on raw_events (event_type);
create index raw_events_created_at_chain_idx on raw_events (created_at_chain desc);
create index raw_events_event_type_decoded_idx
    on raw_events (event_type)
    where event_type is not null;
create index raw_events_chain_order_idx on raw_events (chain_order);

-- The projection loop is the sole projector and reads every pending raw_events
-- row ordered by chain_order via a keyset cursor (chain_order > $after). Keying
-- the pending partial index by chain_order makes that a forward range scan over
-- only pending rows (no separate sort, no scan through projected history).
create index raw_events_pending_chain_order_idx
    on raw_events (chain_order)
    where processed_at is null
      and event_type is not null
      and decoded is not null;

-- The inference reconciler's sweep catch-up gate probes "are there any pending
-- events for this book?" by src_address = orderbook_address over the projection
-- predicate. Indexing src_address over that same partial predicate makes the
-- per-book equality probe an index probe rather than a full-table scan.
create index raw_events_pending_src_idx
    on raw_events (src_address)
    where processed_at is null and event_type is not null and decoded is not null;

create table indexer_cursors (
    stream_name text primary key,
    cursor text,
    -- True only when the capture loop's most recent drain returned
    -- has_next_page=false. The inference reconciler reads it as the `at_head`
    -- sweep catch-up gate: a phantom cancel must not fire while the gateway
    -- still has older pages ahead of the cursor.
    at_head boolean not null default false,
    updated_at timestamptz not null default now()
);

create table oracles (
    id bigserial primary key,
    name text not null unique,
    address text not null unique,
    deploy_msg_id text unique,
    pubkey text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table oracle_event_lists (
    id bigserial primary key,
    msg_id text not null unique,
    oracle_id bigint not null references oracles(id) on delete cascade,
    address text not null unique,
    list_index bigint,
    created_at timestamptz not null default now(),
    last_reconcile_failed_at timestamptz,
    reconcile_attempts integer not null default 0,
    description text not null
);

create index oracle_event_lists_oracle_id_idx on oracle_event_lists (oracle_id);

create table oracle_events (
    id bigserial primary key,
    eventlist_id bigint not null references oracle_event_lists(id) on delete cascade,
    internal_id_in_eventlist numeric(78, 0) not null,
    event_name text not null,
    oracle_fee numeric(78, 0),
    deadline bigint not null,
    describe text,
    count numeric(78, 0),
    trust_addr text,
    outcome_names_jsonb jsonb not null default '{}'::jsonb,
    is_deleted boolean not null default false,
    last_seen_at timestamptz not null default now(),
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    confirmed_pmp_address text,
    confirmed_at timestamptz,
    meta_reconciled_at timestamptz,
    unique (eventlist_id, internal_id_in_eventlist)
);

create index oracle_events_eventlist_id_idx on oracle_events (eventlist_id);
create index oracle_events_deadline_idx on oracle_events (deadline);
create index oracle_events_confirmed_pmp_idx
    on oracle_events (confirmed_pmp_address)
    where confirmed_pmp_address is not null;
create index oracle_events_pending_meta_idx
    on oracle_events (eventlist_id)
    where meta_reconciled_at is null;

create table markets (
    id bigserial primary key,
    pmp_address text not null unique,
    market_id text,
    name text,
    token_type integer not null references ref_tokens(token_type),
    token_code text not null,
    event_id numeric(78, 0) not null,
    oracle_list_hash numeric(78, 0),
    orderbook_address text,
    approved boolean not null default false,
    is_cancelled boolean not null default false,
    stake_start bigint,
    stake_end bigint,
    result_start bigint,
    result_end bigint,
    num_outcomes integer not null default 0,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    last_reconciled_at timestamptz,
    oracle_event_lists_json jsonb,
    oracle_fee_json jsonb,
    frozen_at bigint,
    resolved_at bigint,
    resolved_outcome_id integer,
    cancelled_at bigint,
    cancel_reason text,
    last_reconcile_failed_at timestamptz,
    reconcile_attempts integer not null default 0,
    constraint markets_orderbook_address_set_after_reconcile
        check (last_reconciled_at is null or orderbook_address is not null)
);

create index markets_market_id_idx on markets (market_id);
create index markets_status_idx on markets (approved, is_cancelled);
create index markets_pending_reconcile_idx
    on markets (last_reconciled_at)
    where last_reconciled_at is null;
create index markets_terminal_idx
    on markets (resolved_at, cancelled_at)
    where resolved_at is not null or cancelled_at is not null;
create unique index markets_orderbook_address_unique
    on markets (orderbook_address)
    where orderbook_address is not null;

-- max_batch_size is backend policy (the api's mirror of the chain's compiled-in
-- MAX_BATCH_SIZE), not chain state: it lives in api config (`chain.max_batch_size`),
-- enforced by the batch use cases and advertised in /api/v1/markets from that
-- single source, so it is intentionally not a column here.
create table market_outcomes (
    id bigserial primary key,
    market_id_fk bigint not null references markets(id) on delete cascade,
    pmp_address text not null,
    outcome_id integer not null,
    outcome_name text not null,
    symbol text not null unique,
    price_precision integer not null,
    quantity_precision integer not null,
    tick_size text not null,
    step_size text not null,
    min_notional text not null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (pmp_address, outcome_id)
);

create index market_outcomes_market_id_fk_idx on market_outcomes (market_id_fk);

create table order_book_snapshots (
    id bigserial primary key,
    symbol text not null unique,
    orderbook_address text,
    last_update_id bigint not null,
    bids_jsonb jsonb not null default '[]'::jsonb,
    asks_jsonb jsonb not null default '[]'::jsonb,
    updated_at timestamptz not null default now()
);

create table live_orders (
    orderbook_address text not null,
    order_id numeric(78, 0) not null,
    outcome_id integer not null,
    is_buy boolean not null,
    price numeric(78, 0) not null,
    amount_remaining numeric(78, 0) not null,
    client_order_id text,
    status text not null check (status in ('OPEN', 'FILLED', 'CANCELLED')),
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    last_chain_order text not null,
    owner_pn_address text,
    amount_initial numeric(78, 0) not null,
    chain_created_at timestamptz,
    chain_updated_at timestamptz,
    placed_chain_order text not null,
    primary key (orderbook_address, order_id)
);

create index live_orders_open_book_idx
    on live_orders (orderbook_address, outcome_id, is_buy, price desc)
    where status = 'OPEN';

-- Owner-scoped /api/v1/orders pagination. The `placed_chain_order DESC`
-- key ordering, the partial predicate, and the omission of
-- `chain_updated_at IS NOT NULL` are all justified in
-- docs/tech-specs/read-api.md#index-reliance.
create index live_orders_owner_idx
    on live_orders (owner_pn_address, placed_chain_order desc)
    where owner_pn_address is not null
      and chain_created_at is not null;

create table accounts (
    id uuid primary key default gen_random_uuid(),
    label text,
    pn_address text not null unique,
    pn_pubkey numeric(78, 0) not null,
    pn_seckey_enc bytea not null,
    pn_dih numeric(78, 0) not null unique,
    disabled_at timestamptz,
    created_at timestamptz not null default now()
);

create table api_keys (
    id bigserial primary key,
    account_id uuid not null references accounts(id) on delete cascade,
    api_key text not null,
    api_secret_enc bytea not null,
    permissions auth_permission[] not null default array['USER_DATA'::auth_permission],
    disabled_at timestamptz,
    last_used_at timestamptz,
    created_at timestamptz not null default now()
);

create unique index api_keys_api_key_active_idx
    on api_keys (api_key)
    where disabled_at is null;
create index api_keys_account_id_idx on api_keys (account_id);

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

-- Inference order-book read model: one inference_markets row per InferenceOrderBook
-- (one per model), and one inference_orders row per chain-side order.
-- Every column except the two skeleton-seed columns is nullable/defaulted so the
-- discovery pre-step can seed a market from any event before the reconciler fills it.

create table inference_markets (
    id bigserial primary key,
    orderbook_address text not null unique,
    model_hash numeric(78, 0),
    platform_fee_bps integer,
    quote_token_type integer references ref_tokens(token_type),
    price_precision integer,
    quantity_precision integer,
    tick_size text,
    step_size text,
    min_notional text,
    reference_price numeric(78, 0),
    reference_price_at timestamptz,
    model_ref text,
    producer text,
    model_name text,
    -- Contract version reported by the book's `getVersion` getter (e.g. 4.0.14).
    -- Used by the reconciler's model-slot supersede resolution (parsed as semver).
    -- NOT the model version — that is `model_version`.
    version text,
    -- Parsed model version: the third part of a `producer--model--version`
    -- `model_name`. Rendered as the API's `model.version`. Filled by the
    -- inference reconciler from `getModelName()`; NULL when the name is empty or
    -- not three-part.
    model_version text,
    manifest_address text,
    root_model_address text,
    owner_pubkey numeric(78, 0),
    created_at_chain timestamptz,
    last_reconciled_at timestamptz,
    last_reconcile_failed_at timestamptz,
    reconcile_attempts integer not null default 0,
    -- Set when this book is retired as a stale duplicate of a higher-version
    -- book for the same model. Excluded from discovery, the read API, and the
    -- discovering/visible/failing metric buckets.
    superseded_at timestamptz,
    last_swept_at timestamptz,
    sweep_cursor numeric(78, 0),
    sweep_cycle_max numeric(78, 0),
    -- Monotonic counter bumped whenever a Filled override reopens a provisionally
    -- sweep-cancelled row while the book is still in discovery. The discovery
    -- visibility stamp is guarded on this being unchanged across the completing
    -- sweep tick, so an override that resets sweep_cursor to NULL on the FIRST tick
    -- of a cycle (where a plain cursor-CAS cannot distinguish reset-from-NULL from
    -- start-of-cycle-NULL) still blocks the stamp until a fresh cycle re-checks the
    -- reopened id.
    sweep_override_seq bigint not null default 0,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create unique index inference_markets_model_hash_idx
    on inference_markets (model_hash) where model_hash is not null;
create index inference_markets_pending_reconcile_idx
    on inference_markets (last_reconcile_failed_at nulls first, id)
    where last_reconciled_at is null;
create index inference_markets_refresh_idx
    on inference_markets (reference_price_at nulls first)
    where last_reconciled_at is not null;
create index inference_markets_sweep_idx
    on inference_markets (last_swept_at nulls first)
    where last_reconciled_at is not null;

create table inference_orders (
    orderbook_address text not null,
    order_id numeric(78, 0) not null,
    is_buy boolean not null,
    price numeric(78, 0) not null,
    amount_initial numeric(78, 0) not null,
    amount_remaining numeric(78, 0) not null,
    is_subscription boolean not null default false,
    status text not null check (status in ('OPEN', 'FILLED', 'CANCELLED')),
    swept_at timestamptz,
    note_address text,
    last_chain_order text not null,
    chain_created_at timestamptz,
    chain_updated_at timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    primary key (orderbook_address, order_id)
);

create index inference_orders_open_book_idx
    on inference_orders (orderbook_address, is_buy, price desc) where status = 'OPEN';
create index inference_orders_sweep_idx
    on inference_orders (orderbook_address, order_id) where status = 'OPEN';

-- Inference SETTLEMENT read-model. One inference_deals row per TokenContract
-- (the per-deal streaming escrow auto-deployed when a SELL offer is matched),
-- and one inference_ticks row per finalized tick. Every column except the PK is
-- nullable/defaulted so a deal skeleton can be seeded by any TokenContract.*
-- event (keyed by src_address) before InferenceOrderBook.Filled fills the
-- orderbook_address + seller_note link.

create table inference_deals (
    token_contract_address text primary key,
    orderbook_address text,
    seller_note text,
    buyer_note text,
    deposit numeric(78, 0),
    price_per_tick numeric(78, 0),
    finalized_ticks integer not null default 0,
    funded_at_chain timestamptz,
    opened_at_chain timestamptz,
    settled_at_chain timestamptz,
    close_kind text check (close_kind in ('STOPPED', 'DISPUTE_RESOLVED', 'RECLAIMED', 'DESTROYED', 'PROBE_BURNED')),
    clean_settlement boolean,
    disputed_at_chain timestamptz,
    last_chain_order text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index inference_deals_orderbook_idx on inference_deals (orderbook_address);
create index inference_deals_seller_idx on inference_deals (seller_note);
create index inference_deals_buyer_idx on inference_deals (buyer_note);

create table inference_ticks (
    token_contract_address text not null references inference_deals(token_contract_address) on delete cascade,
    chain_order text not null,
    finalized_owed numeric(78, 0) not null,
    deposit numeric(78, 0) not null,
    chain_at timestamptz,
    created_at timestamptz not null default now(),
    primary key (token_contract_address, chain_order)
);

create index inference_ticks_tc_idx on inference_ticks (token_contract_address);

insert into ref_tokens (
    token_type,
    token_code,
    decimals,
    min_notional,
    lot_size,
    tick_size_bps,
    price_precision,
    quantity_precision
)
values
    (1, 'NACKL', 9, 10000000000, 10000000, 10, 3, 2),
    (2, 'SHELL', 9, 100000000000, 10000000, 10, 3, 2),
    (3, 'USDC', 6, 1000000, 10000, 10, 3, 2);
