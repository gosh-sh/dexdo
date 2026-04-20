# GraphQL Queries in bee_dex

All queries go through `tvm_client::net::query` which hits the blockchain GraphQL endpoint.

## Direct GQL Queries

### 1. Discover PN deploy events (discovery.rs)

Scans RootPN external events to find PrivateNote deployments.
Used by `discover_my_notes()`.

```graphql
query($address: String!, $dst: String!, $last: Int!, $before: String) {
  blockchain {
    account(address: $address) {
      events(dst: $dst, last: $last, before: $before) {
        edges {
          node {
            msg_id
            created_at
            dst
            body
          }
        }
        pageInfo {
          startCursor
          hasPreviousPage
        }
      }
    }
  }
}
```

| Variable | Value | Description |
|----------|-------|-------------|
| `address` | `0:1010...1010` (RootPN) | Source contract |
| `dst` | `:0000...0065` (event ID 101) | `PrivateNoteDeployed` external event |
| `last` | 50 | Page size |
| `before` | cursor or null | Pagination (reverse, newest first) |

Event body decoded via `PrivateNoteDeployedData`:
- `deposit_identifier_hash` (String)
- `note_address` (String)
- `initial_balance` (u128)

### 2. Discover oracle deploy events (market.rs)

Scans RootOracle external events to find all oracles.
Used by `discover_oracles()`, `discover_markets()`, `discover_active_markets()`.

```graphql
# Same query structure as above
```

| Variable | Value | Description |
|----------|-------|-------------|
| `address` | `0:1515...1515` (RootOracle) | Source contract |
| `dst` | `:0000...0088` (event ID 136) | `OracleDeployed` external event |

Event body decoded via `OracleDeployedData`:
- `oracle` (String) — oracle contract address
- `pubkey` (String) — oracle owner public key
- `name` (String) — oracle name

### 3. PN history events (history.rs)

Scans PrivateNote external events for transaction history.
Used by `get_notes_history()`.

```graphql
query($address: String!, $last: Int!, $before: String) {
  blockchain {
    account(address: $address) {
      events(last: $last, before: $before) {
        edges {
          node {
            msg_id
            created_at
            dst
            body
          }
        }
        pageInfo {
          startCursor
          hasPreviousPage
        }
      }
    }
  }
}
```

| Variable | Value | Description |
|----------|-------|-------------|
| `address` | PN address | Source PrivateNote contract |
| `last` | limit | Page size |
| `before` | cursor or null | Pagination |

**No `dst` filter** — returns all event types from this PN. Decoded via `DecodedPrivateNoteEvent` which dispatches by `dst` (event ID):

| Event ID | Type | Key fields |
|----------|------|------------|
| 111 | PmpDeployed | event_id, token_type, pmp_address |
| 112 | OwnerChanged | old_pubkey, new_pubkey |
| 113 | StakeConfirmed | stake_controller, outcome, amount, bet_type |
| 114 | ClaimAccepted | stake_controller, outcome, payout |
| 115 | StakeCancelled | stake_controller, value |
| 116 | FullSetStakeConfirmed | stake_controller, amount[] |
| 117 | FullSetStakeCancelled | stake_controller, value |
| 149 | TransferInitiated | dest, token_type, amount |
| 150 | TransferReceived | from, token_type, amount |

## Kit Get Methods (TVM local execution)

These call contract getters via `tvm_client` — internally they encode a message,
run it against the account state, and decode the result. No GQL query visible,
but they do hit the network to fetch account state.

### RootPN getters

| Method | Params | Returns | Used by |
|--------|--------|---------|---------|
| `getPMPAddress` | event_id, names[], token_type | pmp_address | `get_pmp_address()`, PMP address verification |
| `getPrivateNoteAddress` | deposit_identifier_hash | private_note_address | `get_private_note_address()` |
| `getDetails` | — | pmp_code_hash, pn_code_hash, owner, balance | (not exposed) |

### PrivateNote getters

| Method | Params | Returns | Used by |
|--------|--------|---------|---------|
| `getDetails` | — | deposit_id_hash, ephemeral_pubkey, balance{}, busy, coupons, has_withdrawn | `get_private_note_details()`, `discover_my_notes()` |
| `getStakes` | — | stakes{} | `get_stakes()` |

### PMP getters

| Method | Params | Returns | Used by |
|--------|--------|---------|---------|
| `getDetails` | — | name, token_type, event_id, oracle_list_hash, total_pool, approved, num_outcomes, resolved_outcome, timings, outcome_names, ... | `get_pmp_details()`, `discover_markets()` |

### Oracle getters

| Method | Params | Returns | Used by |
|--------|--------|---------|---------|
| `getEventListAddress` | index | address | `get_event_list_address()` |

### RootOracle getters

| Method | Params | Returns | Used by |
|--------|--------|---------|---------|
| `getOracleAddress` | name | oracle_address | `get_oracle_address()` |

### OracleEventList getters

| Method | Params | Returns | Used by |
|--------|--------|---------|---------|
| `_events` | — | events{} (HashMap<id, EventInfo>) | `get_events()`, `get_parsed_events()` |

## Pagination

All GQL queries use **reverse pagination** (`last` + `before`):
- First page: `last=N, before=null` → newest N events
- Next page: `last=N, before=startCursor` → older events
- `hasPreviousPage=false` → no more data

## Well-known addresses

| Contract | Address | Event dst format |
|----------|---------|-----------------|
| RootPN | `0:1010...1010` | `:0000...00{hex(event_id)}` |
| RootOracle | `0:1515...1515` | `:0000...00{hex(event_id)}` |
