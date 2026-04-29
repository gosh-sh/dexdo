alter table raw_events alter column src_address drop not null;
alter table raw_events alter column dst_address drop not null;
alter table raw_events alter column event_type drop not null;
