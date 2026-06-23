---
name: dexdo-market-data
description: Read DEX.DO market data and a trader's own orders/stakes, and present it to the user as clean tables. Covers active markets to stake/trade, the order book (depth) for a symbol, the market price (best bid/ask/mid/spread + last trade), the user's open orders, the user's order history (all via the indexer/read REST API), and the user's on-chain stakes per market (via the dexdo SDK CLI). Load when the user asks to see markets, "where can I stake/bet", an order book, a price/quote for a symbol, "my open orders", "my order history", or "my stakes / my bets" on DEX.DO. Read-only — never places or cancels orders or stakes (that's the `dexdo-trading` skill).
---

# DEX.DO — Market Data (read)

Read-only skill. It renders DEX.DO data as readable tables for the user. Most views
use the public + signed **read** endpoints of the REST API; "my stakes" is an
on-chain read via the `dexdo` SDK CLI (the REST API has no stakes view). It never
mutates state — placing/cancelling orders, full-splits, and staking live in the
**`dexdo-trading`** skill.

What it answers:

1. **Active markets I can stake/trade** — `GET /api/v1/markets`
2. **Order book for a symbol** — `GET /api/v1/depth`
3. **Market price for a symbol** — best bid/ask/mid/spread + last trade (depth + trades)
4. **My open orders** — `GET /api/v1/orders?status=NEW,PARTIALLY_FILLED` (signed)
5. **My order history** — `GET /api/v1/orders` (signed, all statuses, paginated)
6. **My stakes / bets per market** — `dexdo stakes` (on-chain, via the SDK CLI)

The canonical REST contract is [`docs/api-spec.md`](../../../docs/api-spec.md). The
REST views go through the shared client below; "my stakes" goes through the `dexdo`
SDK CLI (§6). Don't hand-roll curl + HMAC.

## The shared client

Both DEX.DO API skills use one stdlib-only Python client:

```
.claude/skills/dexdo-common/dexdo_client.py
```

It handles base URL, HMAC-SHA256 request signing (the tricky part — the key is the
**hex-decoded** apiSecret, and symbols with spaces / `#` / em-dash are
percent-encoded and signed consistently), and prints the JSON response to stdout.
On an API error envelope (`{"code","msg"}`) it prints it and exits non-zero.

```sh
DEXDO="python3 $PWD/.claude/skills/dexdo-common/dexdo_client.py"   # run from repo root
```

Default base URL is the dev endpoint `https://dodex-dev.ackinacki.org` (override
with `--base-url` or `$DEXDO_BASE_URL`).

### Credentials (only for the two private/USER_DATA views)

The public views (markets, depth, price) need **no** credentials. "My open orders"
and "my order history" are signed and need the account's API credential — the
`<tt>.creds.json` file produced when the PrivateNote was registered
(`POST /api/v1/accounts` — produce it with the **`dexdo-register-account`** skill).
Point the client at it:

```sh
export DEXDO_CREDS="$HOME/dexdo-workspace/notes/nackl.creds.json"   # {apiKey, apiSecret}
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
$DEXDO markets --status STAKING,TRADING --limit 50
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
$DEXDO depth --market-address "0:…" --symbol "<marketName>-<OUTCOME>" --limit 20
```

`symbol` is `<marketName>-<outcomeName>` exactly as it appears in the market's
`outcomes[].symbol` — copy it verbatim (it can contain spaces, `#`, an em-dash; the
client encodes it for you). Render two aligned columns (bids descending, asks
ascending) as `price | qty`, plus the spread between best bid and best ask.

## 3. Market price for a symbol

```sh
$DEXDO price --market-address "0:…" --symbol "<symbol>"
```

Returns `bestBid`, `bestAsk`, `mid`, `spread`, and `lastTrade` (price/qty/time/
isBuyerMaker). Present it as a tiny quote block. Note for the user that a prediction
-market price ≈ the implied probability of that outcome (e.g. `0.53` ≈ 53%).

## 4. My open orders (signed)

```sh
$DEXDO orders --open --creds "$DEXDO_CREDS"
# or scope to one market symbol:
$DEXDO orders --open --creds "$DEXDO_CREDS" --market-address "0:…" --symbol "<symbol>"
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
$DEXDO orders --creds "$DEXDO_CREDS" --limit 100
# filter by status / market as needed:
$DEXDO orders --creds "$DEXDO_CREDS" --status FILLED,CANCELED
$DEXDO orders --creds "$DEXDO_CREDS" --market-address "0:…" --symbol "<symbol>"
```

Results are newest-first. Page with `--cursor <nextCursor>` until `nextCursor` is
`null`. Render the same columns as open orders; group or sort by time if the user
wants. Collateral/outcome balances for context come from `account` and `balances`:

```sh
$DEXDO account --creds "$DEXDO_CREDS"                       # quote-asset free/locked
$DEXDO balances --creds "$DEXDO_CREDS" --market-address "0:…"   # per-outcome holdings
```

## 6. My stakes (on-chain — via the `dexdo` SDK CLI, not the REST API)

Stakes made during a market's **STAKING** phase live on-chain in the user's
PrivateNote, not in the indexer/REST API — so this read goes through the **`dexdo`
SDK CLI** (the same binary the `dexdo-trading` skill builds for `dexdo stake`; build
it once per `dexdo-trading` §0). It needs only the note address, so a read-only
`pn_state.<tt>.json` (or `--pn-address`) is enough — no API creds:

```sh
DEXDO_CLI="$WORKSPACE/dexdo/sdk/target/release/dexdo"
"$DEXDO_CLI" stakes --pn-state-file "$WORKSPACE/notes/pn_state.<tt>.json"
# or: "$DEXDO_CLI" stakes --pn-address "0:<pn>"
```

It prints a JSON `stakes` map keyed by the market's `eventId`. Each entry has
`amount` (a **per-outcome** array, raw token units — scale by the quote asset's
decimals: NACKL/SHELL = 9, USDC = 6), plus `debtAmount` / `couponsAmount`,
`tokenType`, and `oracleListHash`. A position from a full split shows non-zero
amounts on **all** outcomes; a single-side stake shows the amount on one outcome.

Render it for the user by **joining `eventId` → market** via `markets`
(`event.eventId` matches the stakes key) so each row reads as a market name +
phase + the staked amount per outcome. Example shape:

| Market | Phase | Staked (NACKL) |
|---|---|---|
| Acki Nacki: will NACKL be listed…  | STAKING | No = 40.0 |
| Sport: will the home side win…     | STAKING | No = 10.0 |
| Group Stage — USA…                  | TRADING | Yes = 8.78, No = 11.22 (full set) |

(`amount[i]` corresponds to `outcomes[i].outcomeId` from that market; convert raw →
human by the token decimals, and label outcomes with `outcomeNames`.) An empty map
means the note has no stakes yet.

## Error handling

The client prints the API error envelope and exits non-zero. Map the common ones
for the user (full table in `docs/api-spec.md` §Error Response):

- `-1121` invalid market or symbol → re-check the `symbol` is copied verbatim from `markets`.
- `-1002` / `-1022` auth/signature → wrong or missing creds file for a signed view.
- `-2013` account not deployed → the note behind these creds isn't on-chain.
- `-1500` market data temporarily inconsistent → transient, retry.

## Scope / out of scope

- **In:** reading markets, depth, price, the caller's orders/balances, and the
  caller's on-chain stakes (`dexdo stakes`); pretty output.
- **Out:** placing or cancelling orders, full-splits, staking transactions → use
  **`dexdo-trading`**. Registering a note / onboarding → the onboarding/deposit skills.
