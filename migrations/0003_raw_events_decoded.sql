alter table raw_events add column if not exists decoded jsonb;

create index if not exists raw_events_event_type_decoded_idx
    on raw_events (event_type)
    where event_type is not null;
