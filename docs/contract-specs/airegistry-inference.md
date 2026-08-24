# AI Registry — inference market

The AI-inference registry under [`contracts/airegistry/`](../../contracts/airegistry/)
lets a model seller sell inference capacity tick-by-tick. It is integrated
with, but separate from, the DEX.DO prediction-market core under `contracts/dex/`.

## Contracts

| Contract | Role |
| --- | --- |
| `SuperRoot` | Per-network root factory. Registers a `RootModel` at a deterministic address derived from an owner pubkey. (The per-model `ManifestMetadata` contract and SuperRoot's `registerManifest` / `getManifestAddress` were removed upstream in v4.0.10.) |
| `RootModel` | Per-owner model registry. Derives + registers `TokenContract` children at deterministic `(sellerPubkey, nonce)` addresses. |
| `TokenContract` | Per-deal streaming escrow. Holds the buyer's SHELL deposit and settles on the probe-tick model (spec §3.1.2): `fundDeal → open → acceptProbe → stop / dispute / reclaim`. (`advance`, the old tick-by-tick driver, was dropped in v4.0.33 along with `PrivateNote.postSellerBond`; `acceptProbe` and `fundDeal` are what replaced them.) |
| `InferenceOrderBook` | Per-model CLOB. Matches SELL offers (each backed by a `TokenContract`) against BUY orders and subscriptions paid in SHELL escrow. Deployed per `(model)` from a `PrivateNote`. |

Inference settles in **SHELL held physically by the note**, so the
`PrivateNote` itself is the on-chain market participant — the inference
order/stream methods live on the note (`deployInferenceOrderBook`,
`postSellOffer`, `placeInferenceBuy`, `streamStop`, …). Most are keyed by
`modelHash` — the book code is baked into the note at deploy, so the note
derives the book address itself. The seller side is keyed by the deal's
`nonce` instead: `postSellOffer` addresses the note's canonical
`TokenContract`, and that TC carries the model and the offer terms.

## Rust support

Typed wrappers (message encoders/decoders, getters, event decoders) live in
[`crates/contracts/src/airegistry/`](../../crates/contracts/src/airegistry/),
following the same style as the `dex` wrappers. The note-side inference methods
are on [`dex::private_note`](../../crates/contracts/src/dex/private_note.rs). A
method-by-method reference across all three layers (per-contract wrappers,
note-side inference, and the `Dex` facade) is in
[`airegistry-sdk.md`](airegistry-sdk.md).

The `dodex-chain` facade exposes the inference flow behind the `test-helpers`
feature (`deploy_inference_order_book`, `post_sell_offer`, `place_inference_buy`,
`token_contract_*`, …). There are **no inference REST endpoints** — the
write-side SDK support stays behind the `test-helpers` feature flag and out of
the production binary. The indexer captures `InferenceOrderBook.*` through the
DEX dApp stream and projects the public inference order/trade read-model.
`TokenContract.*` handlers remain replay-compatible for rows retained from the
former global capture, but current live capture excludes every TokenContract
event route before decode. See
[docs/tech-specs/indexer.md](../tech-specs/indexer.md).

## Event ids

Defined in [`contracts/airegistry/modifiers/modifiers.sol`](../../contracts/airegistry/modifiers/modifiers.sol):
registry + streaming events occupy the 700s (e.g. `StreamFunded=720`,
`ProbeAccepted=728`); order-book events occupy `1000`–`1007`. The ABI event
names differ from the `*Emit` constant names, so the typed decoders in the
`*_events.rs` wrappers bind each id from the actual `emit … makeAddrExtern(<const>)`
site. When retained rows are replayed, these events decode into `raw_events`
(`event_type = "TokenContract.<Event>"`, `src_address` = the TokenContract
address) and project into the SETTLEMENT read-model: `inference_deals` (one row
per TokenContract / deal) and `inference_ticks` (one row per `TickFinalized`
**event** — the emit follows the week loop in `_chargeWeeksThrough`, so one row
can stand for a batch of closed boundaries, not one week).
The current live indexer does not capture new TokenContract event rows. The
deal's `orderbook_address`,
`seller_note`, and `buyer_note` are linked from `InferenceOrderBook.InferenceFilled`
(`sellerTC` + `buyerNote` + `sellerNote`). The SELL leg's note in `inference_orders`
is a fallback for the one case the event cannot cover: a payload with no `sellerNote`
(or a zero one) — an event captured before the field existed, or an ABI drift the
projector deliberately survives rather than fails. It is not a fallback for orphan
repair: there the SELL leg is precisely what was never projected, so the lookup finds
nothing and the deal keeps a NULL `seller_note` unless the fill's own field supplies it; per-tick rows and the
`finalized_ticks` aggregate comes from `TickFinalized` (per-tick `finalized_owed` is stored on each `inference_ticks` row — it is the contract's cumulative `_finalizedOwed`, not a per-tick delta);
`close_kind` + `clean_settlement` + `settled_at_chain` from the stream-close
events: `StreamStopped` sets `clean_settlement = true`; `DisputeResolved` sets it to
`false`; `ContractDestroyed` (`'DESTROYED'`) and
`ProbeBurned` (`'PROBE_BURNED'` — a buyer stop before probe-accept, or the
dispute-burn path; both terminal) set only `close_kind` and `settled_at_chain`,
leaving `clean_settlement` unchanged (remains `NULL` if no prior close event set
it). Consumers should
treat `clean_settlement IS NOT TRUE` as "not a clean settlement" to cover both
the `false` and `NULL` cases.

## On-chain deploy specifics (Acki Nacki)

Acki Nacki is dApp-sharded, which shapes how these contracts are reached:

- **`InferenceOrderBook`** is deployed by the note via an internal message
  (`deployInferenceOrderBook`), so it inherits the System dApp — addressed the
  same way as the other DEX contracts.
- **`TokenContract`** is deployed by an **external** message, so it is
  *self-rooted*: its `dapp_id` equals its own account id. It must be addressed
  with `self_rooted_contract_params`, not the System dApp. Native value does not
  cross a dApp boundary, so the fresh account is created + gassed by sending
  **ECC SHELL with flag 16** from the giver (flag 16 lands the ECC as the new
  account's native balance).
- **`open()`** requires the seller mirror bond already funded. Since v4.0.33
  that funding is `PrivateNote.fundDeal` — the note pays its own deal out of
  `_balance[CURRENCIES_ID_SHELL]`, deriving the deal address from the `nonce`
  rather than being told it, so a wrong nonce bounces instead of paying a
  stranger. No giver and no separate wallet are involved on the seller side.
  The buyer's half is never called for at all: the fill path inside the buyer's
  note sends `fundBuyerBond` inline (`PrivateNote.sol:752`).

  **The bond is `2 * pricePerTick`, so derive the amount from P rather than
  hardcoding it.** `TokenContract._bondAmount()` (`:554-556`) is the definition
  and `fundDeal` enforces it (`:906`, `ERR_INSUFFICIENT_DEPOSIT`). An undersized
  bond is not loud: the note sends with `bounce:true`, so the SHELL simply comes
  back and the deal reports `bond_funded: false` — which surfaces later as a
  reverting `open()` rather than as a funding error. Excess is refunded
  (`:914`), so erring high is safe and erring low is not.

## End-to-end tests

Node-gated `#[ignore]` tests under `services/api/tests/`, driven through
`dodex_chain::Dex`. The chain half needs only a reachable shellnet endpoint and
a seed-note pool. Six of them — `e2e_inference`, `e2e_inference_clob`,
`e2e_inference_orders`, `e2e_inference_stream`, `e2e_inference_expiry_sweep`
and `e2e_inference_range_link` — additionally carry **read-model phases** that
assert
the chain action reached the public API, polling the production router raised
in-process (`common::setup()`) against the indexer database. Those phases need
**two** things: `TEST_DATABASE_URL`, and `E2E_READ_MODEL=1` to say an indexer is
filling that database. Both are set only on the woodpecker stand; the shellnet
lane sets the first alone, and the phases stay off there because nothing writes
the facts they poll for.

The two are NOT symmetric, and the asymmetry is deliberate. With
`E2E_READ_MODEL` unset the binary prints a skip notice and runs its chain half
exactly as before — the ordinary case on every lane but one, and never red. With
`E2E_READ_MODEL` set and no reachable database the binary pushes a FAILURE: that
combination is an instruction the only lane able to carry it out could not carry
out, and printing a notice onto a green run is precisely how such a run goes
unnoticed.
The polling rules (poll for presence, assert content; never panic mid-scenario)
and the shared wait budget live in
[`docs/tech-specs/indexer.md`](../tech-specs/indexer.md#in-scene-end-to-end-assertions).

| Test | Covers |
| --- | --- |
| `e2e_inference` | Note deploys the book, places a resting BUY with SHELL escrow, cancels it. Read phases (stand only): the placed order in `/orders` and its level in `/depth` (IX-SEQ-02), the deployed book in `/markets` (IX-SEQ-01), and that the producer filter serves only the scene's own market (IX-GATE-17). |
| `e2e_inference_match` | External `TokenContract` deploy + a SELL offer crossed by a BUY ⇒ the match funds the `TokenContract` (handover). |
| `e2e_inference_clob` | Two flows: a partial fill (2-tick offer crossed by a 4-tick limit buy, 2 ticks rest) + `getBestBidAsk`/`getWeeklyMedianPrice`, and a match's `Filled` event confirmed by its routing id. Read phases (stand only): the match on the public tape in `/trades` and the taker leg in `/orders` (IX-SEQ-03). |
| `e2e_inference_orders` | The book as an order book, with no deal at all: two bids, a single `cancelInferenceOrder` by id that takes only its own, and a buy whose deadline has already passed — refused before `tvm.accept()`, so `nextOrderId` never moves. Fast (~35 s). Read phase (stand only): the precise cancel in `/orders` — the cancelled order reads `CANCELLED` with its size preserved (a cancel is not a fill), while its neighbour stays live (IX-SEQ-08). |
| `e2e_inference_funding` | `fundDeployShell`: a note pays its own canonical `TokenContract` address, and the deal contract then deploys onto it with no giver in the run. Also pins the two things the call must not do — reach the RootModel, which it no longer has a leg for, and send anything at all when asked for `0`. Fast (~2 min). |
| `e2e_inference_stream` | The deal lifecycle past the match, between **two** notes: offer → crossing IOC buy → `fundDeal` → `open` → `PROBE_WINDOW` → `acceptProbe` → buyer `stop`. Read phases (stand only): `inference_deals` names a seller and a buyer that DIFFER (IX-SEQ-04), and the deal closes `STOPPED` with `clean_settlement` (IX-SEQ-06). Both are SQL, not HTTP — `inference_deals` has no public surface. Slow (~9 min): `PROBE_WINDOW` alone is 180 s. |
| `e2e_inference_range_link` | The one scene spanning both halves of the product: an `InferenceOrderBook` is deployed first, then a prediction market whose event is a RANGE event carrying that book's address. Read phase (stand only): `/api/v1/prediction/markets?resolvesFrom=<book>` returns exactly that market, naming the book and `WEEKLY_MEDIAN_PRICE` (IX-SEQ-09). Two notes of different currencies — `PN-INF` for the book, `PN-API` for the market. |
| `e2e_inference_recovery` | The exit from a funded deal the seller never opened. Two deals side by side: one is left unopened and swept by the permissionless `cleanupUnopened` after `MATCH_OPEN_TIMEOUT`, the other is opened and must survive the identical call. The buyer is repaid deposit **plus** the bond `_releaseBuyerBond` folds into it, the seller's note gets its bond back unslashed, and the deal self-destructs. Two `PN-INF` notes — the subject is money leaving one party's deal and arriving at the other's note. No read phase. Slow (~15 min). |
| `e2e_inference_range` | A price becoming an outcome, across four contracts with nobody voting: a deal is run to a CLOSE so the book has a reference price at all, an oracle publishes a RANGE event binding bounds `[1, 3, 5]` SHELL to that book, a market is deployed on it, and after the deadline `resolveRange` walks `requestWeeklyMedian` → `onWeeklyMedian` and settles into the bucket the median falls in. Also pins that the LIST set the market's clock: `resultStart` equals the event deadline, which no other scene here gets without a separate `submitSetTimings`. Two notes — `PN-INF` for the deal, `PN-API` for the market. No read phase: the whole reading is on chain. Slow (~11 min): a 190 s probe window and a 240 s range window, in sequence. |
| `e2e_inference_expiry_sweep` | Two endings, two tests. A bid whose deadline passes is expired by the permissionless `expireOrder` and reads `EXPIRED` in `/orders` — its own terminal status — while a neighbour without a deadline stays `LIVE` (IX-SEQ-12). And a taker remainder the chain refunds with no closing event is ended by the reconciler's sweep: `CANCELLED` **with `swept_at` set**, which is SQL because the DTO carries no such field and a provisional cancel is otherwise identical to a real one over HTTP (IX-SEQ-07). |

The streaming-deal suites — `e2e_inference_settlement`, `e2e_inference_twosided`,
`e2e_inference_subscription` and `e2e_inference_dispute` — were removed with the
contract calls they drove when v4.0.33 dropped those calls.

**`e2e_inference_range` was removed with them and should not have been.** Every
call its subject rests on survived that sync untouched — `addRangeEvent`,
`resolveRange` and `onWeeklyMedian` in `OracleEventList`, `requestWeeklyMedian`
and `reportFinalized` in `InferenceOrderBook`. What broke was its SETUP: to have
a reference price at all it must run a deal to a close, and closing went through
`advance`. It is back, with that one section substituted (`postSellerBond` →
`fundDeal`, `advance` → `acceptProbe`) and the rest as it was. The substitution
is sound because `acceptProbe` sets `_ticksFinalized = 1` exactly as the single
`advance` used to, `_settleFees` publishes it on the close, and `MIN_LIQUIDITY`
is 1 — so one probe tick is still the whole liquidity the median needs. The
restored version also waits for BOTH bonds before `open`, which the deleted one
had no reason to.

**`e2e_inference_recovery` came back for half its subject, and only half.** It
covered two exits from a deal the seller walked away from. `reclaimOnTimeout` —
the buyer's way out of a deal that WAS opened and then abandoned — left the ABI
with that sync and has no replacement, so that case is uncovered and named as
such. `cleanupUnopened` survived untouched, `PrivateNote.streamCleanup` still
forwards to it, and the never-opened exit is tested again.

One claim did NOT survive the re-reading, and the restored file says so rather
than repeating it. The deleted version described its control as exercising the
permanent `_everOpened` latch. It does not: `cleanupUnopened` checks `!_opened`
first, and an opened-and-abandoned deal trips that. The latch would need
`_opened == false` with `_everOpened == true`, and no such deal can exist today
— every way out of an open stream destroys the contract, and the one path that
used to leave a released deal standing was `reclaimOnTimeout` itself. The latch
is defensive code with no reachable state behind it.

**That removal was about a changed model, not a disappeared one, and two
contract versions have passed since.** What v4.0.33 dropped were
`TokenContract.advance` and `PrivateNote.postSellerBond`, the optimistic
tick-by-tick driver; what replaced them are `fundDeal` (the seller's half of the
bond, the buyer's being funded inline on the fill) and `acceptProbe` (the seller
claiming the trial tick once `PROBE_WINDOW` has passed). Both are present at
v4.0.35, which is what `e2e_inference_stream` above drives — it is a new binary
of the same name as a deleted one, built on the replacement calls rather than
restored from git. The suites still listed as removed are the ones for which no
such replacement has been written; the deal lifecycle past a match is no longer
entirely unexercised.

They share the seed-note pool (`tests/fixtures/seed_notes.json` /
`E2E_SEED_NOTES`) like the other e2e tests; the note must additionally hold
SHELL for escrow, and the giver must be reachable (shellnet only). Run:

```sh
cargo test -p dodex-api --test e2e_inference -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_match -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_clob -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_orders -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_funding -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_stream -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_expiry_sweep -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_range_link -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_range -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_recovery -- --ignored --nocapture
```

All of them run in the e2e pipeline's `e2e_tests` step, which excludes nothing:
`--run-ignored only` is the whole selection. A `binary()` predicate that matches
no binary is an error in nextest rather than an empty exclusion, so a filter
naming a suite that has since been deleted fails the step before any test runs.

Most of these deploy their `TokenContract` off the shared shellnet giver
because it is the cheap route. It is not the route the contracts are designed
around — a seller in production has only a note — so the note-funded path is
covered on its own by `e2e_inference_funding`.

**A deal only publishes its price when it closes.** `_recordTrade` is reachable
only from `reportFinalized`, which the `TokenContract` calls from `_settleFees`
— on the close, never on a match or an `advance`. A match that is later
refunded served nothing, so counting it would let anyone move the reference
price with orders they never honour. Anything that needs
`getWeeklyMedianPrice` (the range cycle, above) therefore has to run a deal to
a genuine close first.

The `e2e_inference_clob` Filled check asserts the event by its routing id, not
its decoded payload. Typed body decode of these ext-out events returns tvm
error 304 against the current shellnet because of a **code version skew**: the
`InferenceOrderBook` code baked into the shellnet-deployed `PrivateNote` (the
one the seed notes derive their book from) has a different `code_hash` than this
repo's `InferenceOrderBook.tvc`, so the deployed contract emits event bodies
shaped by its own event signatures, not the bundled ABI. Function signatures
(getters) still match, which is why reads work and only event-body decode
fails — it is not the multi-cell off-by-32 case. This is a deployment skew, not
a wrapper bug; typed payload decode will work once the shellnet book matches the
bundled ABI (redeploy of the on-chain registry/RootPN). The routing id is
version-independent, and the payload struct field names are verified offline.

ABI-grounded offline checks (no node) live in `crates/contracts`
(`airegistry::tests`): every `Params`/`Result` struct is verified against the
bundled ABI, every event id against `modifiers.sol`, and every event payload
struct's field names against the ABI event inputs.

## Not yet covered

- **Registry registration** (`SuperRoot → RootModel → TokenContract`) — the
  external `SuperRoot` deploy needs the child code cells extracted into its
  constructor.
- **Continuation queue** (`processHead`) — needs `> MAX_MATCHES_PER_CALL`
  matches in one buy; depends on the deployed contract's constant.
- **Subscription roll** (`pokeSubscription`) — needs a weekly cycle to roll
  over; the closing cycle's unspent budget refunds to the buyer.
- **Longer probe variants** — probe burn, seller no-show reclaim, dispute
  timeout, each waiting a 600s on-chain window.
- **Typed ext-out event payload decode** — blocked on the deployment skew noted
  above (shellnet book `code_hash` ≠ this repo's), not a wrapper issue; events
  are asserted by routing id meanwhile.
