alter table oracle_events add column if not exists confirmed_pmp_address text;
alter table oracle_events add column if not exists confirmed_at timestamptz;

create index if not exists oracle_events_confirmed_pmp_idx
    on oracle_events (confirmed_pmp_address)
    where confirmed_pmp_address is not null;
