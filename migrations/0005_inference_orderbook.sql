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
    version text,
    manifest_address text,
    root_model_address text,
    owner_pubkey numeric(78, 0),
    created_at_chain timestamptz,
    last_reconciled_at timestamptz,
    last_reconcile_failed_at timestamptz,
    reconcile_attempts integer not null default 0,
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

-- The pending-events sweep catch-up gate probes "any row the projector still owes this book"
-- by src_address = orderbook_address over the projection-loop predicate. The
-- existing raw_events_pending_projection_idx leads on (created_at_chain, id), so
-- a per-book equality probe can't use it; this partial index makes it an index probe.
create index raw_events_pending_src_idx
    on raw_events (src_address)
    where processed_at is null and event_type is not null and decoded is not null;
