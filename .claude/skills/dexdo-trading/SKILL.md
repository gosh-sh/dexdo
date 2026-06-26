---
name: dexdo-trading
description: Place manual trades on DEX.DO prediction markets. Two distinct paths by market phase. STAKING phase — stake on an outcome (a real bet on a side) via the on-chain SDK binary (NOT the REST API, which has no staking endpoint). TRADING phase — order-book actions via the signed REST API: a market/limit order on an outcome, a full split (buyFullSet), and cancel. Load when the user wants to stake/bet on an outcome, "fullsplit", place/cancel an order, or take a position on DEX.DO. Spends real funds, so it confirms parameters before submitting. For read-only views (markets, depth, price, my orders) use the `dexdo-market-data` skill.
---

# DEX.DO — Manual Trading

This skill submits **state-changing, fund-spending** actions to DEX.DO. There are
**two different paths depending on the market's phase** — getting this right is the
whole point:

| Want to… | Market phase | How |
|---|---|---|
| **Stake / bet on a side** (back one outcome) | **STAKING** | on-chain `PrivateNote.setStake` via the **`dexdo` SDK CLI** (`dexdo stake`, §0). The REST API has **no** staking endpoint. |
| Take a position via the book (market/limit order) | **TRADING** | signed REST `POST /api/v1/prediction/order` (§2/§3) |
| Full split collateral into all outcomes | AWAITING_FREEZE / TRADING | signed REST `POST /api/v1/prediction/buyFullSet` (§4) |
| Cancel an order | TRADING | signed REST `DELETE /api/v1/prediction/order` (§5) |

> **The #1 thing to get right:** "make a stake into a market" (`сделать стейк в маркет`)
> during the **STAKING** phase is **NOT** a REST/order operation. The order book is
> closed in STAKING, so `POST /api/v1/prediction/order` and `buyFullSet` both return `-2010`.
> Staking is an on-chain library call — use `dexdo stake` (§0). Only once the market
> reaches **TRADING** do the order-book endpoints (§2–§5) apply.

Canonical REST contract: [`docs/api-spec.md`](../../../docs/api-spec.md) (§Trading,
§Position, §Validation Rules). The staking path is the SDK (`dodex-sdk`) →
`PrivateNote.setStake` → `PMP.acceptStake`. Read-only lookups (find the market, see
the book, check fills, check phase) belong to **`dexdo-market-data`**.

## Shared client + credentials

Same client as the read skill:

```sh
DEXDO="python3 $PWD/.claude/skills/dexdo-common/dexdo_client.py"   # from repo root
```

Every endpoint here is `TRADE`-signed, so credentials are **required** — the
account's `<tt>.creds.json` from registration (`POST /api/v1/accounts`; produce it with
the **`dexdo-register-account`** skill if it doesn't exist yet):

```sh
export DEXDO_CREDS="$HOME/dexdo-workspace/notes/nackl.creds.json"
```

The account trades in its PrivateNote's funded quote asset. Match the market's
`quoteAsset` to the creds: NACKL markets need the NACKL-funded note, etc. If the
user hasn't said which account, ask before spending anything.

## Autonomy / confirmation contract

Trades move real funds. Unlike the onboarding skills, **confirm before every
submission**:

1. Resolve the market and outcome first (read-only) so you trade the right symbol at
   valid precision — see *Pre-trade resolution*.
2. Show the user the exact order you are about to send (market, symbol, side, type,
   price, quantity, est. notional) and get a yes.
3. Submit one operation, then read back the result. Do not batch-fire multiple
   trades without re-confirming.

The signature, timestamp, special-character symbol encoding, and numeric-id rules
are handled by the client — you supply the trade parameters.

## Pre-trade resolution (always do this first)

Fetch the market to get the exact `symbol` and the outcome's trading constraints:

```sh
$DEXDO markets --market-address "0:…"
```

From the target outcome read: `symbol` (use verbatim), `pricePrecision`,
`quantityPrecision`, `tickSize`, `stepSize`, `minNotional`, `maxBatchSize`. Confirm
`status == "TRADING"` — order placement is rejected (`-2010`) in any other phase.
Validate the user's numbers against [§Validation Rules](../../../docs/api-spec.md#validation-rules)
**before** submitting (the client does not pre-validate; the backend and chain do):

- `price`: ≤ `pricePrecision` decimals **and** a multiple of `tickSize` (LIMIT only).
- `quantity`: ≤ `quantityPrecision` decimals **and** a multiple of `stepSize`.
- LIMIT: `price × quantity ≥ minNotional`. MARKET BUY: the quote `quantity ≥ minNotional`.

Check funds with `account` (free collateral for buys) / `balances` (outcome tokens
for sells) before placing.

## 0. Stake into a market (STAKING phase) — via the SDK binary, not the API

This is what "make a stake / bet on a side" means while the market is in **STAKING**.
It is an on-chain operation (`PrivateNote.setStake` → `PMP.acceptStake`) signed by the
user's note key — the REST API does not expose it. It is driven by the **`dexdo`** CLI
(one binary, subcommands) in the `dodex-sdk` workspace.

### Setup — build the `dexdo` CLI (one-time, shares the deposit skill's checkout)

Needs Rust + the `dexdo` checkout (same as the deposit skill). If you onboarded with
`dexdo-deposit-shellnet`, the checkout already exists; just build the CLI:

```sh
export WORKSPACE="${WORKSPACE:-$HOME/dexdo-workspace}"
cd "$WORKSPACE/dexdo/sdk" && cargo build --release --bin dexdo
DEXDO_CLI="$WORKSPACE/dexdo/sdk/target/release/dexdo"
"$DEXDO_CLI" --help        # lists subcommands (stake, pmp-details, …)
```

(If there is no checkout yet, run `dexdo-deposit-shellnet` Setup 0.2–0.5 first — it
clones `dexdo`, builds against `ackinacki-kit`, and produces the note state files this
CLI needs.) The `dexdo` CLI is **one entry point for all chain/library operations** —
staking today, with room for more (`claim`, `cancel-stake`, full-set ops, …) as new
subcommands; check `--help` for what's available.

### Inspect the market first (read-only)

`dexdo pmp-details` reads the market's phase window, outcomes, and identity straight
from the PMP — use it to resolve the outcome id and confirm the STAKING window:

```sh
"$DEXDO_CLI" pmp-details --market-address "0:<pmp>"
# → {phaseHint:"STAKING", numOutcomes, outcomeNames:{0:"Yes",1:"No"}, stakeStart/End, tokenLabel, …}
```

### Place the stake

The note's key signs the stake, so this reads the **note state file**
(`pn_state.<tt>.json` from onboarding — holds `pn_address` + the owner keypair), not
the API creds. `dexdo stake` reads the market's `eventId` / `oracleListHash` /
`tokenType` from the PMP on-chain, validates the **STAKING window**, scales `--amount`
by the quote asset's decimals, and submits:

```sh
"$DEXDO_CLI" stake \
  --market-address "0:<pmp>" \
  --pn-state-file  "$WORKSPACE/notes/pn_state.<tt>.json" \
  --outcome        <id> \
  --amount         <human> \
  --endpoint       shellnet.ackinacki.org
# outcome: 0-based (e.g. Yes=0, No=1 — see pmp-details). amount: human, e.g. 20 = 20 NACKL.
# add --use-coupon to spend coupon balance instead of clean balance.
```

It refuses (with a clear message, no spend) when the market is not approved, is
cancelled, staking hasn't started, the **staking window has closed** (then use the
order book, §2–§5), or the outcome id is out of range. On success it prints the
`tx_hash`; the staked amount leaves the note's free balance once the chain settles —
confirm with `dexdo-market-data` (`account` free drops by the staked amount).

> **Verified on dev (2026-06-23, post-restart):** `dexdo stake --outcome 1 --amount 20`
> on a STAKING market submitted on-chain and the note's free NACKL dropped 80 → 60
> (a second `--amount 10` stake then took it 60 → 50).

Resolve the outcome id and currency first with `pmp-details` (above) or
`dexdo-market-data` (`markets --market-address …` → `outcomes[].outcomeId` /
`outcomeName`, and `quoteAsset` → which note to sign with). Confirm market, outcome,
and amount with the user before running — staking spends real funds.

## 1. Take a position with a MARKET order (TRADING phase)

Once a market is **TRADING**, you can take a position on the book by market-buying an
outcome token. (This is order-book trading, **not** the STAKING-phase stake in §0.)
For a **MARKET BUY the `quantity` field is the quote-asset amount to spend** (not
outcome-token units), and `price` must be omitted:

```sh
$DEXDO order --creds "$DEXDO_CREDS" \
  --market-address "0:…" \
  --symbol "<marketName>-<OUTCOME>" \
  --side BUY --type MARKET \
  --quantity "<quote-amount>" \
  --client-id "<numeric-id>"    # quantity = quote spend (e.g. 10 = 10 NACKL); numeric id lets a timeout retry reuse it
```

Pass a numeric `--client-id` you choose (a u64) so that if the request times out you
can resubmit with the **same** id and the exchange deduplicates instead of placing a
second spend (see *Timeouts & retries*). Response is
`{clientOrderId, transactTime, status: PENDING_NEW}` (acceptance only).
The acquired outcome tokens show up in `balances` once the chain confirms. A market
buy takes liquidity from the asks; if the book is thin it may fill partially or move
the price — warn the user when the ask side is shallow (check `depth`/`price` first).
On a brand-new market with an empty ask side a market buy has nothing to match and is
canceled — place a LIMIT order (§3) to set the first price instead.

> A single-outcome **buy** only needs collateral. A single-outcome **sell** needs
> outcome tokens you already hold, which only exist after a full split (or a prior
> buy) on that market — otherwise the chain rejects with `ERR_STAKE_NOT_EXISTS`
> (`-2010`).

## 2. Full split into a market ("fullsplit" / buyFullSet)

Splits `collateral` of the market's quote asset into one outcome token of **every**
outcome. Holding the full set is economically equal to holding the collateral, and
it is the prerequisite for later **selling** any single outcome on the book.

```sh
$DEXDO buy-full-set --creds "$DEXDO_CREDS" \
  --market-address "0:…" \
  --collateral "<quote-amount>"        # e.g. 20  → split 20 NACKL across outcomes
```

Available while the market is `AWAITING_FREEZE` or `TRADING`. Any amount that does
not divide evenly across outcomes is refunded to free balance. Result becomes
visible via `account` (collateral debit) and `balances` (per-outcome credits).
Verified on dev (2026-06-23, post-restart): `buy-full-set 20` debited 20 NACKL and
credited the Yes/No outcome tokens split by current price.

> If `buyFullSet` ever returns `-1000` (HTTP 500, "Unknown error"), that is a
> backend-side failure, not a client bug — the request shape is correct per spec.
> It was observed transiently on an earlier deployment and cleared after the chain
> restart; if you hit it, verify state via `account` and report it as a dev-side
> issue rather than reshaping the request.

## 3. Create a limit order in an order book

```sh
$DEXDO order --creds "$DEXDO_CREDS" \
  --market-address "0:…" \
  --symbol "<symbol>" \
  --side BUY|SELL --type LIMIT \
  --price "<price>" --quantity "<qty>" \
  --tif GTC|IOC|FOK|POST_ONLY \
  --client-id "<numeric-id>"           # OPTIONAL — see below
```

- `--type` defaults to LIMIT and `--tif` to GTC if omitted.
- A BUY below the best ask (or SELL above the best bid) rests on the book; one that
  crosses matches immediately (use `POST_ONLY` to force resting / cancel-on-cross).
- **`--client-id` must be a numeric u64** if provided (the API validates
  `newOrderClientId` as `u64`; a non-numeric id is rejected with `-1130`). If you
  don't need to choose it, omit it — the server generates one and returns it as
  `clientOrderId`.
- Response is `PENDING_NEW` (acceptance only). The resting level appears on `depth`
  shortly; the attributed order appears on `orders` after indexing (which can lag —
  see the read skill's eventual-consistency note).

## 4. Cancel an order

```sh
$DEXDO cancel-order --creds "$DEXDO_CREDS" \
  --market-address "0:…" --symbol "<symbol>" --order-id "<orderId>"
```

You need the chain-assigned `orderId` (from `dexdo-market-data` → `orders`).
Verified on dev (2026-06-23): placing a resting LIMIT order, reading its `orderId`
from `orders`, and cancelling it returns `PENDING_CANCEL` and frees the locked
collateral. Order attribution is eventually consistent, so if a just-placed order
isn't listed yet, retry the cancel once it appears (it surfaced within seconds on the
post-restart deployment, but can lag under indexer backlog).
(`DELETE /api/v1/prediction/openOrders` "cancel all on symbol", `sellFullSet`, and `claim` are
in the draft spec but **not deployed** on dev — don't call them.)

## Network rate limit (429) — pace on-chain SENDS

The network throttles **message sends to the chain at >3 requests/second** → the
Block Producer returns **`429 Too Many Requests`** (surfaced by the SDK CLI as a
`tvm` error, code 621). This limit is on **on-chain writes — creating orders
(`order` / `dexdo place-order`), stakes (`dexdo stake`), full splits, cancels** —
**not** on reads (markets / depth / price / orders / stakes via `dexdo-market-data`,
which you can call freely).

So: **do not fire write ops in a burst.** Submit them **one at a time, spaced
(~1s+ between sends is enough to stay under 3 rps; a few seconds is safer)**, and
never run several order/stake commands in parallel. On a `429`, wait a few seconds
and retry that one op — it's a throttle, not a rejection. (A single SDK write may
itself issue a few chain queries, so leave margin rather than racing the limit.)

## Timeouts & retries

Trade endpoints submit an on-chain transaction before responding and can take tens
of seconds; the client waits up to 90s (`--timeout` / `$DEXDO_HTTP_TIMEOUT`). On a
client timeout the operation **may still have been accepted** — do not blindly
resubmit. For `POST /api/v1/prediction/order`, resubmit with the **same** `--client-id`; the
exchange deduplicates on the coid. For `buyFullSet`, verify via `account` before any
retry. API `-1007` carries the same "may have been accepted" meaning; `-2014`
(trading note busy) means retry shortly.

## Common errors (map for the user)

- `-2010` order would fail validation (bad price/qty vs tick/step/minNotional, market
  not `TRADING`, insufficient balance, sell without a prior split).
- `-1111` precision over the asset maximum.
- `-1130` malformed value (e.g. non-numeric `--client-id`).
- `-2013` account (PrivateNote) not deployed on-chain.
- `-2014` note busy — retry shortly. `-1007` timed out — may be accepted; reuse coid.
- `-1022` / `-1002` signature/auth — wrong creds file.

## Scope / out of scope

- **In:** market-buy stake, full split, limit order, cancel — for one account at a
  time, with confirmation.
- **Out:** reading markets/orders/prices (→ `dexdo-market-data`); registering a note
  / funding (→ onboarding/deposit skills); `sellFullSet` / `claim` / cancel-all
  (not deployed on dev).
