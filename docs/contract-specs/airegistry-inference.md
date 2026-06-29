# AI Registry — inference market

The AI-inference registry under [`contracts/airegistry/`](../../contracts/airegistry/)
lets a model seller sell inference capacity tick-by-tick. It is integrated
with, but separate from, the DEX.DO prediction-market core under `contracts/dex/`.

## Contracts

| Contract | Role |
| --- | --- |
| `SuperRoot` | Per-network root factory. Registers a `RootModel` and a `ManifestMetadata` at deterministic addresses derived from an owner pubkey. |
| `RootModel` | Per-owner model registry. Derives + registers `TokenContract` children at deterministic `(sellerPubkey, nonce)` addresses. |
| `ManifestMetadata` | Per-model manifest. |
| `TokenContract` | Per-deal streaming escrow. Holds the buyer's SHELL deposit and settles ticks one at a time (probe-tick model, spec §3.1.2): `open → advance → stop / dispute / reclaim`. |
| `InferenceOrderBook` | Per-model CLOB. Matches SELL offers (each backed by a `TokenContract`) against BUY orders and subscriptions paid in SHELL escrow. Deployed per `(model)` from a `PrivateNote`. |

Inference settles in **SHELL held physically by the note**, so the
`PrivateNote` itself is the on-chain market participant — the inference
order/stream methods live on the note (`deployInferenceOrderBook`,
`postSellOffer`, `placeInferenceBuy`, `streamStop`, …), keyed by `modelHash`
(the book code is baked into the note at deploy).

## Rust support

Typed wrappers (message encoders/decoders, getters, event decoders) live in
[`crates/contracts/src/airegistry/`](../../crates/contracts/src/airegistry/),
following the same style as the `dex` wrappers. The note-side inference methods
are on [`dex::private_note`](../../crates/contracts/src/dex/private_note.rs).

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
`seller_note`, and `buyer_note` are linked from `InferenceOrderBook.Filled`
(`sellerTC` + `buyerNote` + the SELL leg's note); per-tick rows and the
`finalized_ticks` / `finalized_owed_total` aggregates come from `TickFinalized`;
`close_kind` + `clean_settlement` + `settled_at_chain` from the stream-close
events (`StreamStopped` = clean; `DisputeResolved` / `StreamReclaimed` /
`ContractDestroyed` = not clean).

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
- **`open()`** requires the seller probe commission already funded, and
  `fundProbeCommission` only accepts an internal SHELL-bearing message (an
  external signed call cannot carry currency). The e2e harness delivers it as a
  call body from the giver (`sendCurrencyWithBody`), so no separate wallet is
  needed.

## End-to-end tests

Node-gated `#[ignore]` tests under `services/api/tests/`, driven directly
through `dodex_chain::Dex` (no DB, no HTTP — there are no inference handlers):

| Test | Covers |
| --- | --- |
| `e2e_inference` | Note deploys the book, places a resting BUY with SHELL escrow, cancels it. |
| `e2e_inference_match` | External `TokenContract` deploy + a SELL offer crossed by a BUY ⇒ the match funds the `TokenContract` (handover). |
| `e2e_inference_stream` | Full deal lifecycle: match → probe commission → `open` → wait the 180s settle window → `advance` (probe accepted) → `streamStop`. Slow (~4 min). |

They share the seed-note pool (`tests/fixtures/seed_notes.json` /
`E2E_SEED_NOTES`) like the other e2e tests; the note must additionally hold
SHELL for escrow, and the giver must be reachable (shellnet only). Run:

```sh
cargo test -p dodex-api --test e2e_inference -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_match -- --ignored --nocapture
cargo test -p dodex-api --test e2e_inference_stream -- --ignored --nocapture
```

ABI-grounded offline checks (no node) live in `crates/contracts`
(`airegistry::tests`): every `Params`/`Result` struct is verified against the
bundled ABI, and every event id against `modifiers.sol`.

## Not yet covered

Registry registration (`SuperRoot → RootModel → TokenContract`), order-book
subscriptions / forfeit (spec §8), the continuation queue (`processHead`),
event-payload assertions, and the longer probe variants (probe burn, seller
no-show reclaim, dispute timeout — each waits a 600s on-chain window).
