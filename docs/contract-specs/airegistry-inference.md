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
the production binary. Indexer projectors for `InferenceOrderBook.*` and
`TokenContract.*` events are active; see
[docs/tech-specs/indexer.md](../tech-specs/indexer.md) for the
read-model they build (`inference_orders`, `inference_deals`, `inference_ticks`).

## Event ids

Defined in [`contracts/airegistry/modifiers/modifiers.sol`](../../contracts/airegistry/modifiers/modifiers.sol):
registry + streaming events occupy the 700s (e.g. `StreamFunded=720`,
`ProbeAccepted=728`); order-book events occupy `1000`–`1007`. The ABI event
names differ from the `*Emit` constant names, so the typed decoders in the
`*_events.rs` wrappers bind each id from the actual `emit … makeAddrExtern(<const>)`
site. These events are decoded into `raw_events` (`event_type = "TokenContract.<Event>"`,
`src_address` = the TokenContract address) and projected into the SETTLEMENT
read-model: `inference_deals` (one row per TokenContract / deal) and
`inference_ticks` (one row per finalized tick). The deal's `orderbook_address`,
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
- **`TokenContract`** is deployed by an **external** message, so it is
  *self-rooted*: its `dapp_id` equals its own account id. It must be addressed
  with `self_rooted_contract_params`, not the System dApp. Native value does not
  cross a dApp boundary, so the fresh account is created + gassed by sending
  **ECC SHELL with flag 16** from the giver (flag 16 lands the ECC as the new
  account's native balance).
- **`open()`** requires the seller mirror bond already funded, and
  `fundSellerBond` only accepts an internal SHELL-bearing message (an external
  signed call cannot carry currency). The e2e harness delivers it as a call body
  from the giver (`sendCurrencyWithBody`), so no separate wallet is needed. The
  bond is `2 * pricePerTick`, so a test must derive the amount from P rather
  than hardcoding it.

## End-to-end tests

Node-gated `#[ignore]` tests under `services/api/tests/`, driven directly
through `dodex_chain::Dex` (no DB, no HTTP — there are no inference handlers):

| Test | Covers |
| --- | --- |
| `e2e_inference` | Note deploys the book, places a resting BUY with SHELL escrow, cancels it. |
| `e2e_inference_match` | External `TokenContract` deploy + a SELL offer crossed by a BUY ⇒ the match funds the `TokenContract` (handover). |
| `e2e_inference_clob` | Three flows: a partial fill (2-tick offer crossed by a 4-tick limit buy, 2 ticks rest) + `getBestBidAsk`/`getWeeklyMedianPrice`; a subscription (`placeInferenceSubscription` + `getSubscription`); and a match's `Filled` event confirmed by its routing id. |
| `e2e_inference_stream` | Full deal lifecycle: match → seller bond → `open` → wait the 180s settle window → `advance` (probe accepted) → `streamStop`. Slow (~4 min). |

They share the seed-note pool (`tests/fixtures/seed_notes.json` /
`E2E_SEED_NOTES`) like the other e2e tests; the note must additionally hold
SHELL for escrow, and the giver must be reachable (shellnet only). Run:

```sh
cargo test -p dodex-api --test e2e_inference -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_match -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_clob -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_stream -- --ignored --nocapture
```

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
