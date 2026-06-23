---
name: dexdo-total-withdraw
description: Close out a DEX.DO trading PrivateNote completely and sweep all funds back to the multisig wallet. Cancels every resting order, recovers stakes from still-open STAKING markets, merges held outcome tokens back into collateral (and claims resolved/cancelled markets), then withdraws all free collateral from the note to the multisig. Load when the user wants to "exit everything", "close all my positions and withdraw", "cash out", "total withdraw", or wind a note down before a network restart. Spends real funds and is destructive (drains the note) — confirms before the final withdraw.
---

# DEX.DO — Total Withdraw (close everything → sweep to multisig)

Winds a trading PrivateNote all the way down and moves the funds back to the
multisig. **All on-chain via the SDK** (the REST API does not expose stake/withdraw
and its order path is flaky); driven by the **`dexdo`** SDK CLI.

> **Why before a restart:** the season rules say funds not withdrawn before an
> announced Shellnet restart can be lost. This skill is the "get everything back to
> a wallet you control" button.

Order matters — collateral is locked inside orders/stakes/positions, so you must
**free it before sweeping**:

1. **Cancel resting orders** (unlocks buy-side collateral)
2. **Recover stakes** from still-open STAKING markets (cancel-stake) / **claim**
   resolved or cancelled markets
3. **Merge outcome tokens** back into collateral (merge-full-set)
4. **Withdraw** all free collateral → multisig

## Setup

Uses the same `dexdo` SDK CLI and note state files as the trading/onboarding skills
(build once per `dexdo-trading` §0):

```sh
export WORKSPACE="${WORKSPACE:-$HOME/dexdo-workspace}"
DEXDO="$WORKSPACE/dexdo/sdk/target/release/dexdo"
STATE="$WORKSPACE/notes/pn_state.<tt>.json"     # the note to drain (holds pn_address + owner key)
CREDS="$WORKSPACE/notes/<tt>.creds.json"        # only for the read views below (REST)
READ="python3 $PWD/.claude/skills/dexdo-common/dexdo_client.py"   # market-data client
MULTISIG="0:<your-multisig-address>"            # destination (from the deploy skill)
```

Signing uses the note's own key from `pn_state.<tt>.json`. Reads use the
`dexdo-market-data` skill.

## Step 1 — Inventory what's open (so you know what to close)

```sh
"$DEXDO" stakes --pn-state-file "$STATE"                         # stakes per market (by eventId)
$READ orders --creds "$CREDS"                                   # resting/open orders (all markets)
$READ account --creds "$CREDS"                                  # quote-asset free vs locked
$READ markets --status STAKING,TRADING,RESOLVING,RESOLVED,CANCELLED --limit 200   # phase per market
```

Map each `stakes` entry's `eventId`/`oracleListHash` to a market (via `markets`
→ `event.eventId`) to get its `marketAddress` and `status`. Build the close-list:
markets where you have **open orders**, **stakes**, or **outcome-token balances**
(`$READ balances --market-address …`).

## Step 2 — Cancel all resting orders (per market)

```sh
"$DEXDO" cancel-all-orders --market-address "0:<pmp>" --pn-state-file "$STATE"
```

Run once per market that has open orders. This frees the collateral locked in buy
orders (and removes sell orders). Pace the calls (see *Pacing*).

## Step 3 — Recover stakes / claim

- **STAKING (still open):** recover the stake back to collateral —
  ```sh
  "$DEXDO" cancel-stake --market-address "0:<pmp>" --pn-state-file "$STATE"
  ```
- **RESOLVED / CANCELLED:** claim the payout / refund —
  ```sh
  "$DEXDO" claim --market-address "0:<pmp>" --pn-state-file "$STATE"
  ```

Pick per the market's `status` from Step 1. (`cancel-stake` only works while the
market is still in its STAKING window; after that the position settles via `claim`
once the market resolves.)

## Step 4 — Merge outcome tokens back into collateral

If `balances` shows outcome tokens on a market (e.g. from a full split or fills),
merge them back. `--amounts` is a comma list of per-outcome amounts in order of
`outcomeId` (use the per-outcome `free` from `$READ balances`):

```sh
"$DEXDO" merge-full-set --market-address "0:<pmp>" --pn-state-file "$STATE" --amounts 8.78,11.22
```

The contract merges the largest full set it can from those amounts and leaves the
remainder; collateral returns to the note's free balance.

## Step 5 — Withdraw everything to the multisig

Once orders/stakes/positions are closed, sweep the note's **free** collateral to the
multisig. `--dapp-id` defaults to the destination's account-id (a multisig deploys
under its own account-id as dApp id); pass it explicitly only if different:

```sh
"$DEXDO" withdraw --pn-state-file "$STATE" --dest "$MULTISIG"
```

**Confirm with the user before this step** — it moves all the note's free funds out.
`withdraw_tokens` sweeps the note's free balance of every token type to `--dest`.

> **Hard precondition (contract invariant).** `PrivateNote.withdrawTokens` requires the
> note to be **fully flat first**: `require(_stakes.empty())`, no open orders, nothing
> `lockedInOrders`, no pending place/batch locks, and `_debt == 0`. If ANY stake or
> order remains, the withdraw reverts on the compute phase (TVM **exit 121** /
> `ERR_INVALID_STATE`-class) — it does **not** do a partial sweep. So Steps 2–4 must
> clear **every** market the note touched, not just some.
>
> **The catch:** `cancel-stake` only works while a market is still in **STAKING**.
> A stake in `AWAITING_FREEZE` / `TRADING` / `RESOLVING` can't be cancelled — it is
> released only when the market reaches `RESOLVED`/`CANCELLED` and you **`claim`** it.
> So if the note staked a market that has moved past STAKING and hasn't resolved yet,
> **withdrawal is blocked until that market resolves** (then `claim`, then withdraw).
> Enumerate with `dexdo stakes` + map each `eventId` to its market `status`; if any
> are stuck mid-lifecycle, tell the user the sweep must wait for resolution.
>
> `--dapp-id` is a `uint256` — the CLI passes the destination's account-id **0x-prefixed**
> by default (a bare hex account-id fails to parse as a decimal). Override only if the
> wallet's real dApp id differs.

## Step 6 — Verify

```sh
$READ account --creds "$CREDS"                  # note should be ~0 free
"$DEXDO" stakes --pn-state-file "$STATE"        # no remaining stakes
tvm-cli -u shellnet.ackinacki.org account "${MULTISIG#0:}::${MULTISIG#0:}" | grep -E 'ecc|balance'  # multisig credited
```

The note's free balance should be ~0 and the multisig's `ecc` balance increased by
the swept amount. Leftover locked balance ⇒ a position/order wasn't closed; repeat.

## Step 7 — Report to the user (and the "stuck stakes" table)

Tell the user plainly whether the sweep completed:

- **Fully withdrawn:** note ~0, multisig credited — done.
- **Blocked by stuck stakes:** the note still has stakes in markets that are **past
  STAKING but not yet resolved** (`AWAITING_FREEZE` / `TRADING` / `RESOLVING`). These
  can't be cancelled and can't be claimed yet, so `withdraw` will keep reverting
  (exit 121) until each resolves. **Show a table of exactly where the funds are stuck
  and when they free up — in the user's LOCAL time.**

Build it from `dexdo stakes` joined to `markets`. **Caveat (verified):** the top-level
key in `dexdo stakes` is **not** the market's `event.eventId` — it is an opaque
`tvm.hash(abi.encode(eventId, oracleListHash, tokenType))`, so you **cannot** join it
to `markets` by `event.eventId`. Map each stake to its market instead by:

- **what you staked** — in this same session you placed the stakes with
  `dexdo stake --market-address … --outcome … --amount …`, so you already know each
  stake's market; match on the `oracleListHash` + per-outcome `amount` shown in
  `dexdo stakes` against those calls; **or**
- **enumeration** — for each candidate market, pull `pmp-details` (`eventId`,
  `oracleListHash`, `tokenType`) and recognise it by `oracleListHash` + the amounts.

(Known gap to close: `dexdo stakes` should emit the resolved `marketAddress`/`status`
per stake — it can't be reversed client-side from the hash key alone.)

Once mapped, the unlock moment is when the market resolves: earliest at
`timings.resultStart` (resolution window opens), and no later than `timings.resultEnd`
(deadline → `EXPIRED`, then claimable). Convert those unix-seconds to the user's local
time for display:

```sh
# local time for a unix-seconds value (macOS/BSD: date -r ; GNU: date -d @)
date -r 1782310222 '+%Y-%m-%d %H:%M %Z' 2>/dev/null || date -d @1782310222 '+%Y-%m-%d %H:%M %Z'
```

Render, e.g.:

| Market | Status | Your stake | Claimable from (local) | Hard deadline (local) |
|---|---|---|---|---|
| Group Stage — USA | RESOLVING | No 11.22 NACKL | 2026-06-25 09:30 CEST | 2026-06-26 09:30 CEST |
| Rosengård W vs Djurgården | AWAITING_FREEZE | 0/1 split | 2026-06-25 12:00 CEST | 2026-06-26 12:00 CEST |

Then state the next action: once a row reaches `RESOLVED`/`CANCELLED`, run
`dexdo claim --market-address <that market> --pn-state-file "$STATE"`, and after the
**last** stake clears, re-run `dexdo withdraw` to sweep the rest to the multisig.
Reassure the user the funds are safe on the note in the meantime — nothing is lost,
it's just time-locked by the market lifecycle.

## Pacing / errors (be gentle)

Each step is an on-chain transaction (a network SEND) signed by the note. The network
throttles **sends at >3 requests/second** → the Block Producer returns
**`429 Too Many Requests`** (surfaced by the CLI as a `tvm` error, code 621). The
limit is on **on-chain writes** (cancel / stake / merge / withdraw), **not** on the
read views used to inventory (Step 1) — those you can call freely.

So run the close/withdraw steps **one at a time, spaced a few seconds apart** (never
in parallel); on a `429`, wait a few seconds and retry that one step (it's a
throttle, not a rejection). Other real errors the SDK surfaces (vs the REST `-1000`):
`exit 160` order/amount too small, `exit 129` invalid state for the op. A step that
fails leaves the rest closeable — fix and continue; the flow is resumable.

## Scope / out of scope

- **In:** cancel orders, recover stakes, claim, merge full sets, withdraw a note's
  collateral to the multisig — one note at a time, with confirmation before the sweep.
- **Out:** opening positions / trading (→ `dexdo-trading`); reading data
  (→ `dexdo-market-data`); moving funds off the multisig to an external wallet
  (separate flow). Withdrawal to anything other than a wallet you control.
