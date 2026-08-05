# DEX.DO Event Routing

This document catalogs every `event` defined under `contracts/dex`: when it is emitted, which `dst` address it is routed to, and which events are currently declared only and not actually emitted. The AI-inference registry under `contracts/airegistry` has its own events (id ranges and typed decoders) — see [airegistry-inference.md](airegistry-inference.md).

## General principle

`dst` only exists on external event objects created via `emit Event{dest: ...}(...)`.

Every emitted event is routed to `address.makeAddrExtern(EVENT_ID, bitCntAddress)`, where `EVENT_ID` is the per-event constant in [`modifiers.sol`](../../contracts/dex/modifiers/modifiers.sol) and `bitCntAddress` is the constant `256`. Because the width is fixed, each `EVENT_ID` yields a single stable `dst` string that is identical across every instance of the emitting contract — so the `dst` of an external event is a 1:1 discriminator of its event type, readable from the message header before the body is decoded.

| Case | Routed to |
| --- | --- |
| Emitted external events | `address.makeAddrExtern(EVENT_ID, bitCntAddress)` |
| Event only declared | no `dst`, because there is no `emit` |

## RootPN

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `VoucherGenerated` | `skUCommit`, `voucherNominal`, `tokenType` | In `generateVoucher()` after the nominal check and a possible `SHELL -> SHELL_FEE` remap. | `address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress)` = `135` | [RootPN.sol:651](../../contracts/dex/RootPN.sol:651) |
| `PrivateNoteDeployed` | `depositIdentifierHash`, `noteAddress`, `initialBalance` | In the `privateNoteDeployed()` callback after `_deployedValues[tokenType]` is incremented. | `address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress)` = `101` | [RootPN.sol:433](../../contracts/dex/RootPN.sol:433) |
| `NullifierDeployed` | `nullifierAddress`, `value` | In `sendEccShellToPrivateNote()` after the zk check succeeds and the `Nullifier` is deployed. | `address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress)` = `102` | [RootPN.sol:310](../../contracts/dex/RootPN.sol:310) |
| `TokensWithdrawn` | `amounts`, `noteAddress`, `to`, `dapp_id` | In `withdrawTokens()` after the collateral is transferred to the destination wallet. `noteAddress` is the withdrawing PrivateNote (`msg.sender`); `to` is the destination wallet. | `address.makeAddrExtern(ROOTPN_TOKENS_WITHDRAWN, bitCntAddress)` = `154` | [RootPN.sol:716](../../contracts/dex/RootPN.sol:716) |
| `ProtocolFeeCollected` | `tokenType`, `amount` | In `collectProtocolFee()` after `_protocolFees[tokenType]` is incremented. Called by each `OrderBook` at shutdown to mark its accumulated taker-fee share as owner-withdrawable; the backing real ECC already sits in this RootPN's reserves. | `address.makeAddrExtern(ROOTPN_PROTOCOL_FEE_COLLECTED, bitCntAddress)` = `155` | [RootPN.sol:799](../../contracts/dex/RootPN.sol:799) |
| `ProtocolFeeWithdrawn` | `to`, `dapp_id`, `tokenType`, `amount` | In `withdrawProtocolFees()` (root-owner only) after the amount is debited from `_protocolFees` / `_deployedValues` and transferred to `to` routed to `dapp_id`. | `address.makeAddrExtern(ROOTPN_PROTOCOL_FEE_WITHDRAWN, bitCntAddress)` = `156` | [RootPN.sol:825](../../contracts/dex/RootPN.sol:825) |
| `DealWriteOffReported` | `deal`, `amount` | In `reportDealWriteOff()`, when an inference deal ends owing more than it can pay and the shortfall is written off at protocol level rather than by the deal itself. | `address.makeAddrExtern(ROOTPN_DEAL_WRITE_OFF, bitCntAddress)` = `170` | [RootPN.sol:769](../../contracts/dex/RootPN.sol:769) |

## RootOracle

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OracleDeployed` | `oracle`, `pubkey`, `name` | In `deployOracle()` immediately after a new `Oracle` is deployed. | `address.makeAddrExtern(ROOTORACLE_ORACLE_DEPLOYED, bitCntAddress)` = `136` | [RootOracle.sol:69](../../contracts/dex/RootOracle.sol:69) |

## Oracle

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OracleEventListDeployed` | `eventListAddress`, `index`, `description` | In the `Oracle` constructor for the default list with `index = 0` and an empty description, and in `deployEventList()` for additional lists with the caller-supplied description. | `address.makeAddrExtern(ORACLE_DEPLOYED, bitCntAddress)` = `104` | [Oracle.sol:67](../../contracts/dex/Oracle.sol:67), [Oracle.sol:84](../../contracts/dex/Oracle.sol:84) |

## OracleEventList

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `EventAdded` | `eventId`, `eventName`, `oracleFee`, `deadline` | In `addEvent()` after a new `EventInfo` is written to `_events[eventId]`, and again in `addRangeEvent()` for a range market — a range event is an ordinary event plus its bounds, so it announces itself through both. | `address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress)` = `133` | [OracleEventList.sol:171](../../contracts/dex/OracleEventList.sol:171), [OracleEventList.sol:223](../../contracts/dex/OracleEventList.sol:223) |
| `RangeEventAdded` | `eventId`, `ob`, `bounds` | In `addRangeEvent()` immediately after `EventAdded`, once `_rangeData[eventId]` is written. It names the `InferenceOrderBook` whose weekly median resolves the market and the ascending bounds bracketing the outcomes, so a consumer can tell from the log alone which book a range PMP resolves on, with no per-event getter call. | `address.makeAddrExtern(ORACLE_RANGE_EVENT_ADDED, bitCntAddress)` = `162` | [OracleEventList.sol:224](../../contracts/dex/OracleEventList.sol:224) |
| `EventConfirmed` | `eventId`, `pmpAddress` | In `confirmEvent()` after `deadline` and `oracleFee` are checked, `eventInfo.count` is incremented, and `PMP.approveEvent(...)` is called. | `address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress)` = `106` | [OracleEventList.sol:272](../../contracts/dex/OracleEventList.sol:272) |
| `DescriptionUpdated` | `description` | In `setDescription()` after `_description` is replaced. | `address.makeAddrExtern(ORACLE_LIST_DESCRIPTION_UPDATED, bitCntAddress)` = `107` | [OracleEventList.sol:128](../../contracts/dex/OracleEventList.sol:128) |

## PrivateNote

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OwnerChanged` | `oldPubkey`, `newPubkey` | In `changeOwner()` after `_ephemeralPubkey` is replaced. | `address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress)` = `112` | [PrivateNote.sol:506](../../contracts/dex/PrivateNote.sol:506) |
| `PMPDeployed` | `eventId`, `tokenType`, `pmpAddress`, `oracleEventLists`, `oracleFee` | In `deployPMP()` after the PMP address is computed, `_stakes[hash]` is prepared, and `_busy` is set, immediately before `new PMP`. | `address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress)` = `111` | [PrivateNote.sol:1280](../../contracts/dex/PrivateNote.sol:1280) |
| `StakeCancelled` | `stakeController`, `value` | In `onStakeCancelled()` after the stake record is removed and the funds are returned to `_balance` / `_couponsValue`. | `address.makeAddrExtern(PRIVATENOTE_STAKE_CANCELLED, bitCntAddress)` = `115` | [PrivateNote.sol:1502](../../contracts/dex/PrivateNote.sol:1502) |
| `FullSetStakeConfirmed` | `stakeController`, `amount` | In `onSplitAccepted()` after split amounts are credited to `stake.amount[]` and any unused collateral is refunded. | `address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress)` = `138` | [PrivateNote.sol:1601](../../contracts/dex/PrivateNote.sol:1601) |
| `FullSetStakeCancelled` | `stakeController`, `value` | In `onMergeAccepted()` after merged outcome tokens are debited and the collateral is returned to `_balance[tokenType]`. | `address.makeAddrExtern(PRIVATENOTE_MERGE_CONFIRMED, bitCntAddress)` = `139` | [PrivateNote.sol:1713](../../contracts/dex/PrivateNote.sol:1713) |
| `StakeConfirmed` | `stakeController`, `outcome`, `amount`, `betType` | In `onStakeAccepted()` after `candidateAmount` is moved into the confirmed arrays (`amount`, `debtAmount`, `couponsAmount`). | `address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress)` = `113` | [PrivateNote.sol:1825](../../contracts/dex/PrivateNote.sol:1825) |
| `ClaimAccepted` | `stakeController`, `outcome`, `payout` | In `onClaimAccepted()` only when `outcome.hasValue() == true`; the unresolved branch just clears `_busy` and returns without emitting. | `address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress)` = `114` | [PrivateNote.sol:1899](../../contracts/dex/PrivateNote.sol:1899) |
| `TransferInitiated` | `dest`, `tokenType`, `amount` | In `initTransfer()` after the amount is debited from `_balance`, `_pendingTransferAmount` is recorded, and before the `offerTransfer()` call on the receiving `PrivateNote`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress)` = `149` | [PrivateNote.sol:2312](../../contracts/dex/PrivateNote.sol:2312) |
| `TransferReceived` | `from`, `tokenType`, `amount` | In `offerTransfer()` after `_balance[tokenType]` is credited on the receiving side and before `onTransferAccepted()`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress)` = `150` | [PrivateNote.sol:2354](../../contracts/dex/PrivateNote.sol:2354) |
| `OrderSubmitted` | `clientOrderId`, `outcomeId`, `isBuy`, `price`, `amount`, `flags`, `eventId`, `tokenType` | In `placeOrder()` immediately after parameter validation and before the order is dispatched to `OrderBook`. The event records the submission itself, not the on-book confirmation. | `address.makeAddrExtern(PRIVATENOTE_ORDER_SUBMITTED, bitCntAddress)` = `151` | [PrivateNote.sol:2566](../../contracts/dex/PrivateNote.sol:2566) |
| `OrderPlacedConfirmed` | `orderBook`, `orderId`, `clientOrderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount` | In `onOrderPlaced()` after the `OrderBook` is validated and the per-order `feeReserve` / `lock` are written into the local `PrivateNote` state. | `address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress)` = `147` | [PrivateNote.sol:2730](../../contracts/dex/PrivateNote.sol:2730) |
| `OrderCancelledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `isBuy`, `returnAmount` | In `onOrderCancelled()` after the remaining buy-lock is returned to `_balance`, or outcome tokens are returned to the stake. | `address.makeAddrExtern(PRIVATENOTE_ORDER_CANCELLED, bitCntAddress)` = `152` | [PrivateNote.sol:3027](../../contracts/dex/PrivateNote.sol:3027) |
| `OrderFilledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `filledAmount`, `clearingPrice`, `isBuy`, `feeAmount`, `isRebate`, `isFinal` | In `onOrderFilled()` after the local balances / stakes / fee reserves / locks are updated for the fill reported by `OrderBook`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress)` = `148` | [PrivateNote.sol:3197](../../contracts/dex/PrivateNote.sol:3197) |
| `OrderPlaceRejected` | `orderBook`, `eventId`, `clientOrderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `opNonce` | In `onOrderRejected()` after the buy-lock or sell-lock has been released. Carries the full original `PlaceParams` so off-chain monitors can attribute the rejection back to its `clientOrderId`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_REJECTED, bitCntAddress)` = `153` | [PrivateNote.sol:2826](../../contracts/dex/PrivateNote.sol:2826) |
| `InferenceOrderPlacedConfirmed` | `orderBook`, `tokenContract`, `orderId`, `isBuy`, `price`, `ticks` | In `onInferencePlaced()` after the caller is verified as the canonical `InferenceOrderBook` for `modelHash` (derived from the note's baked `_inferenceOrderBookCode`). Owner-facing mirror of a resting inference-order placement; `tokenContract` is the seller's `TokenContract` for a SELL offer and `0` for a BUY (no TC until a match binds one). | `address.makeAddrExtern(PRIVATENOTE_INFERENCE_PLACED, bitCntAddress)` = `1100` | [PrivateNote.sol:572](../../contracts/dex/PrivateNote.sol:572) |
| `InferenceFilledConfirmed` | `orderBook`, `tokenContract`, `orderId`, `ticks`, `clearingPrice`, `isBuy` | In `onInferenceFilled()` after the same canonical-book auth check. Owner-facing mirror of an inference match; `tokenContract` is the per-deal `TokenContract` the owner reads to track the stream. | `address.makeAddrExtern(PRIVATENOTE_INFERENCE_FILLED, bitCntAddress)` = `1101` | [PrivateNote.sol:708](../../contracts/dex/PrivateNote.sol:708) |
| `InferenceOrderRemoved` | `book`, `orderId` | In `onInferenceOrderRemoved()`, when the book tells the note one of its resting inference orders is gone — cancelled or expired. `InferenceOrderRejectedMirror` is currently emitted to this same id, which breaks the one-id-one-payload rule the routing scheme rests on; it moves to `1102`. | `address.makeAddrExtern(PRIVATENOTE_INFERENCE_REMOVED, bitCntAddress)` = `165` | [PrivateNote.sol:678](../../contracts/dex/PrivateNote.sol:678) |
| `InferenceDealClosed` | `deal` | In `onDealClosed()` when a deal `TokenContract` reports it has settled and died, and again from `onBounce()` when the close message bounces — the note drops the deal either way. | `address.makeAddrExtern(PRIVATENOTE_INFERENCE_DEAL_CLOSED, bitCntAddress)` = `166` | [PrivateNote.sol:650](../../contracts/dex/PrivateNote.sol:650), [PrivateNote.sol:2077](../../contracts/dex/PrivateNote.sol:2077) |
| `DealCredited` | `deal`, `amount` | In `creditFromDeal()` when a deal `TokenContract` pays settlement back into the note. | `address.makeAddrExtern(PRIVATENOTE_DEAL_CREDITED, bitCntAddress)` = `163` | [PrivateNote.sol:1076](../../contracts/dex/PrivateNote.sol:1076) |
| `BookCredited` | `book`, `amount` | In `creditFromBook()` when the order book returns escrow the note had committed — an order that left the book without being filled. | `address.makeAddrExtern(PRIVATENOTE_BOOK_CREDITED, bitCntAddress)` = `164` | [PrivateNote.sol:1113](../../contracts/dex/PrivateNote.sol:1113) |
| `StakeForfeitConfirmed` | `pmp`, `stakeHash` | In `onForfeitAccepted()`, the note's acknowledgement that the PMP accepted the forfeit it was asked for. Pairs with `PMP.StakeForfeited` on `167`. | `address.makeAddrExtern(PRIVATENOTE_STAKE_FORFEITED, bitCntAddress)` = `168` | [PrivateNote.sol:1446](../../contracts/dex/PrivateNote.sol:1446) |
| `StakeDroppedLocally` | `stakeHash`, `tokenType`, `amount`, `debtAmount`, `couponsAmount` | In `onBounce()` when a forfeit message bounces: the note drops the stake from its own books anyway, itemised per leg. Authoritative for stake accounting downstream — dodex-rewards reads this payload out of `raw_events`, so it must not be dropped at ingest. | `address.makeAddrExtern(PRIVATENOTE_STAKE_DROPPED, bitCntAddress)` = `169` | [PrivateNote.sol:2110](../../contracts/dex/PrivateNote.sol:2110) |

## PMP

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `StakeAccepted` | `note`, `outcomeId`, `amount`, `betType` | In `acceptStake()` after pool accounting is updated and the `PrivateNote.onStakeAccepted(...)` callback is dispatched. | `address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress)` = `118` | [PMP.sol:736](../../contracts/dex/PMP.sol:736) |
| `ApprovedByOracle` | `oracleEventList`, `oraclePubkey` | In `approveEvent()` only on the final required oracle approval, when `_approvedOracleEvents == _numberOfOracleEvents`. | `address.makeAddrExtern(PMP_APPROVED_BY_ORACLE, bitCntAddress)` = `119` | [PMP.sol:530](../../contracts/dex/PMP.sol:530) |
| `Resolved` | `outcomeId` | In `resolve()` after the resolution coefficients are computed and after `CreatorFeeCollected` if a fee was charged. | `address.makeAddrExtern(PMP_RESOLVED, bitCntAddress)` = `120` | [PMP.sol:1311](../../contracts/dex/PMP.sol:1311) |
| `ClaimProcessed` | `note`, `payout`, `win` | In `claim()` only for an already-resolved market, after the `PrivateNote.onClaimAccepted(...)` callback — once in the debt-refund branch (payout = refunded debt) and once in the main payout branch. | `address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress)` = `121` | [PMP.sol:1389](../../contracts/dex/PMP.sol:1389), [PMP.sol:1507](../../contracts/dex/PMP.sol:1507) |
| `NetworkFeeBurned` | `amount` | In `setResultStart()` on the first approval only, when `_stakeStart` is still zero: the market goes live, `gosh.burnecc` destroys the network fee, and the event reports the burn. By that point every oracle fee has been dispatched, so the only SHELL the contract still holds is that fee. | `address.makeAddrExtern(PMP_NETWORK_FEE_BURNED, bitCntAddress)` = `122` | [PMP.sol:599](../../contracts/dex/PMP.sol:599) |
| `TimingsSet` | `stakeStart`, `stakeEnd`, `resultStart`, `resultEnd` | In `setTimings()` after `_stakeStart`, `_resultStart`, `_approved = true` are committed and an optional auto-freeze runs. | `address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress)` = `124` | [PMP.sol:622](../../contracts/dex/PMP.sol:622) |
| `EventCancelled` | `-` | In `cancelEvent()` after `_isCancelled = true` is set. | `address.makeAddrExtern(PMP_EVENT_CANCELLED, bitCntAddress)` = `126` | [PMP.sol:662](../../contracts/dex/PMP.sol:662) |
| `PMPRejected` | `-` | In `rejectEvent()` when `OracleEventList` rejects the market or confirmation is no longer possible, after which the PMP enters rollback and `selfdestruct`. | `address.makeAddrExtern(PMP_REJECTED_BY_ORACLE, bitCntAddress)` = `132` | [PMP.sol:374](../../contracts/dex/PMP.sol:374) |
| `CreatorFeeCollected` | `fee` | In `resolve()` only when `_creatorFee > 0`, after the fee is sent to `PrivateNote(_deployer).acceptFee(...)`. | `address.makeAddrExtern(PMP_CREATOR_FEE_COLLECTED, bitCntAddress)` = `137` | [PMP.sol:1307](../../contracts/dex/PMP.sol:1307) |
| `PoolsFrozen` | `baseTotalPool` | In `_ensureFrozen()` after the freeze snapshot and after `OrderBook` is deployed. | `address.makeAddrExtern(PMP_POOLS_FROZEN, bitCntAddress)` = `140` | [PMP.sol:1007](../../contracts/dex/PMP.sol:1007) |
| `SplitProcessed` | `note`, `collateral` | In `splitFullSet()` after the `PrivateNote.onSplitAccepted(...)` callback. The event carries `F_use` — the quantized collateral actually consumed. | `address.makeAddrExtern(PMP_SPLIT_PROCESSED, bitCntAddress)` = `141` | [PMP.sol:1080](../../contracts/dex/PMP.sol:1080) |
| `MergeProcessed` | `note`, `collateral` | In `mergeFullSet()` after the `PrivateNote.onMergeAccepted(...)` callback. | `address.makeAddrExtern(PMP_MERGE_PROCESSED, bitCntAddress)` = `142` | [PMP.sol:1174](../../contracts/dex/PMP.sol:1174) |
| `StakeForfeited` | `wallet`, `stakeAmount`, `debtAmount`, `couponsAmount` | In `forfeitStake()`, the pool's side of a forfeit: the stake is written off against the wallet with its debt and coupon legs itemised. Pairs with `PrivateNote.StakeForfeitConfirmed` on `168`. | `address.makeAddrExtern(PMP_STAKE_FORFEITED, bitCntAddress)` = `167` | [PMP.sol:1676](../../contracts/dex/PMP.sol:1676) |

## OrderBook

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OrderPlaced` | `orderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `clientOrderId`, `depositHash`, `opNonce` | In `_emitOrderPlacedTo()` on successful order placement, before the `PrivateNote.onOrderPlaced(...)` callback. | `address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress)` = `143` | [OrderBook.sol:1248](../../contracts/dex/OrderBook.sol:1248) |
| `OrderCancelled` | `orderId`, `clientOrderId` | In the shared helper `_emitOrderCancelled()`, invoked from regular cancel, cancel-all, and internal no-book-entry / shutdown-cancel paths. | `address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress)` = `144` | [OrderBook.sol:1256](../../contracts/dex/OrderBook.sol:1256) |
| `OrderFilled` | `orderId`, `filledAmount`, `clearingPrice`, `feeAmount`, `isTaker`, `matchId`, `depositHash` | In `_processFillTo()` on every match/fill, before the `PrivateNote.onOrderFilled(...)` callback. | `address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress)` = `146` | [OrderBook.sol:1356](../../contracts/dex/OrderBook.sol:1356) |
| `PartialFill` | `orderId`, `clientOrderId`, `filledAmount`, `remainingAmount` | In `_emitPartialFill()` once per processed taker order when matching leaves a remainder. | `address.makeAddrExtern(OB_PARTIAL_FILL, bitCntAddress)` = `157` | [OrderBook.sol:1289](../../contracts/dex/OrderBook.sol:1289) |
| `FullyFilled` | `orderId`, `clientOrderId`, `filledAmount` | In `_emitFullyFilled()` once per processed taker order when it is fully filled. | `address.makeAddrExtern(OB_FULLY_FILLED, bitCntAddress)` = `158` | [OrderBook.sol:1294](../../contracts/dex/OrderBook.sol:1294) |
| `Queued` | `slot`, `queueId`, `entryType` | After a `QueueEntry` is successfully enqueued in `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(OB_QUEUED, bitCntAddress)` = `159` | [OrderBook.sol:538](../../contracts/dex/OrderBook.sol:538), [OrderBook.sol:574](../../contracts/dex/OrderBook.sol:574), [OrderBook.sol:606](../../contracts/dex/OrderBook.sol:606) |
| `Rejected` | `entryType`, `depositHash` | On an immediate reject of a place request during pre-validation in `executeBatch()`, and on queue overflow inside `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(OB_REJECTED, bitCntAddress)` = `160` | [OrderBook.sol:427](../../contracts/dex/OrderBook.sol:427), [OrderBook.sol:512](../../contracts/dex/OrderBook.sol:512), [OrderBook.sol:549](../../contracts/dex/OrderBook.sol:549), [OrderBook.sol:581](../../contracts/dex/OrderBook.sol:581) |
| `CallbackBounced` | `dest`, `lt` | In `onBounce()` whenever any outgoing `OrderBook -> PrivateNote` callback bounces back. This is an observability hook; OrderBook state is not automatically reverted. | `address.makeAddrExtern(OB_CALLBACK_BOUNCED, bitCntAddress)` = `161` | [OrderBook.sol:1531](../../contracts/dex/OrderBook.sol:1531) |

`PartialFill` / `FullyFilled` are derived aggregates that the contract emits for MM-friendly UX; the underlying state is already captured by `OrderFilled`. `Queued` / `Rejected` occur at the queue level, before any order ID is assigned. `CallbackBounced` is a diagnostic event — the OrderBook state is not automatically rolled back, and the bounced credit requires operator-driven recovery.

## Nullifier

| Contract | Event declarations | Comment |
| --- | --- | --- |
| `Nullifier` | None | `contracts/dex/Nullifier.sol` has no `event` declarations of its own. |
