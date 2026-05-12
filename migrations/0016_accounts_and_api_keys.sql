-- Identity, custody, and credential storage for USER_DATA / TRADE requests.
-- See docs/tech-specs/auth.md for the user model and the auth contract.

create extension if not exists pgcrypto;

create type auth_permission as enum ('USER_DATA', 'TRADE');

-- One logical user. Holds the custodied trading PN inline; multiple PNs
-- per account are not supported in this version. Replacing the PN is
-- operator-only via direct UPDATE on this row.
create table accounts (
    id              uuid               primary key default gen_random_uuid(),
    label           text,
    pn_address      text               not null unique,
    pn_pubkey       numeric(78, 0)     not null,
    pn_seckey_enc   bytea              not null,
    pn_dih          numeric(78, 0)     not null unique,
    disabled_at     timestamptz,
    created_at      timestamptz        not null default now()
);

-- API credentials. Multiple per account, each with its own permissions.
-- api_secret is generated at issuance and stored only as ciphertext.
create table api_keys (
    id              bigserial          primary key,
    account_id      uuid               not null references accounts(id) on delete cascade,
    api_key         text               not null,
    api_secret_enc  bytea              not null,
    permissions     auth_permission[]  not null default array['USER_DATA'::auth_permission],
    disabled_at     timestamptz,
    last_used_at    timestamptz,
    created_at      timestamptz        not null default now()
);

-- Active api_key must be unique; disabled rows may share api_key strings
-- (irrelevant in practice with 256-bit random keys, but the partial
-- predicate is the precise invariant).
create unique index api_keys_api_key_active_idx
    on api_keys (api_key) where disabled_at is null;

create index api_keys_account_id_idx on api_keys (account_id);
