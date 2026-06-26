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
| `VoucherGenerated` | `skUCommit`, `voucherNominal`, `tokenType` | In `generateVoucher()` after the nominal check and a possible `SHELL -> SHELL_FEE` remap. | `address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress)` = `135` | [RootPN.sol:453](../../contracts/dex/RootPN.sol:453) |
| `PrivateNoteDeployed` | `depositIdentifierHash`, `noteAddress`, `initialBalance` | In the `privateNoteDeployed()` callback after `_deployedValues[tokenType]` is incremented. | `address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress)` = `101` | [RootPN.sol:365](../../contracts/dex/RootPN.sol:365) |
| `NullifierDeployed` | `nullifierAddress`, `value` | In `sendEccShellToPrivateNote()` after the zk check succeeds and the `Nullifier` is deployed. | `address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress)` = `102` | [RootPN.sol:273](../../contracts/dex/RootPN.sol:273) |
| `TokensWithdrawn` | `amounts`, `noteAddress`, `to`, `dapp_id` | In `withdrawTokens()` after the collateral is transferred to the destination wallet. `noteAddress` is the withdrawing PrivateNote (`msg.sender`); `to` is the destination wallet. | `address.makeAddrExtern(ROOTPN_TOKENS_WITHDRAWN, bitCntAddress)` = `154` | [RootPN.sol:503](../../contracts/dex/RootPN.sol:503) |
| `ProtocolFeeCollected` | `tokenType`, `amount` | In `collectProtocolFee()` after `_protocolFees[tokenType]` is incremented. Called by each `OrderBook` at shutdown to mark its accumulated taker-fee share as owner-withdrawable; the backing real ECC already sits in this RootPN's reserves. | `address.makeAddrExtern(ROOTPN_PROTOCOL_FEE_COLLECTED, bitCntAddress)` = `155` | [RootPN.sol:523](../../contracts/dex/RootPN.sol:523) |
| `ProtocolFeeWithdrawn` | `to`, `dapp_id`, `tokenType`, `amount` | In `withdrawProtocolFees()` (root-owner only) after the amount is debited from `_protocolFees` / `_deployedValues` and transferred to `to` routed to `dapp_id`. | `address.makeAddrExtern(ROOTPN_PROTOCOL_FEE_WITHDRAWN, bitCntAddress)` = `156` | [RootPN.sol:549](../../contracts/dex/RootPN.sol:549) |

## RootOracle

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OracleDeployed` | `oracle`, `pubkey`, `name` | In `deployOracle()` immediately after a new `Oracle` is deployed. | `address.makeAddrExtern(ROOTORACLE_ORACLE_DEPLOYED, bitCntAddress)` = `136` | [RootOracle.sol:65](../../contracts/dex/RootOracle.sol:65) |

## Oracle

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OracleEventListDeployed` | `eventListAddress`, `index`, `description` | In the `Oracle` constructor for the default list with `index = 0` and an empty description, and in `deployEventList()` for additional lists with the caller-supplied description. | `address.makeAddrExtern(ORACLE_DEPLOYED, bitCntAddress)` = `104` | [Oracle.sol:71](../../contracts/dex/Oracle.sol:71), [Oracle.sol:88](../../contracts/dex/Oracle.sol:88) |
| `EventPublished` | `eventId`, `eventName` | Not emitted in the current implementation. | No `dst` | [Oracle.sol:37](../../contracts/dex/Oracle.sol:37) |

## OracleEventList

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `EventAdded` | `eventId`, `eventName`, `oracleFee`, `deadline` | In `addEvent()` after a new `EventInfo` is written to `_events[eventId]`. | `address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress)` = `133` | [OracleEventList.sol:159](../../contracts/dex/OracleEventList.sol:159) |
| `EventConfirmed` | `eventId`, `pmpAddress` | In `confirmEvent()` after `deadline` and `oracleFee` are checked, `eventInfo.count` is incremented, and `PMP.approveEvent(...)` is called. | `address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress)` = `106` | [OracleEventList.sol:237](../../contracts/dex/OracleEventList.sol:237) |
| `DescriptionUpdated` | `description` | In `setDescription()` after `_description` is replaced. | `address.makeAddrExtern(ORACLE_LIST_DESCRIPTION_UPDATED, bitCntAddress)` = `107` | [OracleEventList.sol:115](../../contracts/dex/OracleEventList.sol:115) |

## PrivateNote

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OwnerChanged` | `oldPubkey`, `newPubkey` | In `changeOwner()` after `_ephemeralPubkey` is replaced. | `address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress)` = `112` | [PrivateNote.sol:361](../../contracts/dex/PrivateNote.sol:361) |
| `PMPDeployed` | `eventId`, `tokenType`, `pmpAddress`, `oracleEventLists`, `oracleFee` | In `deployPMP()` after the PMP address is computed, `_stakes[hash]` is prepared, and `_busy` is set, immediately before `new PMP`. | `address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress)` = `111` | [PrivateNote.sol:654](../../contracts/dex/PrivateNote.sol:654) |
| `StakeCancelled` | `stakeController`, `value` | In `onStakeCancelled()` after the stake record is removed and the funds are returned to `_balance` / `_couponsValue`. | `address.makeAddrExtern(PRIVATENOTE_STAKE_CANCELLED, bitCntAddress)` = `115` | [PrivateNote.sol:847](../../contracts/dex/PrivateNote.sol:847) |
| `FullSetStakeConfirmed` | `stakeController`, `amount` | In `onSplitAccepted()` after split amounts are credited to `stake.amount[]` and any unused collateral is refunded. | `address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress)` = `138` | [PrivateNote.sol:947](../../contracts/dex/PrivateNote.sol:947) |
| `FullSetStakeCancelled` | `stakeController`, `value` | In `onMergeAccepted()` after merged outcome tokens are debited and the collateral is returned to `_balance[tokenType]`. | `address.makeAddrExtern(PRIVATENOTE_MERGE_CONFIRMED, bitCntAddress)` = `139` | [PrivateNote.sol:1060](../../contracts/dex/PrivateNote.sol:1060) |
| `StakeConfirmed` | `stakeController`, `outcome`, `amount`, `betType` | In `onStakeAccepted()` after `candidateAmount` is moved into the confirmed arrays (`amount`, `debtAmount`, `couponsAmount`). | `address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress)` = `113` | [PrivateNote.sol:1172](../../contracts/dex/PrivateNote.sol:1172) |
| `ClaimAccepted` | `stakeController`, `outcome`, `payout` | In `onClaimAccepted()` only when `outcome.hasValue() == true`; the unresolved branch just clears `_busy` and returns without emitting. | `address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress)` = `114` | [PrivateNote.sol:1246](../../contracts/dex/PrivateNote.sol:1246) |
| `TransferInitiated` | `dest`, `tokenType`, `amount` | In `initTransfer()` after the amount is debited from `_balance`, `_pendingTransferAmount` is recorded, and before the `offerTransfer()` call on the receiving `PrivateNote`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress)` = `149` | [PrivateNote.sol:1505](../../contracts/dex/PrivateNote.sol:1505) |
| `TransferReceived` | `from`, `tokenType`, `amount` | In `offerTransfer()` after `_balance[tokenType]` is credited on the receiving side and before `onTransferAccepted()`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress)` = `150` | [PrivateNote.sol:1528](../../contracts/dex/PrivateNote.sol:1528) |
| `OrderSubmitted` | `clientOrderId`, `outcomeId`, `isBuy`, `price`, `amount`, `flags`, `eventId`, `tokenType` | In `placeOrder()` immediately after parameter validation and before the order is dispatched to `OrderBook`. The event records the submission itself, not the on-book confirmation. | `address.makeAddrExtern(PRIVATENOTE_ORDER_SUBMITTED, bitCntAddress)` = `151` | [PrivateNote.sol:1681](../../contracts/dex/PrivateNote.sol:1681) |
| `OrderPlacedConfirmed` | `orderBook`, `orderId`, `clientOrderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount` | In `onOrderPlaced()` after the `OrderBook` is validated and the per-order `feeReserve` / `lock` are written into the local `PrivateNote` state. | `address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress)` = `147` | [PrivateNote.sol:1843](../../contracts/dex/PrivateNote.sol:1843) |
| `OrderCancelledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `isBuy`, `returnAmount` | In `onOrderCancelled()` after the remaining buy-lock is returned to `_balance`, or outcome tokens are returned to the stake. | `address.makeAddrExtern(PRIVATENOTE_ORDER_CANCELLED, bitCntAddress)` = `152` | [PrivateNote.sol:2125](../../contracts/dex/PrivateNote.sol:2125) |
| `OrderFilledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `filledAmount`, `clearingPrice`, `isBuy`, `feeAmount`, `isRebate`, `isFinal` | In `onOrderFilled()` after the local balances / stakes / fee reserves / locks are updated for the fill reported by `OrderBook`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress)` = `148` | [PrivateNote.sol:2294](../../contracts/dex/PrivateNote.sol:2294) |
| `OrderPlaceRejected` | `orderBook`, `eventId`, `clientOrderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `opNonce` | In `onOrderRejected()` after the buy-lock or sell-lock has been released. Carries the full original `PlaceParams` so off-chain monitors can attribute the rejection back to its `clientOrderId`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_REJECTED, bitCntAddress)` = `153` | [PrivateNote.sol:1938](../../contracts/dex/PrivateNote.sol:1938) |

## PMP

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `StakeAccepted` | `note`, `outcomeId`, `amount`, `betType` | In `acceptStake()` after pool accounting is updated and the `PrivateNote.onStakeAccepted(...)` callback is dispatched. | `address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress)` = `118` | [PMP.sol:639](../../contracts/dex/PMP.sol:639) |
| `ApprovedByOracle` | `oracleEventList`, `oraclePubkey` | In `approveEvent()` only on the final required oracle approval, when `_approvedOracleEvents == _numberOfOracleEvents`. | `address.makeAddrExtern(PMP_APPROVED_BY_ORACLE, bitCntAddress)` = `119` | [PMP.sol:474](../../contracts/dex/PMP.sol:474) |
| `Resolved` | `outcomeId` | In `resolve()` after the resolution coefficients are computed and after `CreatorFeeCollected` if a fee was charged. | `address.makeAddrExtern(PMP_RESOLVED, bitCntAddress)` = `120` | [PMP.sol:1186](../../contracts/dex/PMP.sol:1186) |
| `ClaimProcessed` | `note`, `payout`, `win` | In `claim()` only for an already-resolved market, after the `PrivateNote.onClaimAccepted(...)` callback — once in the debt-refund branch (payout = refunded debt) and once in the main payout branch. | `address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress)` = `121` | [PMP.sol:1264](../../contracts/dex/PMP.sol:1264), [PMP.sol:1382](../../contracts/dex/PMP.sol:1382) |
| `NetworkFeeBurned` | `amount` | Not emitted in the current implementation. | No `dst` | [PMP.sol:277](../../contracts/dex/PMP.sol:277) |
| `TimingsSet` | `stakeStart`, `stakeEnd`, `resultStart`, `resultEnd` | In `setTimings()` after `_stakeStart`, `_resultStart`, `_approved = true` are committed and an optional auto-freeze runs. | `address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress)` = `124` | [PMP.sol:561](../../contracts/dex/PMP.sol:561) |
| `NumOutcomesSet` | `numOutcomes` | Not emitted in the current implementation. `_numOutcomes` is derived from `outcomeNames` inside `approveEvent()`. | No `dst` | [PMP.sol:289](../../contracts/dex/PMP.sol:289) |
| `EventCancelled` | `-` | In `cancelEvent()` after `_isCancelled = true` is set. | `address.makeAddrExtern(PMP_EVENT_CANCELLED, bitCntAddress)` = `126` | [PMP.sol:579](../../contracts/dex/PMP.sol:579) |
| `PMPRejected` | `-` | In `rejectEvent()` when `OracleEventList` rejects the market or confirmation is no longer possible, after which the PMP enters rollback and `selfdestruct`. | `address.makeAddrExtern(PMP_REJECTED_BY_ORACLE, bitCntAddress)` = `132` | [PMP.sol:361](../../contracts/dex/PMP.sol:361) |
| `CreatorFeeCollected` | `fee` | In `resolve()` only when `_creatorFee > 0`, after the fee is sent to `PrivateNote(_deployer).acceptFee(...)`. | `address.makeAddrExtern(PMP_CREATOR_FEE_COLLECTED, bitCntAddress)` = `137` | [PMP.sol:1182](../../contracts/dex/PMP.sol:1182) |
| `PoolsFrozen` | `baseTotalPool` | In `_ensureFrozen()` after the freeze snapshot and after `OrderBook` is deployed. | `address.makeAddrExtern(PMP_POOLS_FROZEN, bitCntAddress)` = `140` | [PMP.sol:885](../../contracts/dex/PMP.sol:885) |
| `SplitProcessed` | `note`, `collateral` | In `splitFullSet()` after the `PrivateNote.onSplitAccepted(...)` callback. The event carries `F_use` — the quantized collateral actually consumed. | `address.makeAddrExtern(PMP_SPLIT_PROCESSED, bitCntAddress)` = `141` | [PMP.sol:955](../../contracts/dex/PMP.sol:955) |
| `MergeProcessed` | `note`, `collateral` | In `mergeFullSet()` after the `PrivateNote.onMergeAccepted(...)` callback. | `address.makeAddrExtern(PMP_MERGE_PROCESSED, bitCntAddress)` = `142` | [PMP.sol:1049](../../contracts/dex/PMP.sol:1049) |

## OrderBook

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OrderPlaced` | `orderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `clientOrderId`, `depositHash`, `opNonce` | In `_emitOrderPlacedTo()` on successful order placement, before the `PrivateNote.onOrderPlaced(...)` callback. | `address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress)` = `143` | [OrderBook.sol:1230](../../contracts/dex/OrderBook.sol:1230) |
| `OrderCancelled` | `orderId`, `clientOrderId` | In the shared helper `_emitOrderCancelled()`, invoked from regular cancel, cancel-all, and internal no-book-entry / shutdown-cancel paths. | `address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress)` = `144` | [OrderBook.sol:1238](../../contracts/dex/OrderBook.sol:1238) |
| `OrderFilled` | `orderId`, `filledAmount`, `clearingPrice`, `feeAmount`, `isTaker`, `matchId`, `depositHash` | In `_processFillTo()` on every match/fill, before the `PrivateNote.onOrderFilled(...)` callback. | `address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress)` = `146` | [OrderBook.sol:1338](../../contracts/dex/OrderBook.sol:1338) |
| `PartialFill` | `orderId`, `clientOrderId`, `filledAmount`, `remainingAmount` | In `_emitPartialFill()` once per processed taker order when matching leaves a remainder. | `address.makeAddrExtern(OB_PARTIAL_FILL, bitCntAddress)` = `157` | [OrderBook.sol:1271](../../contracts/dex/OrderBook.sol:1271) |
| `FullyFilled` | `orderId`, `clientOrderId`, `filledAmount` | In `_emitFullyFilled()` once per processed taker order when it is fully filled. | `address.makeAddrExtern(OB_FULLY_FILLED, bitCntAddress)` = `158` | [OrderBook.sol:1276](../../contracts/dex/OrderBook.sol:1276) |
| `Queued` | `slot`, `queueId`, `entryType` | After a `QueueEntry` is successfully enqueued in `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(OB_QUEUED, bitCntAddress)` = `159` | [OrderBook.sol:526](../../contracts/dex/OrderBook.sol:526), [OrderBook.sol:562](../../contracts/dex/OrderBook.sol:562), [OrderBook.sol:594](../../contracts/dex/OrderBook.sol:594) |
| `Rejected` | `entryType`, `depositHash` | On an immediate reject of a place request during pre-validation in `executeBatch()`, and on queue overflow inside `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(OB_REJECTED, bitCntAddress)` = `160` | [OrderBook.sol:415](../../contracts/dex/OrderBook.sol:415), [OrderBook.sol:500](../../contracts/dex/OrderBook.sol:500), [OrderBook.sol:537](../../contracts/dex/OrderBook.sol:537), [OrderBook.sol:569](../../contracts/dex/OrderBook.sol:569) |
| `CallbackBounced` | `dest`, `lt` | In `onBounce()` whenever any outgoing `OrderBook -> PrivateNote` callback bounces back. This is an observability hook; OrderBook state is not automatically reverted. | `address.makeAddrExtern(OB_CALLBACK_BOUNCED, bitCntAddress)` = `161` | [OrderBook.sol:1501](../../contracts/dex/OrderBook.sol:1501) |

`PartialFill` / `FullyFilled` are derived aggregates that the contract emits for MM-friendly UX; the underlying state is already captured by `OrderFilled`. `Queued` / `Rejected` occur at the queue level, before any order ID is assigned. `CallbackBounced` is a diagnostic event — the OrderBook state is not automatically rolled back, and the bounced credit requires operator-driven recovery.

## Nullifier

| Contract | Event declarations | Comment |
| --- | --- | --- |
| `Nullifier` | None | `contracts/dex/Nullifier.sol` has no `event` declarations of its own. |
