alter table live_orders
    add column owner_pn_address  text,
    add column amount_initial    numeric(78, 0) not null default 0,
    add column chain_created_at  timestamptz,
    add column chain_updated_at  timestamptz;

update live_orders
   set amount_initial = amount_remaining
 where amount_initial = 0;

create index if not exists live_orders_open_owner_idx
    on live_orders (owner_pn_address, chain_created_at, order_id)
    where owner_pn_address is not null
      and status = 'OPEN'
      and amount_remaining > 0;
