-- Fields that PMPDeployed alone cannot fill in (they live in PMP storage and
-- come from getDetails() reconciliation). Until reconciliation runs, the row
-- has to exist with these as NULL.
alter table markets alter column market_id drop not null;
alter table markets alter column name drop not null;
alter table markets alter column oracle_list_hash drop not null;

-- Reconciliation bookkeeping.
alter table markets add column if not exists last_reconciled_at timestamptz;

-- Auxiliary data from PMPDeployed for the reconciliation worker / market
-- outcomes projector. Stored as jsonb to avoid a 1:N join table at this stage.
alter table markets add column if not exists oracle_event_lists_json jsonb;
alter table markets add column if not exists oracle_fee_json jsonb;

create index if not exists markets_pending_reconcile_idx
    on markets (last_reconciled_at)
    where last_reconciled_at is null;
