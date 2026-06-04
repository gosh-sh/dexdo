-- Per-list human-readable description, sourced from the
-- Oracle.OracleEventListDeployed event (carried in the event payload as
-- `description`). Nullable: event lists deployed before this field existed,
-- or replayed from pre-change history, carry NULL. Surfaced as
-- `/api/v1/oracles` eventLists[].description.
alter table oracle_event_lists
    add column description text;
