# AI Registry — SDK & wrapper method reference

Method-by-method reference for the AI-inference surface. Pairs with
[`airegistry-inference.md`](airegistry-inference.md) (the protocol spec) and the
contract sources under [`contracts/airegistry/`](../../contracts/airegistry/).

The Rust support comes in three layers. Pick by how close to the raw contract
you need to be:

| Layer | Where | Use it for |
| --- | --- | --- |
| **Per-contract wrappers** | `crates/contracts/src/airegistry/` | Talking to a `SuperRoot` / `RootModel` / `TokenContract` / `InferenceOrderBook` account directly. One wrapper method per contract method; each carries a typed `ParamsOf…` / `ResultOf…`. |
| **Note-side inference** | `crates/contracts/src/dex/private_note.rs` | The buyer/seller actions. Inference settles in SHELL held physically by the `PrivateNote`, so the note is the on-chain market participant — order/stream calls are sent *from the note*, owner-signed. |
| **`Dex` facade** | `crates/chain/src/test_helpers.rs` (behind the `test-helpers` feature) | End-to-end flows. Higher-level helpers that resolve addresses, send from the right note, and decode getters into plain values. Not compiled into the production binary; there are no inference REST endpoints. |

Mutating methods take a `Signer` and return `ResultOfSendMessage`; getters return
their typed `ResultOf…`. Pubkey fields are `uint256` accepted as a decimal or
`0x` hex string.

Each section lists what the SDK **wraps**. Contract methods with no wrapper are
called out under the tables — the contracts carry more surface than the SDK
does, and the gap is where the next e2e test stalls.

---

## 1. Per-contract wrappers

### SuperRoot — `airegistry/super_root.rs`

Per-network root factory. Registers a `RootModel` at a deterministic address
derived from an owner pubkey.

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `set_pubkey` | `setPubkey` | `ParamsOfSetPubkey { pubkey }` | Rotate the owner pubkey. Sign with the current owner key. |
| `deploy_root_model` | `deployRootModel` | `ParamsOfDeployRootModel { owner_pubkey }` | Deploy a `RootModel` for an owner pubkey. The super root performs the deploy with an internal `new`; `RootModel`'s constructor requires the super root as sender, so this is the only way to create one. |
| `get_root_model_address` | `getRootModelAddress` | `ParamsOfGetRootModelAddress { owner_pubkey }` → `ResultOfGetAddress { address }` | Deterministic `RootModel` address for an owner pubkey. |
| `get_owner_pubkey` | `getOwnerPubkey` | → `ResultOfGetOwnerPubkey { owner_pubkey }` | The configured owner pubkey. |
| `get_version` | `getVersion` | → `ResultOfGetVersion { version, name }` | Contract version + name. |

Not wrapped: `updateCode` (upgrade path, driven from the contracts repo).

### RootModel — `airegistry/root_model.rs`

Per-owner model registry. Derives the deterministic `(sellerPubkey, nonce)`
address every `TokenContract` of that owner lives at.

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `get_token_contract_address` | `getTokenContractAddress` | `ParamsOfGetTokenContractAddress { seller_pubkey, nonce }` → `ResultOfGetTokenContractAddress { address }` | Deterministic `TokenContract` address for `(seller_pubkey, nonce)`. |
| `get_owner_pubkey` | `getOwnerPubkey` | → `ResultOfGetOwnerPubkey { owner_pubkey }` | The configured owner pubkey. |
| `get_version` | `getVersion` | → `ResultOfGetVersion { version, name }` | Contract version + name. |

Not wrapped: `registerTokenContract`, deliberately. Since 4.0.36 it no longer
deploys anything — it is a `pure` self-announcement that recomputes the
canonical address from `(sellerPubkey, nonce)`, requires `msg.sender` to equal
it, and emits `TokenContractRegistered`. Only the already-deployed deal
satisfies that, so a wrapper for it could reach nothing but
`ERR_INVALID_SENDER`. Deals are created by the seller's note — see
[§2](#2-note-side-inference--dexprivate_noters).

### TokenContract — `airegistry/token_contract.rs`

Per-deal streaming escrow. Holds the buyer's SHELL deposit and settles by
CLAIMED CONSUMPTION, not by elapsed ticks: the first tick is a probe frozen at
`open()`, and past it the escrow stays whole until the seller claims cumulative
consumption (`claimTokens`), which becomes payable only by outliving its
promote window (`finalize`). Subscriptions settle take-or-pay per week
(`settleWeek`) instead. See the contract header in
[`TokenContract.sol`](../../contracts/airegistry/TokenContract.sol) for the
three-deep claim pipeline and the dispute economics.

**Funding**

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `fund_from_order_book` | `fundFromOrderBook` | `ParamsOfFundFromOrderBook { paid, buyer_note, buyer_pubkey, deal_flags }` | Funded from a matched order-book buy; sender must be the order book. The only funding door the SDK wraps. |

Not wrapped: `fundDeal` (the seller's note ships gas as attached ECC[2] and the
`2 * pricePerTick` mirror bond as a figure — the door that replaced
`fundSellerBond`) and `fundBuyerBond` (the buyer's own `2P`, held outside the
escrow, staked on a dispute). Both are internal-only and value-bearing, so they
are reached from the note or the giver, never from a signed external call.

**Streaming lifecycle**

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `open` | `open` | `ParamsOfOpen { endpoint_cipher }` | Seller posts the endpoint encrypted to the buyer's pubkey and freezes the probe tick. Requires the mirror bond already funded. The note is NOT locked. |
| `stop` | `stop` | — | Buyer exit: settles trusted ticks and returns the rest of the escrow (spec §4.1). Also the seller-no-show path — silence leaves the last claim unpromoted. |
| `dispute` | `dispute` | — | Buyer contests the `claimed - trusted` delta (spec §4.2). |
| `release_dispute` | `releaseDispute` | — | Buyer releases a dispute it raised. |
| `resolve_dispute_timeout` | `resolveDisputeTimeout` | — | Seller resolves a dispute after the dispute window. |
| `cleanup_unopened` | `cleanupUnopened` | — | Recover funds from a funded-but-unopened deal (no-show, §2.1). The deal SURVIVES: it returns to unfunded and can take a new offer. Residual native gas sweeps to the seller note, not to a fixed sink. It tells both notes through `onDealClosed`, so each emits `PrivateNote.InferenceDealClosed` — see [indexer.md § Deal-address reuse](../tech-specs/indexer.md#deal-address-reuse). |
| `withdraw_shell` | `withdrawShell` | `ParamsOfWithdrawShell { amount }` | Seller withdraws finalized SHELL. |

Not wrapped: `claimTokens`, `finalize`, `settleWeek`, `acceptProbe`,
`sellerStop`, `close`, `destroy`, `touchDeal`, `postFromNote`, `onSellClosed`.
`finalize` and `settleWeek` are permissionless, so anything may drive them; the
rest are seller-only or note/book callbacks.

**Getters** (each returns the matching `ResultOf…`)

| Method | Contract method | Returns | Description |
| --- | --- | --- | --- |
| `get_state` | `getState` | `funded, opened, probe_accepted, disputed, deposit, probe_tick, finalized_owed, tokens_final, tokens_pending, probe_time, last_claim_time, dispute_time, funded_time` | Full deal state machine, escrow, and the claim pipeline (`tokens_final` is the only figure money is computed from). |
| `get_seller_bond` | `getSellerBond` | `bond_funded, bond_held, bond_required` | Seller mirror-bond state; `bond_required` is `2 * price_per_tick`. |
| `get_offer` | `getOffer` | `offer_posted` | Whether a sell offer is live on the book. |
| `get_config` | `getConfig` | `platform_fee_bps, min_claim_interval, min_seconds_per_tick, dispute_window` | Protocol-wide constants (spec §9.1). The two claim bounds replaced the old settle-window / stream-timeout pair. |
| `get_fees` | `getFees` | `fee_accrued, ticks_finalized, ever_disputed, rebate_max_bps, rebate_slope_bps` | Accrued platform fees + rebate parameters. |
| `get_deal` | `getDeal` | `tick_size, price_per_tick, max_ticks` | Deal economics. |
| `get_parties` | `getParties` | `buyer, seller_note` | Buyer + seller-note addresses. |
| `get_seller` | `getSeller` | `seller_pubkey, root_model_address, nonce` | Seller identity used for canonical-address derivation. |
| `get_buyer_pubkey` | `getBuyerPubkey` | `buyer_pubkey` | Buyer pubkey. |
| `get_endpoint_cipher` | `getEndpointCipher` | `endpoint_cipher` (hex) | Encrypted endpoint handed over at `open`. |
| `get_model_name` | `getModelName` | model name | Model name this deal serves. |
| `get_shell_balance` | `getShellBalance` | SHELL balance | The deal's escrow LEDGER (`_balance`). Not the ECC[2] on the account — that is the gas reserve every entrypoint burns from, and it is read off the account record, not through a getter. |
| `get_version` | `getVersion` | `version, name` | Contract version + name. |

Not wrapped: `getSubscription` (take-or-pay week accounting), `getBuyerBond`,
`getModelHash`.

### InferenceOrderBook — `airegistry/inference_order_book.rs`

Per-model CLOB. Matches SELL offers (each backed by a `TokenContract`) against
BUY orders paid in SHELL escrow (spec §2 + §8). A subscription is not a separate
method: it is a BUY order carrying subscription `flags`, which the book hands
down to the deal at match.

**Matching / orders**

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `process_head` | `processHead` | — | Drain the matching queue across continuation txs (`> MAX_MATCHES_PER_CALL`). |
| `place_sell_offer` | `placeSellOffer` | `ParamsOfPlaceSellOffer { price_per_tick, max_ticks, flags, seller_pubkey, nonce, owner_note, deadline }` | Post a SELL offer; the book recomputes the canonical `TokenContract` address from `seller_pubkey + nonce` and requires the **caller** to be it, so the deal address is never taken from the message. Sender is the seller's `TokenContract`, not the note; `owner_note` records the note a fill settles back to. |
| `place_buy_order` | `placeBuyOrder` | `ParamsOfPlaceBuyOrder { client_order_id, escrow, max_price_per_tick, ticks, flags, deadline, buyer_pubkey, deposit_hash }` | Place a BUY order; sender (buyer note) forwards the SHELL escrow. `deadline = 0` is good-till-cancel. |
| `cancel_order` | `cancelOrder` | `ParamsOfOrderId { order_id }` | Cancel one resting order and refund its remaining escrow. |
| `cancel_all_orders` | `cancelAllOrders` | — | Cancel every resting order owned by the caller. |
| `expire_order` | `expireOrder` | `ParamsOfOrderId { order_id }` | Retire an order whose deadline has passed and refund it. |
| `request_weekly_median` | `requestWeeklyMedian` | `ParamsOfRequestWeeklyMedian { event_id, oracle_list_hash, token_type }` | Ask the engine to refresh the model's reference price. |

Not wrapped: `reportFinalized` (deal → book settlement report),
`onHandoverAccepted` / `getHandoverBuyer` (match handover).

**Getters**

| Method | Contract method | Returns | Description |
| --- | --- | --- | --- |
| `get_weekly_median_price` | `getWeeklyMedianPrice` | `price` | Current reference price. |
| `get_order` | `getOrder` | `ParamsOfGetOrder { id }` → `note, token_contract, price, amount, escrow, deadline, flags, is_buy, ts` | One order's full record. |
| `get_best_bid_ask` | `getBestBidAsk` | `has_bid, bid, has_ask, ask` | Top of book. |
| `get_stats` | `getStats` | `next_order_id, order_count, executed_notional, executed_ticks` | Book-wide counters. |
| `get_queue_size` | `getQueueSize` | `size` | Pending match-queue depth. |
| `get_params` | `getParams` | `model_hash, platform_fee_bps` | Per-book parameters. |
| `get_version` | `getVersion` | `version, name` | Contract version + name. |

Not wrapped: `getModelName`.

---

## 2. Note-side inference — `dex/private_note.rs`

Sent *from* the `PrivateNote`, owner-signed. These are how a buyer or seller note
actually participates: the note forwards SHELL escrow and is the address recorded
on-chain.

| Method | Contract method | Params | Description |
| --- | --- | --- | --- |
| `deploy_inference_order_book` | `deployInferenceOrderBook` | `ParamsOfDeployInferenceOrderBook { model_hash, model_name }` | Deploy an `InferenceOrderBook` from this note (permissionless at the deterministic per-model address). The book code is baked into the note. |
| `deploy_deal` | `deployDeal` | `ParamsOfDeployDeal { nonce, model_name, model_hash, price_per_tick, max_ticks, gas_reserve }` | Deploy this note's canonical deal `TokenContract`, the only way to create one: the constructor requires `msg.sender` to be the canonical note for its `depositIdentifierHash`. The address derives from `(this note's key, nonce)` and the terms are constructor arguments, not part of it. `gas_reserve` is ECC[2] sent with the deploy — budget `0.300 + 0.015 * max_ticks` SHELL. |
| `fund_deploy_shell` | `fundDeployShell` | `ParamsOfFundDeployShell { nonce, tc_shell }` | Top up the deal's ECC[2] gas reserve, its only route to more. Sends under flag 1 so the credit stays SHELL — the pocket every entrypoint burns from; the deal mints its own native floor and needs nothing sent ahead of it. `tc_shell = 0` skips the send entirely. |
| `post_sell_offer` | `postSellOffer` | `ParamsOfPostSellOffer { flags, nonce, ttl }` | Post a SELL offer, indirectly: the note authorises its canonical `TokenContract` for `nonce` (`postFromNote`) and that deal posts the offer with its own constructor-pinned `price_per_tick` / `max_ticks` / `model_hash`. The terms cannot be overridden per offer — deploy a deal with different ones. The deal must already exist, or the call is a no-op. `ttl` is mandatory and bounded: `0` or `> MAX_SELL_TTL` (3600 s) reverts `ERR_SELL_DEADLINE_TOO_LONG` — `0` is NOT good-till-cancel here, unlike the BUY deadline it resembles. |
| `place_inference_buy` | `placeInferenceBuy` | `ParamsOfPlaceInferenceBuy { model_hash, max_price_per_tick, ticks, escrow, flags, deadline }` | Place a BUY order with SHELL escrow. |
| `cancel_inference_order` | `cancelInferenceOrder` | `ParamsOfCancelInferenceOrder { model_hash, order_id }` | Cancel one resting inference order owned by this note. |
| `cancel_all_inference_orders` | `cancelAllInferenceOrders` | `ParamsOfCancelAllInferenceOrders { model_hash }` | Cancel all resting inference orders owned by this note. |
| `stream_stop` | `streamStop` | `ParamsOfStreamDeal { token_contract }` | Buyer note stops the stream cleanly (amicable exit, §4.1). |
| `stream_dispute` | `streamDispute` | `ParamsOfStreamDeal { token_contract }` | Buyer note disputes the claimed delta (§4.2). |
| `stream_cleanup` | `streamCleanup` | `ParamsOfStreamDeal { token_contract }` | Buyer note recovers a deal it funded and the seller never opened: refunds the whole deposit, returns the bond unslashed, destroys the deal. Scoped to the never-opened case by a permanent latch on the deal. |

Not wrapped: `fundDeal`, the seller-note call that ships the deal its gas and
the `2 * pricePerTick` mirror bond; `fundBuyerBondNow`, the buyer's way back
into a match whose bond his note could not cover at fill time (the bond
otherwise goes out by itself from `onInferenceFilled`); and `creditFromDeal` /
`creditFromBook` / `touchDeal` / `onDealClosed`, which are callbacks the deal
and the book send back to the note.

The three `stream_*` calls name the deal by **address**, not by the
`(sellerPubkey, nonce)` pair the address derives from, so a wrong address is not
silently a no-op: the note sends them `bounce: true` and the value comes back.
Authorisation is on the deal side — `stop` / `dispute` / `cleanup` each require
`msg.sender` to be the recorded buyer.

**Pending-buy state**

The note-side stream/dispute locks are gone: the seller's collateral is the
per-deal mirror bond inside the `TokenContract`, so a note is never frozen by an
inference stream or dispute, and there is nothing for the SDK to take, release,
or clear. One note runs as many deals at once as its owner cares to open.

| Method | Contract method | Description |
| --- | --- | --- |
| `get_pending_place_buy_lock` / `get_pending_place_buy_token_type` | `_pendingPlaceBuyLock` / `_pendingPlaceBuyTokenType` | Inspect a pending buy in flight. |
| `get_inference_order_book_address` | `getInferenceOrderBookAddress` | Deterministic book address for a `modelHash`. |

---

## 3. `Dex` facade — `crates/chain/src/test_helpers.rs`

Behind the `test-helpers` feature. Each helper resolves addresses, sends from the
correct note, and decodes getters into plain Rust values. This is the surface the
inference e2e tests drive.

**Order book / streaming (note-side, via the facade)**

- `deploy_inference_order_book`, `get_inference_order_book_address`
- `deploy_deal`, `fund_deploy_shell`
- `post_sell_offer`, `place_inference_buy`
- `cancel_inference_order`, `cancel_all_inference_orders`
- `stream_stop`, `stream_dispute`, `stream_cleanup`

**TokenContract (deal escrow, via the facade)**

- `token_contract_open`, `token_contract_resolve_dispute_timeout`, `token_contract_withdraw_shell`
- `token_contract_get_state`, `token_contract_get_seller_bond`, `token_contract_get_offer`
- `token_contract_get_fees`, `token_contract_get_config`, `token_contract_get_parties`, `token_contract_get_shell_balance`

**Order-book getters (decoded)**

- `inference_get_order`, `inference_get_stats`, `inference_get_queue_size`
- `inference_get_best_bid_ask`, `inference_get_weekly_median_price`
- `inference_expire_order`, `inference_book_account`

---

A full deal walks: seller `deploy_root_model` → `deploy_deal` →
`fundDeal` (the bond, no wrapper yet) → `post_sell_offer`; buyer `place_inference_buy` →
the match funds the deal through `fundFromOrderBook`; seller `token_contract_open`
→ `claimTokens` per claim with `finalize` promoting them; buyer `stream_stop` (or
`stream_dispute` / `stream_cleanup`). The settlement read-model built from the
emitted events is described in [`airegistry-inference.md`](airegistry-inference.md).

> Browsing tip: `cargo doc -p dodex-contracts --no-deps --open` renders every
> wrapper method with its `ParamsOf…` / `ResultOf…` types and these doc comments.
