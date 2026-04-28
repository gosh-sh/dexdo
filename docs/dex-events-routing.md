# DODEX Event Routing

Документ фиксирует все `event` из `contracts`: когда они отправляются, кому уходит `dst`, и какие события сейчас только объявлены, но не эмитятся.

## Общий принцип

`dst` существует только у внешнего event-object, который создается через `emit Event{dest: ...}(...)`.

В этой кодовой базе есть три режима:

| Случай | Кому уходит |
| --- | --- |
| Обычные внешние события | `address.makeAddrExtern(EVENT_ID, bitCntAddress)` |
| Специальные каналы `OrderBook` | `address.makeAddrExtern(0, bitCntAddress)` |
| Событие только объявлено | `dst` отсутствует, потому что `emit` нет |

## RootPN

| Event | Fields | Когда отправляется | Кому (`dst`) | Код |
| --- | --- | --- | --- | --- |
| `VoucherGenerated` | `skUCommit`, `voucherNominal`, `tokenType` | В `generateVoucher()` после проверки номинала и возможного ремапа `SHELL -> SHELL_FEE`. | `address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress)` = `135` | [RootPN.sol:427](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:427) |
| `PrivateNoteDeployed` | `depositIdentifierHash`, `noteAddress`, `initialBalance` | В callback `privateNoteDeployed()` после увеличения `_deployedValues[tokenType]`. | `address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress)` = `101` | [RootPN.sol:339](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:339) |
| `NullifierDeployed` | `nullifierAddress`, `value` | В `sendEccShellToPrivateNote()` после успешной zk-проверки и деплоя `Nullifier`. | `address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress)` = `102` | [RootPN.sol:247](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:247) |

## RootOracle

| Event | Fields | Когда отправляется | Кому (`dst`) | Код |
| --- | --- | --- | --- | --- |
| `OracleDeployed` | `oracle`, `pubkey`, `name` | В `deployOracle()` сразу после деплоя нового `Oracle`. | `address.makeAddrExtern(ROOTORACLE_ORACLE_DEPLOYED, bitCntAddress)` = `136` | [RootOracle.sol:65](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootOracle.sol:65) |

## Oracle

| Event | Fields | Когда отправляется | Кому (`dst`) | Код |
| --- | --- | --- | --- | --- |
| `OracleEventListDeployed` | `eventListAddress`, `index` | В конструкторе `Oracle` для дефолтного списка с `index = 0`, и в `deployEventList()` для дополнительных списков. | `address.makeAddrExtern(ORACLE_DEPLOYED, bitCntAddress)` = `104` | [Oracle.sol:70](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:70), [Oracle.sol:86](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:86) |
| `EventPublished` | `eventId`, `eventName` | В текущей реализации не эмитится. | Нет `dst` | [Oracle.sol:36](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:36) |

## OracleEventList

| Event | Fields | Когда отправляется | Кому (`dst`) | Код |
| --- | --- | --- | --- | --- |
| `EventAdded` | `eventId`, `eventName`, `oracleFee`, `deadline` | В `addEvent()` после записи нового `EventInfo` в `_events[eventId]`. | `address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress)` = `133` | [OracleEventList.sol:105](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:105) |
| `EventConfirmed` | `eventId`, `pmpAddress` | В `confirmEvent()` после проверки `deadline` и `oracleFee`, увеличения `eventInfo.count` и вызова `PMP.approveEvent(...)`. | `address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress)` = `106` | [OracleEventList.sol:129](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:129) |

## PrivateNote

| Event | Fields | Когда отправляется | Кому (`dst`) | Код |
| --- | --- | --- | --- | --- |
| `OwnerChanged` | `oldPubkey`, `newPubkey` | В `changeOwner()` после замены `_ephemeralPubkey`. | `address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress)` = `112` | [PrivateNote.sol:243](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:243) |
| `PMPDeployed` | `eventId`, `tokenType`, `pmpAddress`, `oracleEventLists`, `oracleFee` | В `deployPMP()` после вычисления адреса PMP, подготовки `_stakes[hash]` и установки `_busy`, прямо перед `new PMP`. | `address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress)` = `111` | [PrivateNote.sol:346](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:346) |
| `StakeCancelled` | `stakeController`, `value` | В `onStakeCancelled()` после удаления stake-record и возврата средств в `_balance` / `_couponsValue`. | `address.makeAddrExtern(PRIVATENOTE_STAKE_CANCELLED, bitCntAddress)` = `115` | [PrivateNote.sol:476](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:476) |
| `FullSetStakeConfirmed` | `stakeController`, `amount` | В `onSplitAccepted()` после зачисления split-amounts в `stake.amount[]` и возможного возврата неиспользованного collateral. | `address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress)` = `138` | [PrivateNote.sol:570](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:570) |
| `FullSetStakeCancelled` | `stakeController`, `value` | В `onMergeAccepted()` после списания merged outcome-токенов и возврата collateral в `_balance[tokenType]`. | `address.makeAddrExtern(PRIVATENOTE_MERGE_CONFIRMED, bitCntAddress)` = `139` | [PrivateNote.sol:669](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:669) |
| `StakeConfirmed` | `stakeController`, `outcome`, `amount`, `betType` | В `onStakeAccepted()` после переноса `candidateAmount` в подтвержденные массивы (`amount`, `debtAmount`, `couponsAmount`). | `address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress)` = `113` | [PrivateNote.sol:781](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:781) |
| `ClaimAccepted` | `stakeController`, `outcome`, `payout` | В `onClaimAccepted()` только если `outcome.hasValue() == true`; при unresolved-ветке callback просто очищает `_busy` и выходит без события. | `address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress)` = `114` | [PrivateNote.sol:846](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:846) |
| `TransferInitiated` | `dest`, `tokenType`, `amount` | В `initTransfer()` после списания суммы из `_balance`, фиксации `_pendingTransferAmount` и перед вызовом `offerTransfer()` у другого `PrivateNote`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress)` = `149` | [PrivateNote.sol:1052](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1052) |
| `TransferReceived` | `from`, `tokenType`, `amount` | В `offerTransfer()` после кредитования `_balance[tokenType]` у получателя и перед `onTransferAccepted()`. | `address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress)` = `150` | [PrivateNote.sol:1075](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1075) |
| `OrderSubmitted` | `clientOrderId`, `outcomeId`, `isBuy`, `price`, `amount`, `flags`, `eventId`, `tokenType` | В `placeOrder()` сразу после валидации параметров и до отправки заявки в `OrderBook`. Это событие фиксирует сам факт отправки заявки, а не подтверждение постановки в книгу. | `address.makeAddrExtern(PRIVATENOTE_ORDER_SUBMITTED, bitCntAddress)` = `151` | [PrivateNote.sol:1201](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1201) |
| `OrderPlacedConfirmed` | `orderBook`, `orderId` | В `onOrderPlaced()` после валидации `OrderBook` и записи per-order `feeReserve` / `lock` в локальное состояние `PrivateNote`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress)` = `147` | [PrivateNote.sol:1322](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1322) |
| `OrderCancelledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `isBuy`, `returnAmount` | В `onOrderCancelled()` после возврата оставшегося buy-lock в `_balance` или возврата outcome-токенов в stake. | `address.makeAddrExtern(PRIVATENOTE_ORDER_CANCELLED, bitCntAddress)` = `152` | [PrivateNote.sol:1528](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1528) |
| `OrderFilledConfirmed` | `orderBook`, `orderId`, `outcomeId`, `filledAmount`, `clearingPrice`, `isBuy`, `feeAmount`, `isFinal` | В `onOrderFilled()` после обновления локальных balances / stakes / fee reserves / locks по факту fill от `OrderBook`. | `address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress)` = `148` | [PrivateNote.sol:1654](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1654) |

## PMP

| Event | Fields | Когда отправляется | Кому (`dst`) | Код |
| --- | --- | --- | --- | --- |
| `StakeAccepted` | `note`, `outcomeId`, `amount`, `betType` | В `acceptStake()` после обновления pool accounting и отправки callback `PrivateNote.onStakeAccepted(...)`. | `address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress)` = `118` | [PMP.sol:518](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:518) |
| `ApprovedByOracle` | `oracleEventList`, `oraclePubkey` | В `approveEvent()` только на последнем обязательном oracle approval, когда `_approvedOracleEvents == _numberOfOracleEvents`. | `address.makeAddrExtern(PMP_APPROVED_BY_ORACLE, bitCntAddress)` = `119` | [PMP.sol:389](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:389) |
| `Resolved` | `outcomeId` | В `resolve()` после вычисления resolution-коэффициентов и после `CreatorFeeCollected`, если fee был начислен. | `address.makeAddrExtern(PMP_RESOLVED, bitCntAddress)` = `120` | [PMP.sol:971](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:971) |
| `ClaimProcessed` | `note`, `payout`, `win` | В `claim()` только для уже resolved market, после callback `PrivateNote.onClaimAccepted(...)`. | `address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress)` = `121` | [PMP.sol:1096](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:1096) |
| `NetworkFeeBurned` | `amount` | В текущей реализации не эмитится. | Нет `dst` | [PMP.sol:204](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:204) |
| `TimingsSet` | `stakeStart`, `stakeEnd`, `resultStart`, `resultEnd` | В `setTimings()` после фиксации `_stakeStart`, `_resultStart`, `_approved = true` и возможного auto-freeze. | `address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress)` = `124` | [PMP.sol:445](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:445) |
| `NumOutcomesSet` | `numOutcomes` | В текущей реализации не эмитится. `_numOutcomes` выводится из `outcomeNames` в `approveEvent()`. | Нет `dst` | [PMP.sol:216](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:216) |
| `EventCancelled` | `-` | В `cancelEvent()` после установки `_isCancelled = true`. | `address.makeAddrExtern(PMP_EVENT_CANCELLED, bitCntAddress)` = `126` | [PMP.sol:460](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:460) |
| `PMPCancelled` | `-` | В `rejectEvent()` когда `OracleEventList` отклоняет маркет или подтверждение невозможно, после чего PMP начинает rollback и `selfdestruct`. | `address.makeAddrExtern(PMP_CANCELLED_BY_ORACLE, bitCntAddress)` = `132` | [PMP.sol:283](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:283) |
| `CreatorFeeCollected` | `fee` | В `resolve()` только если `_creatorFee > 0`, после отправки fee в `PrivateNote(_deployer).acceptFee(...)`. | `address.makeAddrExtern(PMP_CREATOR_FEE_COLLECTED, bitCntAddress)` = `137` | [PMP.sol:967](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:967) |
| `PoolsFrozen` | `baseTotalPool` | В `_ensureFrozen()` после freeze snapshot и после деплоя `OrderBook`. | `address.makeAddrExtern(PMP_POOLS_FROZEN, bitCntAddress)` = `140` | [PMP.sol:712](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:712) |
| `SplitProcessed` | `note`, `collateral` | В `splitFullSet()` после callback `PrivateNote.onSplitAccepted(...)`. В событие идет именно `F_use`, то есть реально использованный quantized collateral. | `address.makeAddrExtern(PMP_SPLIT_PROCESSED, bitCntAddress)` = `141` | [PMP.sol:777](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:777) |
| `MergeProcessed` | `note`, `collateral` | В `mergeFullSet()` после callback `PrivateNote.onMergeAccepted(...)`. | `address.makeAddrExtern(PMP_MERGE_PROCESSED, bitCntAddress)` = `142` | [PMP.sol:862](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:862) |

## OrderBook

| Event | Fields | Когда отправляется | Кому (`dst`) | Код |
| --- | --- | --- | --- | --- |
| `OrderPlaced` | `orderId`, `outcomeId`, `isBuy`, `flags`, `price`, `amount`, `clientOrderId` | В `_emitOrderPlacedTo()` при успешной постановке ордера, перед callback `PrivateNote.onOrderPlaced(...)`. | `address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress)` = `143` | [OrderBook.sol:1134](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1134) |
| `OrderCancelled` | `orderId`, `clientOrderId` | В общей helper-функции `_emitOrderCancelled()`, которая вызывается из сценариев обычной отмены, cancel-all и внутренних no-book-entry / shutdown-cancel путей. | `address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress)` = `144` | [OrderBook.sol:1142](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1142) |
| `OrderFilled` | `orderId`, `filledAmount`, `clearingPrice`, `feeAmount`, `isTaker` | В `_processFillTo()` на каждый match/fill, перед callback `PrivateNote.onOrderFilled(...)`. | `address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress)` = `146` | [OrderBook.sol:1225](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1225) |
| `PartialFill` | `orderId`, `clientOrderId`, `filledAmount`, `remainingAmount` | В `_emitPartialFill()` один раз на завершение обработки конкретного taker-order, если после матчинга остается остаток. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:1177](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1177) |
| `FullyFilled` | `orderId`, `clientOrderId`, `filledAmount` | В `_emitFullyFilled()` один раз на завершение обработки конкретного taker-order, если он полностью исполнен. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:1182](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1182) |
| `Queued` | `slot`, `queueId`, `entryType` | После успешной постановки `QueueEntry` в `_enqueuePlace()`, `_enqueueCancel()` и `_enqueueCancelAll()`. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:481](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:481), [OrderBook.sol:515](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:515), [OrderBook.sol:546](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:546) |
| `Rejected` | `entryType`, `depositHash` | При немедленном reject place-заявки на этапе pre-validation в `executeBatch()`, а также при переполнении очереди в `_enqueuePlace()`, `_enqueueCancel()`, `_enqueueCancelAll()`. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:362](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:362), [OrderBook.sol:456](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:456), [OrderBook.sol:491](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:491), [OrderBook.sol:522](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:522) |
| `CallbackBounced` | `dest`, `lt` | В `onBounce()` когда любой исходящий callback `OrderBook -> PrivateNote` отскакивает назад. Это observability hook, состояние OrderBook автоматически не откатывает. | `address.makeAddrExtern(0, bitCntAddress)` | [OrderBook.sol:1367](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1367) |

## Nullifier

| Contract | Event declarations | Комментарий |
| --- | --- | --- |
| `Nullifier` | Нет | У `contracts/Nullifier.sol` нет собственных `event`-объявлений. |
