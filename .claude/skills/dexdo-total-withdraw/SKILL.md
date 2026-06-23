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
Anything still locked (an order/stake you didn't close) stays behind, so re-run
Steps 2–4 for it and withdraw again.

## Step 6 — Verify

```sh
$READ account --creds "$CREDS"                  # note should be ~0 free
"$DEXDO" stakes --pn-state-file "$STATE"        # no remaining stakes
tvm-cli -u shellnet.ackinacki.org account "${MULTISIG#0:}::${MULTISIG#0:}" | grep -E 'ecc|balance'  # multisig credited
```

The note's free balance should be ~0 and the multisig's `ecc` balance increased by
the swept amount. Leftover locked balance ⇒ a position/order wasn't closed; repeat.

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
