create table if not exists live_orders (
    orderbook_address text not null,
    order_id numeric(78, 0) not null,
    outcome_id integer not null,
    is_buy boolean not null,
    price numeric(78, 0) not null,
    amount_remaining numeric(78, 0) not null,
    client_order_id text,
    status text not null check (status in ('OPEN', 'FILLED', 'CANCELLED')),
    last_event_lt bigint,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    primary key (orderbook_address, order_id)
);

create index if not exists live_orders_open_book_idx
    on live_orders (orderbook_address, outcome_id, is_buy, price desc)
    where status = 'OPEN';
