---
name: dexdo-trading
description: Place manual trades on DEX.DO prediction markets via the signed REST API — stake on an outcome for an amount (market buy), full-split collateral into a market's outcome tokens (buyFullSet), create a limit order in an outcome's order book, and cancel an order. Load when the user wants to bet/stake on an outcome, buy a full set / "fullsplit", place or cancel an order on DEX.DO. Spends real funds, so it confirms parameters before submitting. For read-only views (markets, depth, price, my orders) use the `dexdo-market-data` skill.
---

# DEX.DO — Manual Trading (signed TRADE API)

This skill submits **state-changing, fund-spending** trades to DEX.DO through the
signed `TRADE` endpoints. Three operations the user asks for, plus cancel:

1. **Stake on an outcome for an amount** — market-buy that outcome's token, spending
   a quote-asset amount → `POST /api/v1/order` (`type=MARKET`, `side=BUY`).
2. **Full split into a market** ("fullsplit") — split collateral into one token of
   every outcome → `POST /api/v1/buyFullSet`.
3. **Create an order in an order book** — a resting limit order →
   `POST /api/v1/order` (`type=LIMIT`).
4. **Cancel an order** — `DELETE /api/v1/order` by `orderId`.

Canonical contract: [`docs/api-spec.md`](../../../docs/api-spec.md) (§Trading,
§Position, §Validation Rules). Read-only lookups (find the market, see the book,
check fills) belong to **`dexdo-market-data`**; this skill calls the read client only
to validate inputs and to confirm results.

## Shared client + credentials

Same client as the read skill:

```sh
DODEX="python3 $PWD/.claude/skills/dodex-common/dodex_client.py"   # from repo root
```

Every endpoint here is `TRADE`-signed, so credentials are **required** — the
account's `<tt>.creds.json` from registration (`POST /api/v1/accounts`):

```sh
export DODEX_CREDS="$HOME/dexdo-workspace/notes/nackl.creds.json"
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
$DODEX markets --market-address "0:…"
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

## 1. Stake on an outcome (market buy)

"Stake N <quote> on <outcome> in <market>" = buy that outcome's token at market,
spending N of the quote asset. For a **MARKET BUY the `quantity` field is the
quote-asset amount to spend** (not outcome-token units), and `price` must be omitted:

```sh
$DODEX order --creds "$DODEX_CREDS" \
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

> A single-outcome **buy** only needs collateral. A single-outcome **sell** needs
> outcome tokens you already hold, which only exist after a full split (or a prior
> buy) on that market — otherwise the chain rejects with `ERR_STAKE_NOT_EXISTS`
> (`-2010`).

## 2. Full split into a market ("fullsplit" / buyFullSet)

Splits `collateral` of the market's quote asset into one outcome token of **every**
outcome. Holding the full set is economically equal to holding the collateral, and
it is the prerequisite for later **selling** any single outcome on the book.

```sh
$DODEX buy-full-set --creds "$DODEX_CREDS" \
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
$DODEX order --creds "$DODEX_CREDS" \
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
$DODEX cancel-order --creds "$DODEX_CREDS" \
  --market-address "0:…" --symbol "<symbol>" --order-id "<orderId>"
```

You need the chain-assigned `orderId` (from `dexdo-market-data` → `orders`).
Verified on dev (2026-06-23): placing a resting LIMIT order, reading its `orderId`
from `orders`, and cancelling it returns `PENDING_CANCEL` and frees the locked
collateral. Order attribution is eventually consistent, so if a just-placed order
isn't listed yet, retry the cancel once it appears (it surfaced within seconds on the
post-restart deployment, but can lag under indexer backlog).
(`DELETE /api/v1/openOrders` "cancel all on symbol", `sellFullSet`, and `claim` are
in the draft spec but **not deployed** on dev — don't call them.)

## Timeouts & retries

Trade endpoints submit an on-chain transaction before responding and can take tens
of seconds; the client waits up to 90s (`--timeout` / `$DODEX_HTTP_TIMEOUT`). On a
client timeout the operation **may still have been accepted** — do not blindly
resubmit. For `POST /api/v1/order`, resubmit with the **same** `--client-id`; the
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
