# Authentication: Backend Notes

This document contains implementation-facing notes for the DODEX authentication backend. The public HTTP contract — `X-DODEX-APIKEY` header, `signature`/`timestamp`/`recvWindow` query parameters, signature formula, and error codes — lives in [api-spec.md](../api-spec.md).

## User Model

A user interacts with the API through three concepts, each a separate entity in the backend:

| Concept | Description |
| --- | --- |
| `account` | Logical user identity. Identified by `accountId` (UUID). Stable across key rotation. The only identifier the public API exposes. |
| `trading PN` | One `PrivateNote` contract bound to the account at creation. Holds the user's balances on chain. The backend submits trades from this PN on the user's behalf. |
| `api_key` | Credential pair (`api_key` + `api_secret`) issued under an account. An account may hold several keys with different permissions. |

The mapping is:

```text
account (UUID)
  ├── trading PN (one, multi-asset)
  └── api_keys (one or more, each with its own permissions)
```

`accountId` is the only identifier that crosses the API boundary. Clients do not see the trading PN address or the api_key id; the chain side is internal to the backend.

## Trading Private Note

Each account is bound to exactly one trading PN, and every API-side trade is submitted from that PN. A `PrivateNote` contract is a multi-asset position. It can hold collateral tokens (NACKL, USDC, SHELL) and outcome tokens from any market the user has traded. The PN is deployed with one initial `tokenType`, but the on-chain `_balance` is a mapping keyed by `tokenType` — after deploy the PN accepts transfers of any other token type from PNs the user controls. One trading PN per account is therefore enough to participate in markets across any quote asset the user has funded.

Deposit and withdrawal flows are user-side and are outside the API. The user transfers funds into the trading PN from their own PNs via `init_transfer`; withdrawal moves funds out of the trading PN to a user-controlled destination. The HTTP-level contract for either flow is not yet defined in `api-spec.md`.

## Authentication

The HMAC contract — fields, formula, and error codes — is given in [api-spec.md §Security Types](../api-spec.md). The backend looks up the api_key, decrypts the matching api_secret, and recomputes the signature for each request. Verification covers:

1. Header `X-DODEX-APIKEY` is present and matches an `api_key` row with `disabled_at IS NULL`.
2. `timestamp` falls within `[now - recvWindow, now + 1s]` after clamping `recvWindow` to the spec maximum of `60000`.
3. The recomputed HMAC-SHA256 over `canonicalQueryString + canonicalRequestBody` matches the supplied `signature`.

The `canonicalQueryString` is built from the raw URL query by removing the `signature` parameter and lexicographically sorting the remaining `key=value` pairs without re-encoding. The body is taken byte-exact as transmitted; the backend never re-serializes JSON or reorders body keys. Signature comparison is constant-time.

Error mapping:

| Condition | Code |
| --- | --- |
| Missing header / unknown api_key / disabled key / missing permission | `-1002` |
| `recvWindow` expired | `-1021` |
| Signature mismatch | `-1022` |

The api_secret never travels in any request after issuance — only the signature does. Both api_secrets and PN signing keys are stored encrypted at rest under a backend-side master key loaded from the environment.

## Permissions

Each api_key carries a subset of `{USER_DATA, TRADE}`, matching the security levels in `api-spec.md`:

| Permission | Endpoints |
| --- | --- |
| `USER_DATA` | Read-only account endpoints (`/account`, `/openOrders`, `/allOrders`) |
| `TRADE` | Order placement and cancellation (`/order`, `/batchOrders`, `/openOrders DELETE`) |

A `USER_DATA`-only key on a `TRADE` endpoint returns `-1002`. This separation lets a user issue read-only keys (for a dashboard) and trading keys (for a bot) under the same account.

## Account Lifecycle

| Operation | Effect |
| --- | --- |
| Create account | Allocates a new `accountId` and binds the user's trading PN in a single operation. Both are required at creation — trading cannot start until the PN is bound. |
| Issue api_key | Generates an `api_key` and `api_secret` under an account, with chosen permissions. The `api_secret` is shown once at creation and cannot be recovered later. |
| Disable api_key | Marks the key disabled. Subsequent requests with that key return `-1002`. |

Provisioning is operator-only in this version. The HTTP contract for self-service account management is not yet defined in `api-spec.md`. The trading PN bound at account creation is used by the API for the lifetime of the account; replacing it is operator-only via direct database edit.

## Balance Source

The trading PN is the source of truth for user balances. The backend does not maintain its own balance ledger; balances are read from chain state when a client requests `/api/v1/account`.

| Field | Source |
| --- | --- |
| `balances[].free` (collateral) | `PrivateNote.getDetails()._balance[tokenType]` |
| `outcome_balances[].free` (outcome tokens) | `PrivateNote.getStakes()` |
| `balances[].locked` / `outcome_balances[].lockedInOrders` | Computed from the indexed `live_orders` read-model — sum of open orders owned by this PN |

The `_lockedInOrders` mapping inside `PrivateNote.sol` is not exposed by a public getter, so the locked amounts come from the indexed `live_orders` table rather than a chain getter. This stays consistent with on-chain state because the contract uses the same lock/release rules at order placement and at fill or cancel.

## Not Included

- Self-service account creation and key issuance over HTTP.
- A public deposit-address endpoint; `/account` does not yet return the trading PN address.
- A withdrawal endpoint.
- IP allow-lists per api_key.
- Subaccounts (multiple trading PNs under one account).
- API-side rotation of the trading PN.
