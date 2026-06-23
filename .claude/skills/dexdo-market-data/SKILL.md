---
name: dexdo-market-data
description: Read DEX.DO market data and a trader's own orders from the indexer/read API, and present it to the user as clean tables. Covers active markets to stake/trade, the order book (depth) for a symbol, the market price (best bid/ask/mid/spread + last trade), the user's open orders, and the user's order history. Load when the user asks to see markets, "where can I stake/bet", an order book, a price/quote for a symbol, "my open orders", or "my order history" on DEX.DO. Read-only — never places or cancels orders (that's the `dexdo-trading` skill).
---

# DEX.DO — Market Data (read / indexer API)

Read-only skill. It calls the public + signed **read** endpoints of the DEX.DO
REST API and renders the results as readable tables for the user. It never
mutates state — placing/cancelling orders and full-splits live in the
**`dexdo-trading`** skill.

What it answers:

1. **Active markets I can stake/trade** — `GET /api/v1/markets`
2. **Order book for a symbol** — `GET /api/v1/depth`
3. **Market price for a symbol** — best bid/ask/mid/spread + last trade (depth + trades)
4. **My open orders** — `GET /api/v1/orders?status=NEW,PARTIALLY_FILLED` (signed)
5. **My order history** — `GET /api/v1/orders` (signed, all statuses, paginated)

The canonical API contract is [`docs/api-spec.md`](../../../docs/api-spec.md). This
skill drives it through the shared client described below; you should not hand-roll
curl + HMAC.

## The shared client

Both DEX.DO API skills use one stdlib-only Python client:

```
.claude/skills/dodex-common/dodex_client.py
```

It handles base URL, HMAC-SHA256 request signing (the tricky part — the key is the
**hex-decoded** apiSecret, and symbols with spaces / `#` / em-dash are
percent-encoded and signed consistently), and prints the JSON response to stdout.
On an API error envelope (`{"code","msg"}`) it prints it and exits non-zero.

```sh
DODEX="python3 $PWD/.claude/skills/dodex-common/dodex_client.py"   # run from repo root
```

Default base URL is the dev endpoint `https://dodex-dev.ackinacki.org` (override
with `--base-url` or `$DODEX_BASE_URL`).

### Credentials (only for the two private/USER_DATA views)

The public views (markets, depth, price) need **no** credentials. "My open orders"
and "my order history" are signed and need the account's API credential — the
`<tt>.creds.json` file produced when the PrivateNote was registered
(`POST /api/v1/accounts`; see the deposit/onboarding flow). Point the client at it:

```sh
export DODEX_CREDS="$HOME/dexdo-workspace/notes/nackl.creds.json"   # {apiKey, apiSecret}
# or pass --creds <file> per call
```

If the user asks for "my orders" and no creds file is known, ask which account
(which `*.creds.json`, i.e. which registered note) they mean.

## Rendering — make it pretty

The client returns raw JSON. **You** turn it into a compact Markdown table in your
reply. Guidelines:

- Lead with a one-line summary (e.g. "7 active markets, showing TRADING + STAKING").
- Tables: short headers, right-align numbers, keep addresses truncated
  (`0:46fd…a3d9`) but show the full address in a final "addresses" line or on request.
- Convert unix-seconds timings to human time **in the user's view only**; keep the
  raw value if they ask.
- Money/prices are decimal **strings** in the API — print them verbatim, do not
  round or reformat (precision is significant).
- If a list is empty, say so plainly ("no open orders").

## 1. Active markets (where can I stake / trade?)

Staking is open in `STAKING`; order-book trading is open in `TRADING`. For "markets
I can act on now" fetch both:

```sh
$DODEX markets --status STAKING,TRADING --limit 50
```

Other useful filters: `--quote-asset NACKL`, `--oracle-name <name>`,
`--market-address 0:…` (one market), `--sort createdAt`, `--cursor <nextCursor>`
(next page when `hasMore`).

Render one row per market: `marketName`, `status`, `quoteAsset`, the outcome
symbols, and the key timings (`stakeEnd` for STAKING, `resultStart` for TRADING).
Put the event question (`event.eventName` / `event.description`) on a sub-line — it
is the human-facing title. Each outcome's `symbol`, `tickSize`, `stepSize`,
`minNotional` matter for trading, so surface them when the user is about to trade.

> "Where can I stake" specifically = `STAKING` markets. If there are none (common on
> dev), say so and offer the `TRADING` markets, where the user can take a position by
> buying outcome tokens on the book (the `dexdo-trading` skill).

## 2. Order book (depth) for a symbol

```sh
$DODEX depth --market-address "0:…" --symbol "<marketName>-<OUTCOME>" --limit 20
```

`symbol` is `<marketName>-<outcomeName>` exactly as it appears in the market's
`outcomes[].symbol` — copy it verbatim (it can contain spaces, `#`, an em-dash; the
client encodes it for you). Render two aligned columns (bids descending, asks
ascending) as `price | qty`, plus the spread between best bid and best ask.

## 3. Market price for a symbol

```sh
$DODEX price --market-address "0:…" --symbol "<symbol>"
```

Returns `bestBid`, `bestAsk`, `mid`, `spread`, and `lastTrade` (price/qty/time/
isBuyerMaker). Present it as a tiny quote block. Note for the user that a prediction
-market price ≈ the implied probability of that outcome (e.g. `0.53` ≈ 53%).

## 4. My open orders (signed)

```sh
$DODEX orders --open --creds "$DODEX_CREDS"
# or scope to one market symbol:
$DODEX orders --open --creds "$DODEX_CREDS" --market-address "0:…" --symbol "<symbol>"
```

`--open` is the shortcut for `--status NEW,PARTIALLY_FILLED`. Render: `symbol`,
`side`, `price`, `origQty`, `executedQty`, `status`, `time`. Empty ⇒ "no open orders".

> **Eventual consistency.** A freshly placed order can briefly not appear here — the
> public placement is indexed before the private owner-attribution this endpoint
> needs. On the post-restart dev deployment it surfaced within seconds, but it can
> lag further under indexer backlog. If the user "just placed" an order and it's not
> listed, that does **not** mean it failed: corroborate with `account` (collateral
> shows up as `locked`) and `depth` (the resting level appears publicly). Tell the
> user it's placed and will surface shortly; offer to re-check.

## 5. My order history (signed)

```sh
$DODEX orders --creds "$DODEX_CREDS" --limit 100
# filter by status / market as needed:
$DODEX orders --creds "$DODEX_CREDS" --status FILLED,CANCELED
$DODEX orders --creds "$DODEX_CREDS" --market-address "0:…" --symbol "<symbol>"
```

Results are newest-first. Page with `--cursor <nextCursor>` until `nextCursor` is
`null`. Render the same columns as open orders; group or sort by time if the user
wants. Collateral/outcome balances for context come from `account` and `balances`:

```sh
$DODEX account --creds "$DODEX_CREDS"                       # quote-asset free/locked
$DODEX balances --creds "$DODEX_CREDS" --market-address "0:…"   # per-outcome holdings
```

## Error handling

The client prints the API error envelope and exits non-zero. Map the common ones
for the user (full table in `docs/api-spec.md` §Error Response):

- `-1121` invalid market or symbol → re-check the `symbol` is copied verbatim from `markets`.
- `-1002` / `-1022` auth/signature → wrong or missing creds file for a signed view.
- `-2013` account not deployed → the note behind these creds isn't on-chain.
- `-1500` market data temporarily inconsistent → transient, retry.

## Scope / out of scope

- **In:** reading markets, depth, price, the caller's orders/balances; pretty output.
- **Out:** placing or cancelling orders, full-splits, staking transactions → use
  **`dexdo-trading`**. Registering a note / onboarding → the onboarding/deposit skills.
