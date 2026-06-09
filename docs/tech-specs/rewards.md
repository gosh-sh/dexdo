# Rewards Service Technical Specification

`dodex-rewards` is a standalone service that turns indexed on-chain activity into
season **points balances** per user, following the DEX.DO rewards table
(Season 1 · Shellnet + market makers).

This document is the authoritative spec and lives here because the service reads
the indexer's read-model. The service does **not** modify `dodex-backend`; it only
reads `raw_events` and `accounts` read-only (see [Data sources](#data-sources)).

## Glossary

**Subject** — the technical point-earning entity: a `PrivateNote` contract
address. Every on-chain event the service consumes carries one.

**Real user** — the wallet contract (multisig / multifactor) behind one or more
PrivateNotes. Resolved from the subject through the [identity port](#3-identity-model);
the link source is an [open question](#open-questions).

**Principal** — the entity an award is scoped and attributed to: the PrivateNote
for per-subject rules, and the real user (resolved wallet, else the PrivateNote)
for per-user rules. Balances are keyed by principal, with PrivateNote principals
rolled up to their wallet when a link exists.

**Reducer** — a pure function `(event, prior_state) -> (point_deltas, new_state)`
implementing one rewards-table row. Deterministic and unit-testable.

**Award** — one idempotent point grant: a row in `point_awards` keyed by
`(rule_id, dedupe_key)`. One-time / per-market / capped semantics are enforced by
the uniqueness of that key.

**Fold** — a single pass over the ordered event stream that dispatches each event
to every active reducer. Re-running the fold from genesis is the full recompute.

**Synthetic event** — an off-chain, manual, or sampled input lifted into the same
event stream by a source adapter, so all reward archetypes are processed
uniformly.

## Scope

Fully specified:

- **Season 1 · Shellnet ON-CHAIN** — the 9 mechanics in the rewards table.
- **Season 1 OFF-CHAIN** — referral, bug bounty.
- **Market-maker tester (Shellnet)** — maker-fill daily-cap override + manual grant.

Architecture seams + data requirements only (no economics):

- **MM · Mainnet** — LP emission scoring, NACKL pools, rebates, vaults. See
  [Mainnet seams](#8-mainnet-seams-and-data-requirements).

Out of scope:

- Actual reward / token distribution (the service computes points, it does not pay
  them out).
- A public ledger or leaderboard surface — the external surface is balances +
  per-mechanic breakdown only.
- Changes to the indexer or contracts, except the deferred PN→wallet linkage path
  if that open question is resolved on-chain.

## Reference: rewards table

Source: `DEX.DO Season Points Rewards`. Reward numbers in the table are flagged
*illustrative* by their authors; final values are an [open question](#open-questions)
and live in season config, not in code.

## Architecture

```
 raw_events (read-only) ─┐
 off-chain source port ──┤
 manual-grant port ──────┼─▶ merge into one ordered stream ─▶ fold ─▶ reducers (per season config)
 (later) sampling port ──┘                                                   │
                                                                             ▼
                          identity port (PN→wallet, for per-user rules) ─▶ point_awards (idempotent)
                                                                             │
                                                                             ▼
                                                              subject_balances ─▶ axum HTTP API
```

- **Reducers** are pure and registered per season; the engine owns ordering,
  persistence, idempotency, and cursoring.
- **Source adapters** convert non-event inputs into synthetic events so on-chain,
  off-chain, manual, and (later) sampled rewards flow through one path.
- **Ports** are traits with deferred implementations: `IdentityResolver`
  (PN→wallet), `OffchainSource` (referrals / app logins), `ManualGrants`.
- **Season config** (`config/rewards.<env>.yaml`) selects active reducers, their
  point values and caps, the season window and timezone, and the MM address list.

Stack: Rust, `sqlx` (own Postgres schema), `axum` (HTTP). Mirrors `dodex-backend`
conventions so event types and ABI decoding can be shared as a crate.

## 3. Identity model

- **Awards are scoped to a principal.** Per-subject rules use the PrivateNote as
  the principal; per-user rules use the resolved wallet, falling back to the
  PrivateNote when no link exists. `point_awards.principal` records this, so its
  dedupe key carries the right granularity at compute time.
- **Balances roll up to the wallet.** `subject_balances` sums awards per principal
  and folds PrivateNote principals into their wallet wherever `identity_links`
  provides one. A link arriving mid-season is absorbed by a full recompute, which
  re-derives both the per-user dedupe and the roll-up under the same wallet.
- **Per-user rules** are unique-market counting in staking, the "only against
  another account" guard on fills, and referral qualification. All other rules are
  per-subject. The per-user-vs-per-subject granularity of the one-time rules other
  than staking is an [open question](#open-questions) — the table only states
  "по юзеру" for staking.
- **The PN→wallet link is not observable on-chain today.** `PrivateNote.withdrawTokens`
  (`contracts/PrivateNote.sol:1355`) passes `destWalletAddr` as an internal
  function-call argument to `RootPN.withdrawTokens` (`contracts/RootPN.sol:440`);
  neither emits an external event, and the indexer subscribes only to
  `blockchain.events` (`crates/infrastructure/src/graphql.rs:14`). Obtaining the
  link is an [open question](#open-questions) (contract event vs. indexer
  internal-message capture vs. app-provided mapping).
- **Fallback:** with no link available, each PrivateNote is treated as its own
  user. This is an explicit Season-1 limitation, not a silent default — it is
  surfaced in the balance response (`identity_resolved: false`) and logged.

Withdrawals are optional and usually late (often after the season), so even with an
on-chain source many subjects will never resolve to a wallet during the season.
The design therefore never blocks point accrual on identity resolution; roll-up is
applied where data exists and re-applied on recompute as links arrive.

## 4. Season 1 rule catalog

Each rule is one reducer. The "dedupe key" column defines the `point_awards`
uniqueness that enforces one-time / per-market / capped semantics. The `user` in a
per-user dedupe key is the [principal](#3-identity-model) (resolved wallet, else the
PrivateNote); `subject` is the PrivateNote.

| # | Rule | Trigger event(s) | Subject | Points | Dedupe key | Cap / guard |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | Stake in PMP | `PMP.StakeAccepted{note, outcomeId, amount, betType}` (src = PMP = market) | `note` | 1 / unique market | `(user, market)` | oracle-list markets only; unique market, not bet count |
| 2 | buyFullSet | `PMP.SplitProcessed{note, collateral}` | `note` | 5 | `(subject)` | one-time |
| 3 | Maker-fill | `OrderBook.OrderFilled{orderId, isTaker=false}` + owner lookup | owner PN | 1 / fill | `(subject, orderbook, fill_seq)` | daily cap (default 50, MM override); self-trade excluded |
| 4 | Taker-fill | `OrderBook.OrderFilled{orderId, isTaker=true}` + owner lookup | owner PN | 1 / fill | `(subject, orderbook, fill_seq)` | same |
| 5 | timeInForce coverage | `PrivateNote.OrderSubmitted{flags}` (src = PN) | src | 10 | `(subject)` | one-time, once all 4 TIF seen |
| 6 | Batch orders | `PrivateNote.OrderSubmitted` grouped by submission (≥2) | src | 3 | `(subject)` | one-time |
| 7 | Cancel flow | `PrivateNote.OrderCancelledConfirmed{orderBook, orderId, ...}` (src = PN) | src | 3 | `(subject)` | one-time |
| 8 | sellFullSet | `PMP.MergeProcessed{note, collateral}` | `note` | 3 | `(subject)` | one-time |
| 9 | Claim payout | `PMP.ClaimProcessed{note, payout, win}` (src = PMP = market) | `note` | 5 | `(subject, market)` | one-time per terminal market |
| 10 | Referral | synthetic `ReferralRegistered{referrer, referred}` + referred's first on-chain stake/trade | referrer | 10 / qualified referral | `(referrer, referred)` | referred must act; cap on referral count |
| 11 | Bug bounty | synthetic `ManualGrant{subject, points, reason}` | subject | fixed grant | `(grant_id)` | manual moderation; criteria out of band |
| 12 | MM tester | maker-fill cap override (config) + synthetic `ManualGrant` | MM PN | raised cap + grant | n/a | targeted invite; address known in advance |

### Rule details that need precision

- **Staking (1).** Subject is `note`; market is the event source (PMP). Count only
  markets in the oracle list — those reachable via an oracle EventList
  (`oracle_events.confirmed_pmp_address` / the markets the indexer linked to an
  oracle). Award once per `(user, market)`.
- **Maker/taker (3, 4).** `OrderBook.OrderFilled` carries `isTaker` but not the
  owner. The service builds its own `(orderbook, orderId) → owner PN` map from
  `PrivateNote.OrderPlacedConfirmed{orderBook, orderId}` (src = owner PN) and
  attributes each fill. `fill_seq` is a per-order monotonic counter derived from
  `chain_order` so repeated partial fills each award once and replay is idempotent.
  **Self-trade exclusion** requires pairing the maker leg with the taker leg of the
  same match (shared `clearingPrice` within one clearing) and dropping the award
  when both legs resolve to the same real user — see [open questions](#open-questions)
  for the pairing approach. The **daily cap** is per subject per season-timezone
  day; the MM override raises the cap for configured addresses.
- **timeInForce coverage (5).** Decode the `flags` byte
  (`contracts/OrderBook.sol:151`): `GTC` = none of IOC/FOK/POST_ONLY set,
  `IOC = flags & 0x01`, `FOK = flags & 0x02`, `POST_ONLY = flags & 0x08`
  (`FLAG_MARKET = 0x04` is order type, not a TIF). Maintain a 4-bit coverage set per
  subject; award 10 once all four are seen.
- **Batch orders (6).** "Several orders in one request" maps to ≥2 orders submitted
  in one transaction. Detected by grouping `OrderSubmitted` events that share an
  originating transaction; if transaction grouping is unavailable from the gateway,
  fall back to write-api batch data. The detection mechanism is an
  [open question](#open-questions).
- **Claim (9).** Award once per `(subject, terminal market)`. `win` is recorded for
  analytics but does not gate the award (the table credits claiming a settled
  position).
- **Referral (10).** Hybrid: the referrer→referred relationship comes from the
  off-chain port; qualification is the referred subject's first on-chain
  `StakeAccepted` or `OrderFilled`. The referrer is credited once per referred
  user, up to a configured cap.

## 5. Service data model (own Postgres schema)

| Table | Purpose |
| --- | --- |
| `reward_cursor(stream, chain_order)` | Processing position over `raw_events`, one row per stream. |
| `point_awards(id, principal, rule_id, dedupe_key, points, season, award_day, event_ref, created_at)` | Append-only grants. `principal` is the award's scope (PrivateNote or wallet, see [identity model](#3-identity-model)). `UNIQUE(rule_id, dedupe_key)` is the idempotency and one-time/per-market enforcement point. |
| `subject_balances(principal, season, total, breakdown_jsonb, updated_at)` | Materialized `SUM(points)` and per-`rule_id` breakdown, with PrivateNote principals rolled up to their wallet via `identity_links`. |
| `rule_progress(principal, rule_id, state_jsonb)` | Reducer state that is not itself an award: TIF coverage bitset, daily counters, seen-market sets. |
| `identity_links(pn_address, wallet_address, source, linked_at)` | PN→wallet, populated by the identity port. |
| `referrals(referrer, referred, status, registered_at)` | Off-chain referral relationships and their qualification status. |
| `manual_grants(grant_id, subject, points, reason, created_by, created_at)` | Operator-entered grants (bug bounty, MM grants). |

Season and MM configuration (active rules, point values, caps, window, timezone,
MM address list) live in `config/rewards.<env>.yaml`, not the database, so a config
change plus full recompute is the supported way to adjust illustrative numbers.

## 6. Processing model (hybrid)

- **Incremental.** Poll `raw_events WHERE chain_order > :cursor ORDER BY chain_order
  ASC` in batches, fold each event through the active reducers, insert awards
  idempotently (`ON CONFLICT (rule_id, dedupe_key) DO NOTHING`), update
  `subject_balances`, and advance the cursor in the same transaction. This keeps
  balances near real-time and mirrors the indexer's cursor pattern.
- **Full recompute.** Truncate the derived tables (`point_awards`,
  `subject_balances`, `rule_progress`), reset the cursor to genesis, and re-fold.
  Deterministic because every award is a pure function of the ordered event stream
  and its dedupe key. Triggered on rule/config changes or to repair drift.
- **Ordering.** On-chain events are ordered by `raw_events.chain_order` (the same
  strict-monotonic key the indexer uses). Synthetic events (off-chain, manual) are
  ordered by their own timestamp and merged deterministically; their correctness
  does not depend on interleaving with on-chain order (e.g. referral qualification
  only asks whether the referred subject has ≥1 qualifying on-chain action by a
  point in time).

## 7. HTTP API

Minimal surface — balances and operations only.

| Endpoint | Purpose |
| --- | --- |
| `GET /v1/balances/{subject}` | `{ subject, season, total, identity_resolved, breakdown: { rule_id: points } }`. `subject` accepts a PrivateNote address or a resolved wallet address. |
| `GET /v1/health` | Liveness / readiness. |
| `POST /v1/admin/recompute` | Trigger a full recompute (operator-only). |

Consumer authentication is an [open question](#open-questions); the likely default
is an API-key scheme consistent with `dodex-backend`'s auth.

## 8. Mainnet seams and data requirements

The Mainnet MM block is a different shape from Season 1 and is **not** implemented
here; the engine only reserves the seams.

- **Continuous order-book sampling.** LP scoring needs closeness-to-midpoint × size
  × time-in-book, which requires periodic snapshots of book state — not events.
  This is a future **sampling adapter** producing synthetic `BookSample` events (or
  a dedicated snapshotter). The indexer does not sample today
  (`order_book_snapshots` is reserved and unused).
- **Cross-subject distribution.** LP rewards are a daily-pool pro-rata split across
  market makers — a periodic aggregation over accumulated scores, not a per-event
  reducer. The engine reserves a **periodic distribution hook** for this; per-event
  reducers alone cannot express it.
- **Deferred economics.** NACKL emission, two-sided multiplier (×1 / ÷3), maker
  rebate fee-share, sponsor liquidity, founding-MM circle, and the HLP vault are
  specified separately once the explanatory note lands and the
  [open questions](#open-questions) (fee-share when the quote asset is not a
  stablecoin) are resolved.

## Open questions

1. **PN→wallet link source** — contract `emit Event` in `withdrawTokens` vs.
   indexer internal-message capture vs. app-provided mapping. Affects every
   per-user rule. Deferred.
2. **Off-chain source** — how referral / login data reaches the service (app push,
   read-only app DB, operator import). The app is still being built. Deferred.
3. **Bug-bounty acceptance criteria** — separate document; gates manual grants.
4. **MM tester** — cap-override values and the invited-address list.
5. **Final reward numbers** — table values are illustrative; confirm before launch.
6. **Batch detection** — transaction grouping from the gateway vs. write-api batch
   data.
7. **Self-trade pairing** — how maker/taker legs of one match are paired to exclude
   same-user fills from rules 3/4.
8. **Mainnet** — sampling cadence, scoring/emission formulas, fee-share on
   non-stable quote assets.
9. **API auth** — consumer authentication scheme.
10. **Season window** — exact start/end and the timezone used for daily-cap day
    boundaries (table says start ~10 June, 1 month).

## Testing strategy

- **Reducer unit tests** — pure functions: an event sequence in, expected awards
  out. Cover caps, one-time semantics, TIF coverage, per-market dedup, and
  self-trade exclusion.
- **Determinism test** — incremental processing and a full recompute over the same
  `raw_events` produce identical balances.
- **DB-backed tests** — idempotency (replaying the same events grants no extra
  points) and daily-cap day-boundary behavior, against a disposable Postgres.
