alter table markets
    add column if not exists frozen_at bigint,
    add column if not exists resolved_at bigint,
    add column if not exists resolved_outcome_id integer,
    add column if not exists cancelled_at bigint,
    add column if not exists cancel_reason text;

create index if not exists markets_terminal_idx
    on markets (resolved_at, cancelled_at)
    where resolved_at is not null or cancelled_at is not null;
