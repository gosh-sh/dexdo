# AI Registry — inference market

The AI-inference registry under [`contracts/airegistry/`](../../contracts/airegistry/)
lets a model seller sell inference capacity tick-by-tick. It is integrated
with, but separate from, the DEX.DO prediction-market core under `contracts/dex/`.

## Contracts

| Contract | Role |
| --- | --- |
| `SuperRoot` | Per-network root factory. Registers a `RootModel` at a deterministic address derived from an owner pubkey. (The per-model `ManifestMetadata` contract and SuperRoot's `registerManifest` / `getManifestAddress` were removed upstream in v4.0.10.) |
| `RootModel` | Per-owner model registry. Derives + registers `TokenContract` children at deterministic `(sellerPubkey, nonce)` addresses. |
| `TokenContract` | Per-deal streaming escrow. Holds the buyer's SHELL deposit and settles ticks one at a time (probe-tick model, spec §3.1.2): `open → advance → stop / dispute / reclaim`. |
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
`TokenContract.*` handlers run on live traffic as of contracts 4.0.36, which put
the deal in the note's dApp; they stay replay-compatible for rows retained from
the former global capture. See
[docs/tech-specs/indexer.md](../tech-specs/indexer.md).

## Event ids

Defined in [`contracts/airegistry/modifiers/modifiers.sol`](../../contracts/airegistry/modifiers/modifiers.sol):
registry + streaming events occupy the 700s (e.g. `StreamFunded=720`,
`ProbeAccepted=728`); order-book events occupy `1000`–`1007`. The ABI event
names differ from the `*Emit` constant names, so the typed decoders in the
`*_events.rs` wrappers bind each id from the actual `emit … makeAddrExtern(<const>)`
site. These events decode into `raw_events`
(`event_type = "TokenContract.<Event>"`, `src_address` = the TokenContract
address) and project into the SETTLEMENT read-model: `inference_deals` (one row
per TokenContract / deal) and `inference_ticks` (one row per finalized tick of
the deal's current funding cycle). The indexer captures them live as of
contracts 4.0.36, which deploys the deal from the seller's `PrivateNote` and so
puts it in the DEX dApp; before that they arrived only as retained rows replayed
during a rebuild. A deal address serves more than one match now — see
[indexer.md § Deal-address reuse](../tech-specs/indexer.md#deal-address-reuse).
The deal's `orderbook_address`,
`seller_note`, and `buyer_note` are linked from `InferenceOrderBook.InferenceFilled`
(`sellerTC` + `buyerNote` + the SELL leg's note); per-tick rows and the
`finalized_ticks` aggregate comes from `TickFinalized` (per-tick `finalized_owed` is stored on each `inference_ticks` row — it is the contract's cumulative `_finalizedOwed`, not a per-tick delta);
`close_kind` + `clean_settlement` + `settled_at_chain` from the stream-close
events: `StreamStopped` sets `clean_settlement = true`; `DisputeResolved` and
`StreamReclaimed` set it to `false`; `ContractDestroyed` (`'DESTROYED'`) and
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
- **`TokenContract`** is deployed by the seller's `PrivateNote`
  (`deployDeal`) as of contracts 4.0.36, so it inherits the note's DEX dApp
  and is addressed with `dex_contract_params` like everything else. It used to
  be created by an **external** message and was therefore *self-rooted* (its
  `dapp_id` equalled its own account id, needing `self_rooted_contract_params`
  and a flag-16 ECC send from the giver to gas the fresh account). That is what
  changed, and it is why the indexer now sees `TokenContract.*` events on the
  DEX stream at all.
- **The deal carries its own gas reserve**, and nothing has to be sent ahead of
  it any more. `deployDeal` ships `gasReserve` ECC[2] with the deploy under
  flag 1, and the deal mints its own native floor in `ensureBalance` now that it
  shares the note's dApp. A plain run — deploy, offer, match, open, probe, T
  claims, close, withdraw — comes to about **0.300 + 0.015·T** SHELL; budget a
  second terminal charge for the paths wound down by a later one
  (`dispute → releaseDispute`, `sellerStop → close`). Older figures (0.240,
  0.215 + 0.013·T, 0.210 + 0.015·T) sized mechanisms that no longer exist.
  `PrivateNote.fundDeployShell(nonce, tcShell)` is the deal's only top-up and
  may be repeated: running the reserve down refuses a call, it does not strand
  the escrow.
- **`open()`** requires the seller mirror bond already funded, and the funding
  door is `TokenContract.fundDeal` — reached from the seller's note, which
  attaches the gas as ECC[2] and passes the bond as a figure. (`fundSellerBond`
  was the pre-4.0.36 name and no longer exists.) The bond is `2 * pricePerTick`,
  so a test must derive the amount from P rather than hardcoding it.

### Provisioning a root before any note is issued

`RootPN` bakes the codes it hands to notes, and two of them arrive through
their own setters rather than through the upgrade cell. `onCodeUpgrade` calls
`tvm.resetStorage()` and restores **six codes plus the owner pubkey** — a
seventh would push the upgrade cell past the shellnet BM gateway's JSON body
limit — so everything set outside it is wiped by every `updateCode`.

Run, in this order, on a fresh root and again after each upgrade:

1. `RootPN.updateCode(newcode, cell)` — the six bundled codes + owner pubkey.
2. `RootPN.setInferenceOrderBookCode(code)`
3. `RootPN.setTokenContractCode(code)`
4. `RootPN.setPrevPrivateNoteCode(hash, depth)` — the note generation this root
   still serves, so an upgrade does not strand balances on existing notes.

Skipping step 3 fails **late and quietly**. The note's
`_tokenContractCodeHash` / `_tokenContractCodeDepth` come from compiled-in
`RootPN` constants and are correct regardless, so every address the note derives
looks right; only the `_tokenContractCode` cell it would build the `StateInit`
from is empty, and the first `deployDeal` puts a codeless account at a
well-formed address. The e2e preflight catches this before a run:
`NOTE_CODE_CELL_FIELDS` in
[`sdk/tests/integration/common/preflight.rs`](../../sdk/tests/integration/common/preflight.rs)
hashes the cell itself and compares it with the manifest.

## End-to-end tests

Node-gated `#[ignore]` tests under `services/api/tests/`, driven directly
through `dodex_chain::Dex` (no DB, no HTTP — there are no inference handlers):

| Test | Covers |
| --- | --- |
| `e2e_inference` | Note deploys the book, places a resting BUY with SHELL escrow, cancels it. |
| `e2e_inference_match` | The note deploys its own deal (`deployDeal`) + a SELL offer crossed by a BUY ⇒ the match funds the deal (handover). |
| `e2e_inference_clob` | Two flows: a partial fill (2-tick offer crossed by a 4-tick limit buy, 2 ticks rest) + `getBestBidAsk`/`getWeeklyMedianPrice`, and a match's `Filled` event confirmed by its routing id. |
| `e2e_inference_orders` | The book as an order book, with no deal at all: two bids, a single `cancelInferenceOrder` by id that takes only its own, and a buy whose deadline has already passed — refused before `tvm.accept()`, so `nextOrderId` never moves. Fast (~35 s). |
| `e2e_inference_funding` | `deployDeal` + `fundDeployShell` with no giver in the run: the note deploys its own deal paying the gas reserve out of its own SHELL, then tops that reserve up and is checked to have moved exactly the figure it was asked for, in ECC[2] rather than native. Also pins `tcShell = 0` sending nothing at all. Fast (~2 min). |

The streaming-deal suites — `e2e_inference_stream`, `e2e_inference_settlement`,
`e2e_inference_twosided`, `e2e_inference_range`, `e2e_inference_subscription`,
`e2e_inference_recovery` and `e2e_inference_dispute` — were removed with the
contract calls they drove when v4.0.33 dropped those calls. What remains above
is the whole of the inference e2e coverage; the deal lifecycle past a match is
not exercised end to end.

They share the seed-note pool (`tests/fixtures/seed_notes.json` /
`E2E_SEED_NOTES`) like the other e2e tests. The note must additionally hold
SHELL — for escrow, and now for the gas reserve it sends with every deal it
deploys. No giver is used by any of them. Run:

```sh
cargo test -p dodex-api --test e2e_inference -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_match -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_clob -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_orders -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_funding -- --ignored --nocapture
```

All of them run in the e2e pipeline's `e2e_tests` step, which excludes nothing:
`--run-ignored only` is the whole selection. A `binary()` predicate that matches
no binary is an error in nextest rather than an empty exclusion, so a filter
naming a suite that has since been deleted fails the step before any test runs.

None of these needs a giver any more. Every deal is deployed the way a seller
in production deploys one — from the seller's own note, which pays the gas
reserve out of its own SHELL — so the route under test is the route that ships.
The external, giver-funded deploy the suites used before 4.0.36 is not merely
retired: the deal's constructor requires `msg.sender` to be the canonical note,
and an external message has none.

**A deal only publishes its price when it closes.** `_recordTrade` is reachable
only from `reportFinalized`, which the `TokenContract` calls from `_settleFees`
— on the close, never on a match or a claim. A match that is later
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

- **Registry registration** (`SuperRoot → RootModel`) — the external
  `SuperRoot` deploy needs the child code cells extracted into its constructor.
  The `TokenContract` leg is no longer part of it: since 4.0.36 the deal is
  deployed by the seller's note and only announces itself back to the
  `RootModel`, which is a callback nothing external can drive.
- **Continuation queue** (`processHead`) — needs `> MAX_MATCHES_PER_CALL`
  matches in one buy; depends on the deployed contract's constant.
- **Subscription roll** (`settleWeek`) — needs a weekly cycle to roll over;
  each boundary credits the whole weekly quota take-or-pay, consumed or not.
- **Longer probe variants** — probe burn, seller no-show reclaim, dispute
  timeout, each waiting a 600s on-chain window.
- **Typed ext-out event payload decode** — blocked on the deployment skew noted
  above (shellnet book `code_hash` ≠ this repo's), not a wrapper issue; events
  are asserted by routing id meanwhile.
