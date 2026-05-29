# DEX.DO Event Routing

This document catalogs every `event` defined under `contracts`: when it is emitted, which `dst` address it is routed to, and which events are currently declared only and not actually emitted.

## General principle

`dst` only exists on external event objects created via `emit Event{dest: ...}(...)`.

This codebase uses three modes:

| Case | Routed to |
| --- | --- |
| Regular external events | `address.makeAddrExtern(EVENT_ID, bitCntAddress)` |
| Special `OrderBook` channels | `address.makeAddrExtern(0, bitCntAddress)` |
| Event only declared | no `dst`, because there is no `emit` |

## RootPN

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `VoucherGenerated` | `skUCommit`, `voucherNominal`, `tokenType` | In `generateVoucher()` after the nominal check and a possible `SHELL -> SHELL_FEE` remap. | `address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress)` = `135` | [RootPN.sol:427](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:427) |
| `PrivateNoteDeployed` | `depositIdentifierHash`, `noteAddress`, `initialBalance` | In the `privateNoteDeployed()` callback after `_deployedValues[tokenType]` is incremented. | `address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress)` = `101` | [RootPN.sol:339](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:339) |
| `NullifierDeployed` | `nullifierAddress`, `value` | In `sendEccShellToPrivateNote()` after the zk check succeeds and the `Nullifier` is deployed. | `address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress)` = `102` | [RootPN.sol:247](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:247) |

## RootOracle

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OracleDeployed` | `oracle`, `pubkey`, `name` | In `deployOracle()` immediately after a new `Oracle` is deployed. | `address.makeAddrExtern(ROOTORACLE_ORACLE_DEPLOYED, bitCntAddress)` = `136` | [RootOracle.sol:65](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootOracle.sol:65) |

## Oracle

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OracleEventListDeployed` | `eventListAddress`, `index` | In the `Oracle` constructor for the default list with `index = 0`, and in `deployEventList()` for additional lists. | `address.makeAddrExtern(ORACLE_DEPLOYED, bitCntAddress)` = `104` | [Oracle.sol:70](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:70), [Oracle.sol:86](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:86) |
| `EventPublished` | `eventId`, `eventName` | Not emitted in the current implementation. | No `dst` | [Oracle.sol:36](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:36) |

## OracleEventList

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `EventAdded` | `eventId`, `eventName`, `oracleFee`, `deadline` | In `addEvent()` after a new `EventInfo` is written to `_events[eventId]`. | `address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress)` = `133` | [OracleEventList.sol:105](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:105) |
| `EventConfirmed` | `eventId`, `pmpAddress` | In `confirmEvent()` after `deadline` and `oracleFee` are checked, `eventInfo.count` is incremented, and `PMP.approveEvent(...)` is called. | `address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress)` = `106` | [OracleEventList.sol:129](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:129) |

## PrivateNote

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OwnerChanged` | `oldPubkey`, `newPubkey` | In `changeOwner()` after `_ephemeralPubkey` is replaced. | `address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress)` = `112` | [PrivateNote.sol:243](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:243) |
| `PMPDeployed` | `eventId`, `tokenType`, `pmpAddress`, `oracleEventLists`, `oracleFee` | In `deployPMP()` after the PMP address is computed, `_stakes[hash]` is prepared, and `_busy` is set, immediately before `new PMP`. | `address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress)` = `111` | [PrivateNote.sol:346](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:346) |
| `StakeCancelled` | `stakeController`, `value` | In `onStakeCancelled()` after the stake record is removed and the funds are returned to `_balance` / `_couponsValue`. | `address.makeAddrExtern(PRIVATENOTE_STAKE_CANCELLED, bitCntAddress)` = `115` | [PrivateNote.sol:476](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:476) |
| `FullSetStakeConfirmed` | `stakeController`, `amount` | In `onSplitAccepted()` after split amounts are credited to `stake.amount[]` and any unused collateral is refunded. | `address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress)` = `138` | [PrivateNote.sol:570](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:570) |
| `FullSetStakeCancelled` | `stakeController`, `value` | In `onMergeAccepted()` after merged outcome tokens are debited and the collateral is returned to `_balance[tokenType]`. | `address.makeAddrExtern(PRIVATENOTE_MERGE_CONFIRMED, bitCntAddress)` = `139` | [PrivateNote.sol:669](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:669) |
| `StakeConfirmed` | `stakeController`, `outcome`, `amount`, `betType` | In `onStakeAccepted()` after `candidateAmount` is moved into the confirmed arrays (`amount`, `debtAmount`, `couponsAmount`). | `address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress)` = `113` | [PrivateNote.sol:781](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:781) |
| `ClaimAccepted` | `stakeController`, `outcome`, `payout` | In `onClaimAccepted()` only when `outcome.hasValue() == true`; the unresolved branch just clears `_busy` and returns without emitting. | `address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress)` = `114` | [PrivateNote.sol:846](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:846) |
| `TransferInitiated` | `dest`, `tokenType`, `amount` | In `initTransfer()` after the amount is debited from `_balance`, `_pendingTransferAmount` is recorded, and before the `offerTransfer()` call on the receiving `PrivateNote`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress)` = `149` | [PrivateNote.sol:1052](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1052) |
| `TransferReceived` | `from`, `tokenType`, `amount` | In `offerTransfer()` after `_balance[tokenType]` is credited on the receiving side and before `onTransferAccepted()`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress)` = `150` | [PrivateNote.sol:1075](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1075) |
| `OrderSubmitted` | `clientOrderId`, `outcomeId`, `isBuy`, `price`, `amount`, `flags`, `eventId`, `tokenType` | In `placeOrder()` immediately after parameter validation and before the order is dispatched to `OrderBook`. The event records the submission itself, not the on-book confirmation. | `address.makeAddrExtern(PRIVATENOTE_ORDER_SUBMITTED, bitCntAddress)` = `151` | [PrivateNote.sol:1201](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1201) |
| `OrderPlacedConfirmed` | `orderBook`, `orderId` | In `onOrderPlaced()` after the `OrderBook` is validated and the per-order `feeReserve` / `lock` are written into the local `PrivateNote` state. | `address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress)` = `147` | [PrivateNote.sol:1322](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1322) |
| `OrderCancelledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `isBuy`, `returnAmount` | In `onOrderCancelled()` after the remaining buy-lock is returned to `_balance`, or outcome tokens are returned to the stake. | `address.makeAddrExtern(PRIVATENOTE_ORDER_CANCELLED, bitCntAddress)` = `152` | [PrivateNote.sol:1528](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1528) |
| `OrderFilledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `filledAmount`, `clearingPrice`, `isBuy`, `feeAmount`, `isFinal` | In `onOrderFilled()` after the local balances / stakes / fee reserves / locks are updated for the fill reported by `OrderBook`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress)` = `148` | [PrivateNote.sol:1654](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1654) |
| `OrderPlaceRejected` | `orderBook`, `eventId`, `clientOrderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `opNonce` | In `onOrderRejected()` after the buy-lock or sell-lock has been released. Carries the full original `PlaceParams` so off-chain monitors can attribute the rejection back to its `clientOrderId`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_REJECTED, bitCntAddress)` = `153` | [PrivateNote.sol:1659](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1659) |

## PMP

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `StakeAccepted` | `note`, `outcomeId`, `amount`, `betType` | In `acceptStake()` after pool accounting is updated and the `PrivateNote.onStakeAccepted(...)` callback is dispatched. | `address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress)` = `118` | [PMP.sol:518](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:518) |
| `ApprovedByOracle` | `oracleEventList`, `oraclePubkey` | In `approveEvent()` only on the final required oracle approval, when `_approvedOracleEvents == _numberOfOracleEvents`. | `address.makeAddrExtern(PMP_APPROVED_BY_ORACLE, bitCntAddress)` = `119` | [PMP.sol:389](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:389) |
| `Resolved` | `outcomeId` | In `resolve()` after the resolution coefficients are computed and after `CreatorFeeCollected` if a fee was charged. | `address.makeAddrExtern(PMP_RESOLVED, bitCntAddress)` = `120` | [PMP.sol:971](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:971) |
| `ClaimProcessed` | `note`, `payout`, `win` | In `claim()` only for an already-resolved market, after the `PrivateNote.onClaimAccepted(...)` callback. | `address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress)` = `121` | [PMP.sol:1096](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:1096) |
| `NetworkFeeBurned` | `amount` | Not emitted in the current implementation. | No `dst` | [PMP.sol:204](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:204) |
| `TimingsSet` | `stakeStart`, `stakeEnd`, `resultStart`, `resultEnd` | In `setTimings()` after `_stakeStart`, `_resultStart`, `_approved = true` are committed and an optional auto-freeze runs. | `address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress)` = `124` | [PMP.sol:445](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:445) |
| `NumOutcomesSet` | `numOutcomes` | Not emitted in the current implementation. `_numOutcomes` is derived from `outcomeNames` inside `approveEvent()`. | No `dst` | [PMP.sol:216](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:216) |
| `EventCancelled` | `-` | In `cancelEvent()` after `_isCancelled = true` is set. | `address.makeAddrExtern(PMP_EVENT_CANCELLED, bitCntAddress)` = `126` | [PMP.sol:460](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:460) |
| `PMPRejected` | `-` | In `rejectEvent()` when `OracleEventList` rejects the market or confirmation is no longer possible, after which the PMP enters rollback and `selfdestruct`. | `address.makeAddrExtern(PMP_REJECTED_BY_ORACLE, bitCntAddress)` = `132` | [PMP.sol:352](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:352) |
| `CreatorFeeCollected` | `fee` | In `resolve()` only when `_creatorFee > 0`, after the fee is sent to `PrivateNote(_deployer).acceptFee(...)`. | `address.makeAddrExtern(PMP_CREATOR_FEE_COLLECTED, bitCntAddress)` = `137` | [PMP.sol:967](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:967) |
| `PoolsFrozen` | `baseTotalPool` | In `_ensureFrozen()` after the freeze snapshot and after `OrderBook` is deployed. | `address.makeAddrExtern(PMP_POOLS_FROZEN, bitCntAddress)` = `140` | [PMP.sol:712](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:712) |
| `SplitProcessed` | `note`, `collateral` | In `splitFullSet()` after the `PrivateNote.onSplitAccepted(...)` callback. The event carries `F_use` — the quantized collateral actually consumed. | `address.makeAddrExtern(PMP_SPLIT_PROCESSED, bitCntAddress)` = `141` | [PMP.sol:777](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:777) |
| `MergeProcessed` | `note`, `collateral` | In `mergeFullSet()` after the `PrivateNote.onMergeAccepted(...)` callback. | `address.makeAddrExtern(PMP_MERGE_PROCESSED, bitCntAddress)` = `142` | [PMP.sol:862](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:862) |

## OrderBook

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OrderPlaced` | `orderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `clientOrderId` | In `_emitOrderPlacedTo()` on successful order placement, before the `PrivateNote.onOrderPlaced(...)` callback. | `address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress)` = `143` | [OrderBook.sol:1134](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1134) |
| `OrderCancelled` | `orderId`, `clientOrderId` | In the shared helper `_emitOrderCancelled()`, invoked from regular cancel, cancel-all, and internal no-book-entry / shutdown-cancel paths. | `address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress)` = `144` | [OrderBook.sol:1142](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1142) |
| `OrderFilled` | `orderId`, `filledAmount`, `clearingPrice`, `feeAmount`, `isTaker` | In `_processFillTo()` on every match/fill, before the `PrivateNote.onOrderFilled(...)` callback. | `address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress)` = `146` | [OrderBook.sol:1225](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1225) |
| `PartialFill` | `orderId`, `clientOrderId`, `filledAmount`, `remainingAmount` | In `_emitPartialFill()` once per processed taker order when matching leaves a remainder. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:1177](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1177) |
| `FullyFilled` | `orderId`, `clientOrderId`, `filledAmount` | In `_emitFullyFilled()` once per processed taker order when it is fully filled. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:1182](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1182) |
| `Queued` | `slot`, `queueId`, `entryType` | After a `QueueEntry` is successfully enqueued in `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:481](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:481), [OrderBook.sol:515](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:515), [OrderBook.sol:546](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:546) |
| `Rejected` | `entryType`, `depositHash` | On an immediate reject of a place request during pre-validation in `executeBatch()`, and on queue overflow inside `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:362](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:362), [OrderBook.sol:456](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:456), [OrderBook.sol:491](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:491), [OrderBook.sol:522](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:522) |
| `CallbackBounced` | `dest`, `lt` | In `onBounce()` whenever any outgoing `OrderBook -> PrivateNote` callback bounces back. This is an observability hook; OrderBook state is not automatically reverted. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:1367](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1367) |

## Nullifier

| Contract | Event declarations | Comment |
| --- | --- | --- |
| `Nullifier` | None | `contracts/Nullifier.sol` has no `event` declarations of its own. |
