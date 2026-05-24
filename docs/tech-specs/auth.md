# Authentication Technical Specification

This document defines the implementation-facing requirements for DODEX authentication. The public HTTP contract — `X-DODEX-APIKEY` header, `signature`/`timestamp`/`recvWindow` query parameters, signature formula, and error codes — lives in [api-spec.md](../api-spec.md).

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

The HMAC contract — fields, formula, and error codes — is given in [api-spec.md §Security Types](../api-spec.md). The backend looks up the api_key, decrypts the matching api_secret, and recomputes the signature for each request. Authentication establishes *who* the caller is; checking whether the caller is *allowed* on a given endpoint is handled separately, in [Authorization](#authorization).

Verification runs in a fixed order. Each step fails closed with its own error code; later steps do not run:

1. **Envelope assembly** — the request must carry `X-DODEX-APIKEY`, query `timestamp` (parseable as `i64`), and a non-empty query `signature`. Query `recvWindow`, if present, must be parseable as `u64`. The request body is also capped at a server-side maximum (currently 64 KB); exceeding it returns `-1009 / HTTP 413` before any HMAC compute.
2. **Credential lookup** — the api_key must match a row with `disabled_at IS NULL`.
3. **Timestamp window** — `timestamp` must fall within `[now - recvWindow, now + 1s]` after clamping `recvWindow` to the spec maximum of `60000`. A missing `recvWindow` uses the server-side default.
4. **Signature** — the HMAC-SHA256 over `canonicalQueryString + canonicalRequestBody` must equal the supplied `signature` under constant-time comparison.

The `canonicalQueryString` is built from the raw URL query by removing the `signature` parameter and lexicographically sorting the remaining `key=value` pairs without re-encoding. The body is taken byte-exact as transmitted; the backend never re-serializes JSON or reorders body keys.

Error mapping:

| Step | Condition | Code | HTTP |
| --- | --- | --- | --- |
| 1 | Missing or malformed `X-DODEX-APIKEY`, `timestamp`, `signature`, or `recvWindow` | `-1003` | 401 |
| 1 | Request body exceeds the server-side cap | `-1009` | 413 |
| 2 | Unknown api_key or `disabled_at IS NOT NULL` | `-1002` | 401 |
| 3 | `timestamp` outside the (clamped) recvWindow | `-1021` | 401 |
| 4 | Signature mismatch | `-1022` | 401 |

The split between `-1003` and `-1002` is intentional: `-1003` says the server could not even attempt verification (client-side request-shape bug), while `-1002` says verification was attempted and the credential was rejected. Splitting them lets clients and ops distinguish broken SDKs from unauthorized callers.

The `msg` field never identifies which specific envelope field is missing or why a credential was rejected. It returns generic copy (`"Required auth parameter missing."` for `-1003`, `"Authentication required."` for `-1002`, `"Timestamp outside recvWindow."` for `-1021`, `"Invalid signature."` for `-1022`) so the response does not help an attacker probe the request shape. Specific reasons are recorded in server-side logs for alerting.

A malformed `recvWindow` (present but not a non-negative integer) is rejected with `-1003` rather than silently falling back to the default. Silent fallback would mask client SDK bugs and surface later as confusing `-1021` errors when the chosen default does not match the client's expected tolerance.

The api_secret never travels in any request after issuance — only the signature does. Both api_secrets and PN signing keys are stored encrypted at rest under a backend-side master key (`auth.kek_hex`) loaded from the service config. The committed `config/api.local.yaml` ships a shared dev value; stage and prod configs are assembled by CI from the secret store.

## Authorization

Authentication confirms identity; authorization decides whether that identity is allowed on a specific endpoint. The two run in series — authorization is only evaluated once authentication has produced an `AuthContext` — and they emit different error codes.

Each protected endpoint declares the permission it requires (see [Permissions](#permissions)). After the authentication pipeline succeeds, the endpoint checks the resolved api_key's permission set against that requirement. A failure here returns `-1002 AUTH_REQUIRED` with HTTP `401`, identical on the wire to a credential rejection. Clients cannot tell from the response whether their key is unknown, disabled, or simply lacking the right permission — this is intentional, see the §Authentication note on `msg` opacity.

Because authorization runs **after** authentication, a request that fails both checks (e.g., a `USER_DATA`-only key with a stale `timestamp` calling a `TRADE` endpoint) surfaces the authentication error first. The caller must produce a request that passes all four authentication steps before the authorization layer sees it and rejects on permission. This ordering is a deliberate authentication-before-authorization split — the same pattern used by OAuth, OIDC, and Kubernetes RBAC — not a bypass: an unauthorized request is rejected regardless of which check fires first.

The check itself is enforced through a single helper in the API service so that protected handlers cannot accidentally read the `AuthContext` without naming a required permission. The handler signature carries the permission as a function argument, which means a new protected endpoint cannot compile without declaring its authorization requirement.

## Permissions

Each api_key carries a subset of `{USER_DATA, TRADE}`, matching the security levels in `api-spec.md`:

| Permission | Endpoints |
| --- | --- |
| `USER_DATA` | Read-only account endpoints (`/account`, `/orders`) |
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

The trading PN is the source of truth for user balances. The backend does not maintain its own balance ledger; balances are read from chain state on every request to `/api/v1/account` and `/api/v1/account/balances`.

| Field | Endpoint | Source |
| --- | --- | --- |
| `balances[].free` (collateral) | `/api/v1/account` | `PrivateNote.getDetails()._balance[tokenType]` |
| `balances[].locked` (collateral) | `/api/v1/account` | `PrivateNote.getDetails()._lockedInOrders[tokenType]` |
| `balances[].free` (outcome tokens) | `/api/v1/account/balances` | `PrivateNote._stakes` — TVM Solidity auto-getter for the public mapping (returns the full `map(uint256 → StakeInfo)` in one call; the API looks up `map[hash]` after the chain returns). Key is `tvm.hash(abi.encode(eventId, oracleListHash, tokenType))`. Returned value is summed across the three stake pools (`amount`, `debtAmount`, `couponsAmount`) per outcome. |
| `balances[].lockedInOrders` (outcome tokens) | `/api/v1/account/balances` | Sum of `amount_remaining` over OPEN sell rows in the indexed `live_orders` read-model for this PN on the market's order book |

Collateral `free` and `locked` both come from one `getDetails()` call — the API runs the chain getter once per `/api/v1/account` request via the off-chain TVM executor (`tvm_runner::run_getter`). Outcome `free` comes from a second per-market getter call (`_stakes(hash)`) on `/api/v1/account/balances`. Outcome `lockedInOrders` is read from `live_orders` because the contract has no public getter for resting orders' outcome quantities — they live inside the `OrderBook` contract's internal order tree, not on the `PrivateNote` — so the indexed read-model is the only available source. See [read-api.md §Locked source split](read-api.md#locked-source-split) for the consistency consequences.

## Not Included

- Self-service account creation and key issuance over HTTP.
- A public deposit-address endpoint; `/account` does not yet return the trading PN address.
- A withdrawal endpoint.
- IP allow-lists per api_key.
- Subaccounts (multiple trading PNs under one account).
- API-side rotation of the trading PN.
