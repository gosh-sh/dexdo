create table if not exists ref_tokens (
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

create table if not exists raw_events (
    id bigserial primary key,
    msg_id text not null unique,
    created_at_chain timestamptz,
    src_address text not null,
    dst_address text not null,
    event_type text not null,
    body_json jsonb not null default '{}'::jsonb,
    processed_at timestamptz,
    created_at timestamptz not null default now()
);

create index if not exists raw_events_event_type_idx on raw_events (event_type);
create index if not exists raw_events_created_at_chain_idx on raw_events (created_at_chain desc);

create table if not exists indexer_cursors (
    stream_name text primary key,
    cursor text,
    last_seen_lt text,
    updated_at timestamptz not null default now()
);

create table if not exists oracles (
    id bigserial primary key,
    name text not null unique,
    address text not null unique,
    deploy_msg_id text unique,
    pubkey text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table if not exists oracle_event_lists (
    id bigserial primary key,
    msg_id text not null unique,
    oracle_id bigint not null references oracles(id) on delete cascade,
    address text not null unique,
    list_index bigint,
    created_at timestamptz not null default now()
);

create index if not exists oracle_event_lists_oracle_id_idx on oracle_event_lists (oracle_id);

create table if not exists oracle_events (
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
    unique (eventlist_id, internal_id_in_eventlist)
);

create index if not exists oracle_events_eventlist_id_idx on oracle_events (eventlist_id);
create index if not exists oracle_events_deadline_idx on oracle_events (deadline);

create table if not exists markets (
    id bigserial primary key,
    pmp_address text not null unique,
    market_id text not null,
    name text not null,
    token_type integer not null references ref_tokens(token_type),
    token_code text not null,
    event_id numeric(78, 0) not null,
    oracle_list_hash numeric(78, 0) not null,
    orderbook_address text,
    approved boolean not null default false,
    is_cancelled boolean not null default false,
    stake_start bigint,
    stake_end bigint,
    result_start bigint,
    result_end bigint,
    num_outcomes integer not null default 0,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists markets_market_id_idx on markets (market_id);
create index if not exists markets_status_idx on markets (approved, is_cancelled);

create table if not exists market_outcomes (
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

create index if not exists market_outcomes_market_id_fk_idx on market_outcomes (market_id_fk);

create table if not exists order_book_snapshots (
    id bigserial primary key,
    symbol text not null unique,
    orderbook_address text,
    last_update_id bigint not null,
    bids_jsonb jsonb not null default '[]'::jsonb,
    asks_jsonb jsonb not null default '[]'::jsonb,
    updated_at timestamptz not null default now()
);

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
    (3, 'USDC', 6, 1000000, 10000, 10, 3, 2)
on conflict (token_type) do update
set
    token_code = excluded.token_code,
    decimals = excluded.decimals,
    min_notional = excluded.min_notional,
    lot_size = excluded.lot_size,
    tick_size_bps = excluded.tick_size_bps,
    price_precision = excluded.price_precision,
    quantity_precision = excluded.quantity_precision,
    updated_at = now();

