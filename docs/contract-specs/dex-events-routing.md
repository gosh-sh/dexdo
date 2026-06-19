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
| `VoucherGenerated` | `skUCommit`, `voucherNominal`, `tokenType` | In `generateVoucher()` after the nominal check and a possible `SHELL -> SHELL_FEE` remap. | `address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress)` = `135` | [RootPN.sol:427](../../contracts/dex/RootPN.sol:427) |
| `PrivateNoteDeployed` | `depositIdentifierHash`, `noteAddress`, `initialBalance` | In the `privateNoteDeployed()` callback after `_deployedValues[tokenType]` is incremented. | `address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress)` = `101` | [RootPN.sol:339](../../contracts/dex/RootPN.sol:339) |
| `NullifierDeployed` | `nullifierAddress`, `value` | In `sendEccShellToPrivateNote()` after the zk check succeeds and the `Nullifier` is deployed. | `address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress)` = `102` | [RootPN.sol:247](../../contracts/dex/RootPN.sol:247) |
| `TokensWithdrawn` | `amount`, `noteAddress`, `to`, `dapp_id` | In `withdrawTokens()` after the collateral is transferred to the destination wallet. `noteAddress` is the withdrawing PrivateNote (`msg.sender`); `to` is the destination wallet. | `address.makeAddrExtern(ROOTPN_TOKENS_WITHDRAWN, bitCntAddress)` = `154` | [RootPN.sol:481](../../contracts/dex/RootPN.sol:481) |
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
| `EventAdded` | `eventId`, `eventName`, `oracleFee`, `deadline` | In `addEvent()` after a new `EventInfo` is written to `_events[eventId]`. | `address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress)` = `133` | [OracleEventList.sol:129](../../contracts/dex/OracleEventList.sol:129) |
| `EventConfirmed` | `eventId`, `pmpAddress` | In `confirmEvent()` after `deadline` and `oracleFee` are checked, `eventInfo.count` is incremented, and `PMP.approveEvent(...)` is called. | `address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress)` = `106` | [OracleEventList.sol:153](../../contracts/dex/OracleEventList.sol:153) |
| `DescriptionUpdated` | `description` | In `setDescription()` after `_description` is replaced. | `address.makeAddrExtern(ORACLE_LIST_DESCRIPTION_UPDATED, bitCntAddress)` = `107` | [OracleEventList.sol:94](../../contracts/dex/OracleEventList.sol:94) |

## PrivateNote

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OwnerChanged` | `oldPubkey`, `newPubkey` | In `changeOwner()` after `_ephemeralPubkey` is replaced. | `address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress)` = `112` | [PrivateNote.sol:333](../../contracts/dex/PrivateNote.sol:333) |
| `PMPDeployed` | `eventId`, `tokenType`, `pmpAddress`, `oracleEventLists`, `oracleFee` | In `deployPMP()` after the PMP address is computed, `_stakes[hash]` is prepared, and `_busy` is set, immediately before `new PMP`. | `address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress)` = `111` | [PrivateNote.sol:447](../../contracts/dex/PrivateNote.sol:447) |
| `StakeCancelled` | `stakeController`, `value` | In `onStakeCancelled()` after the stake record is removed and the funds are returned to `_balance` / `_couponsValue`. | `address.makeAddrExtern(PRIVATENOTE_STAKE_CANCELLED, bitCntAddress)` = `115` | [PrivateNote.sol:640](../../contracts/dex/PrivateNote.sol:640) |
| `FullSetStakeConfirmed` | `stakeController`, `amount` | In `onSplitAccepted()` after split amounts are credited to `stake.amount[]` and any unused collateral is refunded. | `address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress)` = `138` | [PrivateNote.sol:738](../../contracts/dex/PrivateNote.sol:738) |
| `FullSetStakeCancelled` | `stakeController`, `value` | In `onMergeAccepted()` after merged outcome tokens are debited and the collateral is returned to `_balance[tokenType]`. | `address.makeAddrExtern(PRIVATENOTE_MERGE_CONFIRMED, bitCntAddress)` = `139` | [PrivateNote.sol:849](../../contracts/dex/PrivateNote.sol:849) |
| `StakeConfirmed` | `stakeController`, `outcome`, `amount`, `betType` | In `onStakeAccepted()` after `candidateAmount` is moved into the confirmed arrays (`amount`, `debtAmount`, `couponsAmount`). | `address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress)` = `113` | [PrivateNote.sol:961](../../contracts/dex/PrivateNote.sol:961) |
| `ClaimAccepted` | `stakeController`, `outcome`, `payout` | In `onClaimAccepted()` only when `outcome.hasValue() == true`; the unresolved branch just clears `_busy` and returns without emitting. | `address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress)` = `114` | [PrivateNote.sol:1035](../../contracts/dex/PrivateNote.sol:1035) |
| `TransferInitiated` | `dest`, `tokenType`, `amount` | In `initTransfer()` after the amount is debited from `_balance`, `_pendingTransferAmount` is recorded, and before the `offerTransfer()` call on the receiving `PrivateNote`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress)` = `149` | [PrivateNote.sol:1289](../../contracts/dex/PrivateNote.sol:1289) |
| `TransferReceived` | `from`, `tokenType`, `amount` | In `offerTransfer()` after `_balance[tokenType]` is credited on the receiving side and before `onTransferAccepted()`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress)` = `150` | [PrivateNote.sol:1312](../../contracts/dex/PrivateNote.sol:1312) |
| `OrderSubmitted` | `clientOrderId`, `outcomeId`, `isBuy`, `price`, `amount`, `flags`, `eventId`, `tokenType` | In `placeOrder()` immediately after parameter validation and before the order is dispatched to `OrderBook`. The event records the submission itself, not the on-book confirmation. | `address.makeAddrExtern(PRIVATENOTE_ORDER_SUBMITTED, bitCntAddress)` = `151` | [PrivateNote.sol:1460](../../contracts/dex/PrivateNote.sol:1460) |
| `OrderPlacedConfirmed` | `orderBook`, `orderId` | In `onOrderPlaced()` after the `OrderBook` is validated and the per-order `feeReserve` / `lock` are written into the local `PrivateNote` state. | `address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress)` = `147` | [PrivateNote.sol:1622](../../contracts/dex/PrivateNote.sol:1622) |
| `OrderCancelledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `isBuy`, `returnAmount` | In `onOrderCancelled()` after the remaining buy-lock is returned to `_balance`, or outcome tokens are returned to the stake. | `address.makeAddrExtern(PRIVATENOTE_ORDER_CANCELLED, bitCntAddress)` = `152` | [PrivateNote.sol:1904](../../contracts/dex/PrivateNote.sol:1904) |
| `OrderFilledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `filledAmount`, `clearingPrice`, `isBuy`, `feeAmount`, `isFinal` | In `onOrderFilled()` after the local balances / stakes / fee reserves / locks are updated for the fill reported by `OrderBook`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress)` = `148` | [PrivateNote.sol:2073](../../contracts/dex/PrivateNote.sol:2073) |
| `OrderPlaceRejected` | `orderBook`, `eventId`, `clientOrderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `opNonce` | In `onOrderRejected()` after the buy-lock or sell-lock has been released. Carries the full original `PlaceParams` so off-chain monitors can attribute the rejection back to its `clientOrderId`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_REJECTED, bitCntAddress)` = `153` | [PrivateNote.sol:1717](../../contracts/dex/PrivateNote.sol:1717) |

## PMP

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `StakeAccepted` | `note`, `outcomeId`, `amount`, `betType` | In `acceptStake()` after pool accounting is updated and the `PrivateNote.onStakeAccepted(...)` callback is dispatched. | `address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress)` = `118` | [PMP.sol:630](../../contracts/dex/PMP.sol:630) |
| `ApprovedByOracle` | `oracleEventList`, `oraclePubkey` | In `approveEvent()` only on the final required oracle approval, when `_approvedOracleEvents == _numberOfOracleEvents`. | `address.makeAddrExtern(PMP_APPROVED_BY_ORACLE, bitCntAddress)` = `119` | [PMP.sol:465](../../contracts/dex/PMP.sol:465) |
| `Resolved` | `outcomeId` | In `resolve()` after the resolution coefficients are computed and after `CreatorFeeCollected` if a fee was charged. | `address.makeAddrExtern(PMP_RESOLVED, bitCntAddress)` = `120` | [PMP.sol:1169](../../contracts/dex/PMP.sol:1169) |
| `ClaimProcessed` | `note`, `payout`, `win` | In `claim()` only for an already-resolved market, after the `PrivateNote.onClaimAccepted(...)` callback — once in the debt-refund branch (payout = refunded debt) and once in the main payout branch. | `address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress)` = `121` | [PMP.sol:1247](../../contracts/dex/PMP.sol:1247), [PMP.sol:1371](../../contracts/dex/PMP.sol:1371) |
| `NetworkFeeBurned` | `amount` | Not emitted in the current implementation. | No `dst` | [PMP.sol:268](../../contracts/dex/PMP.sol:268) |
| `TimingsSet` | `stakeStart`, `stakeEnd`, `resultStart`, `resultEnd` | In `setTimings()` after `_stakeStart`, `_resultStart`, `_approved = true` are committed and an optional auto-freeze runs. | `address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress)` = `124` | [PMP.sol:552](../../contracts/dex/PMP.sol:552) |
| `NumOutcomesSet` | `numOutcomes` | Not emitted in the current implementation. `_numOutcomes` is derived from `outcomeNames` inside `approveEvent()`. | No `dst` | [PMP.sol:280](../../contracts/dex/PMP.sol:280) |
| `EventCancelled` | `-` | In `cancelEvent()` after `_isCancelled = true` is set. | `address.makeAddrExtern(PMP_EVENT_CANCELLED, bitCntAddress)` = `126` | [PMP.sol:570](../../contracts/dex/PMP.sol:570) |
| `PMPRejected` | `-` | In `rejectEvent()` when `OracleEventList` rejects the market or confirmation is no longer possible, after which the PMP enters rollback and `selfdestruct`. | `address.makeAddrExtern(PMP_REJECTED_BY_ORACLE, bitCntAddress)` = `132` | [PMP.sol:352](../../contracts/dex/PMP.sol:352) |
| `CreatorFeeCollected` | `fee` | In `resolve()` only when `_creatorFee > 0`, after the fee is sent to `PrivateNote(_deployer).acceptFee(...)`. | `address.makeAddrExtern(PMP_CREATOR_FEE_COLLECTED, bitCntAddress)` = `137` | [PMP.sol:1165](../../contracts/dex/PMP.sol:1165) |
| `PoolsFrozen` | `baseTotalPool` | In `_ensureFrozen()` after the freeze snapshot and after `OrderBook` is deployed. | `address.makeAddrExtern(PMP_POOLS_FROZEN, bitCntAddress)` = `140` | [PMP.sol:868](../../contracts/dex/PMP.sol:868) |
| `SplitProcessed` | `note`, `collateral` | In `splitFullSet()` after the `PrivateNote.onSplitAccepted(...)` callback. The event carries `F_use` — the quantized collateral actually consumed. | `address.makeAddrExtern(PMP_SPLIT_PROCESSED, bitCntAddress)` = `141` | [PMP.sol:938](../../contracts/dex/PMP.sol:938) |
| `MergeProcessed` | `note`, `collateral` | In `mergeFullSet()` after the `PrivateNote.onMergeAccepted(...)` callback. | `address.makeAddrExtern(PMP_MERGE_PROCESSED, bitCntAddress)` = `142` | [PMP.sol:1032](../../contracts/dex/PMP.sol:1032) |

## OrderBook

| Event | Fields | Emitted | Destination (`dst`) | Source |
| --- | --- | --- | --- | --- |
| `OrderPlaced` | `orderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `clientOrderId` | In `_emitOrderPlacedTo()` on successful order placement, before the `PrivateNote.onOrderPlaced(...)` callback. | `address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress)` = `143` | [OrderBook.sol:1222](../../contracts/dex/OrderBook.sol:1222) |
| `OrderCancelled` | `orderId`, `clientOrderId` | In the shared helper `_emitOrderCancelled()`, invoked from regular cancel, cancel-all, and internal no-book-entry / shutdown-cancel paths. | `address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress)` = `144` | [OrderBook.sol:1230](../../contracts/dex/OrderBook.sol:1230) |
| `OrderFilled` | `orderId`, `filledAmount`, `clearingPrice`, `feeAmount`, `isTaker` | In `_processFillTo()` on every match/fill, before the `PrivateNote.onOrderFilled(...)` callback. | `address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress)` = `146` | [OrderBook.sol:1327](../../contracts/dex/OrderBook.sol:1327) |
| `PartialFill` | `orderId`, `clientOrderId`, `filledAmount`, `remainingAmount` | In `_emitPartialFill()` once per processed taker order when matching leaves a remainder. | `address.makeAddrExtern(OB_PARTIAL_FILL, bitCntAddress)` = `157` | [OrderBook.sol:1271](../../contracts/dex/OrderBook.sol:1271) |
| `FullyFilled` | `orderId`, `clientOrderId`, `filledAmount` | In `_emitFullyFilled()` once per processed taker order when it is fully filled. | `address.makeAddrExtern(OB_FULLY_FILLED, bitCntAddress)` = `158` | [OrderBook.sol:1276](../../contracts/dex/OrderBook.sol:1276) |
| `Queued` | `slot`, `queueId`, `entryType` | After a `QueueEntry` is successfully enqueued in `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(OB_QUEUED, bitCntAddress)` = `159` | [OrderBook.sol:526](../../contracts/dex/OrderBook.sol:526), [OrderBook.sol:562](../../contracts/dex/OrderBook.sol:562), [OrderBook.sol:594](../../contracts/dex/OrderBook.sol:594) |
| `Rejected` | `entryType`, `depositHash` | On an immediate reject of a place request during pre-validation in `executeBatch()`, and on queue overflow inside `_enqueuePlace()`, `_enqueueCancel()`, and `_enqueueCancelAll()`. | `address.makeAddrExtern(OB_REJECTED, bitCntAddress)` = `160` | [OrderBook.sol:415](../../contracts/dex/OrderBook.sol:415), [OrderBook.sol:500](../../contracts/dex/OrderBook.sol:500), [OrderBook.sol:537](../../contracts/dex/OrderBook.sol:537), [OrderBook.sol:569](../../contracts/dex/OrderBook.sol:569) |
| `CallbackBounced` | `dest`, `lt` | In `onBounce()` whenever any outgoing `OrderBook -> PrivateNote` callback bounces back. This is an observability hook; OrderBook state is not automatically reverted. | `address.makeAddrExtern(OB_CALLBACK_BOUNCED, bitCntAddress)` = `161` | [OrderBook.sol:1501](../../contracts/dex/OrderBook.sol:1501) |

## Nullifier

| Contract | Event declarations | Comment |
| --- | --- | --- |
| `Nullifier` | None | `contracts/dex/Nullifier.sol` has no `event` declarations of its own. |
