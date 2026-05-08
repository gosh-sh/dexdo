-- Partial index supporting the deferred-projection retry pass.
-- Scans raw_events that are decoded (event_type + decoded set) but not yet
-- projected (processed_at null), in chain-arrival order so out-of-order
-- parents get their first chance before children retry.
create index if not exists raw_events_pending_projection_idx
    on raw_events (created_at_chain, id)
    where processed_at is null
      and event_type is not null
      and decoded is not null;
