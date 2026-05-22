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
create index raw_events_pending_projection_idx
    on raw_events (created_at_chain, id)
    where processed_at is null
      and event_type is not null
      and decoded is not null;
create index raw_events_chain_order_idx on raw_events (chain_order);

create table indexer_cursors (
    stream_name text primary key,
    cursor text,
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
    reconcile_attempts integer not null default 0
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
    max_batch_size integer not null,
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
