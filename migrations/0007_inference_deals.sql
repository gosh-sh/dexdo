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
    finalized_owed_total numeric(78, 0) not null default 0,
    funded_at_chain timestamptz,
    opened_at_chain timestamptz,
    settled_at_chain timestamptz,
    close_kind text check (close_kind in ('STOPPED', 'DISPUTE_RESOLVED', 'RECLAIMED', 'DESTROYED')),
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
