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

---

## 1. Per-contract wrappers

### SuperRoot — `airegistry/super_root.rs`

Per-network root factory. Registers a `RootModel` at a deterministic address
derived from an owner pubkey.

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `set_pubkey` | `setPubkey` | `ParamsOfSetPubkey { pubkey }` | Rotate the owner pubkey. Sign with the current owner key. |
| `register_root` | `registerRoot` | `ParamsOfRegisterRoot { owner_pubkey }` | Deploy + register a `RootModel` for an owner pubkey. |
| `get_root_model_address` | `getRootModelAddress` | `ParamsOfGetRootModelAddress { owner_pubkey }` → `ResultOfGetAddress { address }` | Deterministic `RootModel` address for an owner pubkey. |
| `get_owner_pubkey` | `getOwnerPubkey` | → `ResultOfGetOwnerPubkey { owner_pubkey }` | The configured owner pubkey. |
| `get_version` | `getVersion` | → `ResultOfGetVersion { version, name }` | Contract version + name. |

### RootModel — `airegistry/root_model.rs`

Per-owner model registry. Derives + registers `TokenContract` children at
deterministic `(sellerPubkey, nonce)` addresses.

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `register_token_contract` | `registerTokenContract` | `ParamsOfRegisterTokenContract { seller_pubkey, nonce }` | Deploy + register a `TokenContract` for `(sellerPubkey, nonce)`. |
| `get_token_contract_address` | `getTokenContractAddress` | `ParamsOfGetTokenContractAddress { seller_pubkey, nonce }` → `ResultOfGetTokenContractAddress { address }` | Deterministic `TokenContract` address for `(sellerPubkey, nonce)`. |
| `get_owner_pubkey` | `getOwnerPubkey` | → `ResultOfGetOwnerPubkey { owner_pubkey }` | The configured owner pubkey. |
| `get_version` | `getVersion` | → `ResultOfGetVersion { version, name }` | Contract version + name. |

### TokenContract — `airegistry/token_contract.rs`

Per-deal streaming escrow. Holds the buyer's SHELL deposit and settles ticks one
at a time (probe-tick model, spec §3.1.2): `open → advance → stop / dispute /
reclaim`.

**Funding**

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `fund` | `fund` | `ParamsOfFund` | Buyer pays the deposit straight to the deal. |
| `fund_from_order_book` | `fundFromOrderBook` | `ParamsOfFundFromOrderBook` | Funded from a matched order-book buy; sender must be the order book. |
| `fund_probe_commission` | `fundProbeCommission` | — | Seller posts the probe commission (ECC[2] SHELL). |

**Streaming lifecycle**

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `open` | `open` | `ParamsOfOpen { endpoint_cipher }` | Seller opens the stream (freezes the probe tick). |
| `advance` | `advance` | — | Seller advances one tick (optimistic-accept after the settle window). |
| `stop` | `stop` | — | Buyer stops the stream cleanly (spec §4.1). |
| `dispute` | `dispute` | — | Buyer disputes the current ticks (spec §4.2). |
| `release_dispute` | `releaseDispute` | — | Buyer releases a dispute it raised. |
| `resolve_dispute_timeout` | `resolveDisputeTimeout` | — | Seller resolves a dispute after the dispute window (50/50 or burn). |
| `reclaim_on_timeout` | `reclaimOnTimeout` | — | Buyer reclaims on seller no-show after the stream timeout. |
| `cleanup_unopened` | `cleanupUnopened` | — | Recover funds from a funded-but-unopened deal (seller no-show, §2.1). Residual native is swept to the canonical SuperRoot. |
| `withdraw_shell` | `withdrawShell` | `ParamsOfWithdrawShell { amount, recipient }` | Seller withdraws finalized SHELL. |
| `destroy` | `destroy` | `ParamsOfDestroy { payout_address }` | Destroy a settled deal, sweeping any residue to `payout_address`. |

**Getters** (each returns the matching `ResultOf…`)

| Method | Contract method | Returns | Description |
| --- | --- | --- | --- |
| `get_state` | `getState` | `funded, opened, probe_accepted, disputed, deposit, prepaid, frozen, finalized_owed, prepaid_time, last_advance, dispute_time` | Full deal state machine + balances. |
| `get_probe` | `getProbe` | `probe_funded, probe_locked, probe_commission` | Probe-tick funding state. |
| `get_config` | `getConfig` | `platform_fee_bps, settle_window, stream_timeout, dispute_window` | Protocol-wide constants (spec §9.1). |
| `get_fees` | `getFees` | `fee_accrued, ticks_finalized, ever_disputed, rebate_max_bps, rebate_slope_bps` | Accrued platform fees + rebate parameters. |
| `get_deal` | `getDeal` | `tick_size, price_per_tick, max_ticks` | Deal economics. |
| `get_parties` | `getParties` | `buyer, seller_note` | Buyer + seller-note addresses. |
| `get_seller` | `getSeller` | `seller_pubkey, root_model_address, nonce` | Seller identity used for canonical-address derivation. |
| `get_buyer_pubkey` | `getBuyerPubkey` | `buyer_pubkey` | Buyer pubkey. |
| `get_endpoint_cipher` | `getEndpointCipher` | `endpoint_cipher` (hex) | Encrypted endpoint handed over at `open`. |
| `get_model_name` | `getModelName` | model name | Model name this deal serves. |
| `get_shell_balance` | `getShellBalance` | SHELL balance | The deal's physical SHELL (ECC[2]) balance. |
| `get_version` | `getVersion` | `version, name` | Contract version + name. |

### InferenceOrderBook — `airegistry/inference_order_book.rs`

Per-model CLOB. Matches SELL offers (each backed by a `TokenContract`) against
BUY orders / subscriptions paid in SHELL escrow (spec §2 + §8).

**Matching / orders**

| Method | Contract method | Params / Result | Description |
| --- | --- | --- | --- |
| `process_head` | `processHead` | — | Drain the matching queue across continuation txs (`> MAX_MATCHES_PER_CALL`). |
| `place_sell_offer` | `placeSellOffer` | `ParamsOfPlaceSellOffer { price_per_tick, max_ticks, token_contract, flags, seller_pubkey, nonce }` | Post a SELL offer; the book recomputes the canonical `TokenContract` address from `seller_pubkey + nonce` and rejects a mismatch. Sender is normally the seller note. |
| `place_buy_order` | `placeBuyOrder` | `ParamsOfPlaceBuyOrder { max_price_per_tick, ticks, flags, deadline, buyer_pubkey }` | Place a BUY order; sender (buyer note) forwards the SHELL escrow. `deadline = 0` is good-till-cancel. |
| `place_subscription` | `placeSubscription` | `ParamsOfPlaceSubscription { max_price_per_tick, ticks, auto_renew, buyer_pubkey }` | Place a subscription (weekly semantic order, spec §8). |
| `poke_subscription` | `pokeSubscription` | `ParamsOfOrderId { order_id }` | Roll a subscription onto its next cycle / forfeit the closing cycle's unspent budget. |
| `cancel_order` | `cancelOrder` | `ParamsOfOrderId { order_id }` | Cancel one resting order and refund its remaining escrow. |
| `cancel_all_orders` | `cancelAllOrders` | — | Cancel every resting order owned by the caller. |
| `claim_forfeit` | `claimForfeit` | `ParamsOfForfeit { order_id, cycle }` | Seller claims a share of a forfeited subscription cycle. |
| `request_weekly_median` | `requestWeeklyMedian` | `ParamsOfRequestWeeklyMedian { event_id, oracle_list_hash, token_type }` | Ask the engine to refresh the model's reference price. |

**Getters**

| Method | Contract method | Returns | Description |
| --- | --- | --- | --- |
| `get_weekly_median_price` | `getWeeklyMedianPrice` | `price` | Current reference price. |
| `get_order` | `getOrder` | `ParamsOfGetOrder { id }` → `note, token_contract, price, amount, escrow, deadline, flags, is_buy, ts` | One order's full record. |
| `get_best_bid_ask` | `getBestBidAsk` | `has_bid, bid, has_ask, ask` | Top of book. |
| `get_stats` | `getStats` | `next_order_id, order_count, executed_notional, executed_ticks` | Book-wide counters. |
| `get_queue_size` | `getQueueSize` | `size` | Pending match-queue depth. |
| `get_subscription` | `getSubscription` | `ParamsOfOrderId { order_id }` → `exists, period_start, cur_cycle, cycle_budget, cycle_spent, auto_renew` | Subscription state. |
| `get_forfeit` | `getForfeit` | `ParamsOfForfeit { order_id, cycle }` → `pool, funded_ticks` | Forfeited-cycle pool. |
| `get_params` | `getParams` | `model_hash, platform_fee_bps` | Per-book parameters. |
| `get_version` | `getVersion` | `version, name` | Contract version + name. |

---

## 2. Note-side inference — `dex/private_note.rs`

Sent *from* the `PrivateNote`, owner-signed. These are how a buyer or seller note
actually participates: the note forwards SHELL escrow and is the address recorded
on-chain.

| Method | Contract method | Params | Description |
| --- | --- | --- | --- |
| `deploy_inference_order_book` | `deployInferenceOrderBook` | `ParamsOfDeployInferenceOrderBook` | Deploy an `InferenceOrderBook` from this note (permissionless at the deterministic per-model address). The book code is baked into the note. |
| `post_sell_offer` | `postSellOffer` | `ParamsOfPostSellOffer` | Post a SELL offer to a book. |
| `place_inference_buy` | `placeInferenceBuy` | `ParamsOfPlaceInferenceBuy` | Place a BUY order with SHELL escrow. |
| `place_inference_subscription` | `placeInferenceSubscription` | `ParamsOfPlaceInferenceSubscription` | Place a subscription (semantic order). |
| `cancel_inference_order` | `cancelInferenceOrder` | `ParamsOfCancelInferenceOrder` | Cancel one resting inference order owned by this note. |
| `cancel_all_inference_orders` | `cancelAllInferenceOrders` | `ParamsOfCancelAllInferenceOrders` | Cancel all resting inference orders owned by this note. |
| `stream_stop` | `streamStop` | `ParamsOfStreamDeal` | Buyer note stops the stream cleanly (amicable exit, §4.1). |
| `stream_dispute` | `streamDispute` | `ParamsOfStreamDeal` | Buyer note disputes the current ticks (§4.2). |
| `stream_reclaim` | `streamReclaim` | `ParamsOfStreamDeal` | Buyer note reclaims a probe tick after the stream timeout (seller no-show). |

**Stream locks** (concurrency guards around streaming calls)

| Method | Contract method | Description |
| --- | --- | --- |
| `stream_lock` / `stream_unlock` | `streamLock` / `streamUnlock` | Take / release the per-deal streaming lock. |
| `stream_dispute_lock` / `stream_dispute_unlock` | `streamDisputeLock` / `streamDisputeUnlock` | Take / release the dispute lock. |
| `force_clear_stream_locks` | `forceClearStreamLocks` | Clear stale locks once they exceed `STREAM_LOCK_MAX`. Owner-signed. |
| `get_stream_locks` | `getStreamLocks` | Read the current lock state. |
| `get_pending_place_buy_lock` / `get_pending_place_buy_token_type` | getters | Inspect a pending buy in flight. |
| `get_inference_order_book_address` | `getInferenceOrderBookAddress` | Deterministic book address for a `modelHash`. |

---

## 3. `Dex` facade — `crates/chain/src/test_helpers.rs`

Behind the `test-helpers` feature. Each helper resolves addresses, sends from the
correct note, and decodes getters into plain Rust values. This is the surface the
inference e2e tests drive.

**Order book / streaming (note-side, via the facade)**

- `deploy_inference_order_book`, `get_inference_order_book_address`
- `post_sell_offer`, `place_inference_buy`, `place_inference_subscription`
- `cancel_inference_order`, `cancel_all_inference_orders`
- `stream_stop`, `stream_dispute`, `stream_reclaim`

**TokenContract (deal escrow, via the facade)**

- `token_contract_open`, `token_contract_advance`, `token_contract_fund_probe_commission`
- `token_contract_resolve_dispute_timeout`, `token_contract_withdraw_shell`
- `token_contract_get_state`, `token_contract_get_probe`, `token_contract_get_parties`, `token_contract_get_shell_balance`

**Order-book getters (decoded)**

- `inference_get_order`, `inference_get_stats`, `inference_get_queue_size`
- `inference_get_best_bid_ask`, `inference_get_subscription`, `inference_get_weekly_median_price`

---

A full deal walks: seller `register_root` → `register_token_contract` →
`post_sell_offer`; buyer `place_inference_buy` → match funds the `TokenContract`;
seller `token_contract_open` → `advance` per tick; buyer `stream_stop` (or
`stream_dispute` / `stream_reclaim`). The settlement read-model built from the
emitted events is described in [`airegistry-inference.md`](airegistry-inference.md).

> Browsing tip: `cargo doc -p dodex-contracts --no-deps --open` renders every
> wrapper method with its `ParamsOf…` / `ResultOf…` types and these doc comments.
