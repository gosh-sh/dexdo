-- Partial index supporting the OracleEventList reconciler. Targets
-- `oracle_events` rows whose metadata (`describe`, `trust_addr`) was not
-- carried by the `EventAdded` event and must be filled from the
-- `OracleEventList._events` getter on a separate cadence.
create index if not exists oracle_events_describe_pending_idx
    on oracle_events (eventlist_id)
    where describe is null;
