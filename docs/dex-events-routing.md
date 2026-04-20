# DODEX Event Routing

Документ фиксирует по каждому `event` из `contracts`:

- в какой момент он отправляется;
- кому он отправляется (`dst`), если используется `emit ...{dest: ...}`;
- если событие объявлено, но в текущей реализации не эмитится, это отмечено явно.

## Общий принцип доставки

В DEX-системе внешние события обычно отправляются на внешний адрес вида:

```solidity
address.makeAddrExtern(EVENT_ID, bitCntAddress)
```

То есть `dst` определяется не пользовательским адресом, а внешним event-channel адресом, собранным из numeric event id.

Исключение в текущем коде:

- некоторые события `OrderBook` отправляются в `address.makeAddrExtern(0, bitCntAddress)`;
- некоторые объявленные события вообще не эмитятся.

## RootPN

### `voucherGenerated(uint256 sk_u_commit, uint voucher_nominal, uint32 token_type)`

- Когда отправляется: в `generatevoucher()` после проверки номинала ваучера и перед завершением обработки входящего сообщения.
- `dst`: `address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress)` (`135`).
- Код: [contracts/RootPN.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:245), [contracts/RootPN.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:258)

### `PrivateNoteDeployed(uint256 depositIdentifierHash, address noteAddress, uint128 initialBalance)`

- Когда отправляется: в `privateNoteDeployed()` когда `RootPN` получает callback от только что задеплоенного `PrivateNote` и фиксирует его баланс в `_deployedValues`.
- `dst`: `address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress)` (`101`).
- Код: [contracts/RootPN.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:169), [contracts/RootPN.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:171)

### `NullifierDeployed(address nullifierAddress, uint64 value)`

- Когда отправляется: в `sendEccShellToPrivateNote()` после успешной zk-проверки и после деплоя `Nullifier`.
- `dst`: `address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress)` (`102`).
- Код: [contracts/RootPN.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:95), [contracts/RootPN.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootPN.sol:130)

## RootOracle

### `OracleDeployed(address oracle, uint256 pubkey, string name)`

- Когда отправляется: в `deployOracle()` сразу после деплоя нового `Oracle`.
- `dst`: `address.makeAddrExtern(ROOTORACLE_ORACLE_DEPLOYED, bitCntAddress)` (`136`).
- Код: [contracts/RootOracle.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootOracle.sol:52), [contracts/RootOracle.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/RootOracle.sol:61)

## Oracle

### `OracleEventListDeployed(address eventListAddress, uint128 index)`

- Когда отправляется:
- в конструкторе `Oracle` после деплоя дефолтного `OracleEventList` с `index = 0`;
- в `deployEventList(uint128 index)` после деплоя дополнительного `OracleEventList`.
- `dst`: `address.makeAddrExtern(ORACLE_DEPLOYED, bitCntAddress)` (`104`).
- Код: [contracts/Oracle.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:59), [contracts/Oracle.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:66), [contracts/Oracle.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:75), [contracts/Oracle.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:82)

### `EventPublished(uint256 event_id, string event_name)`

- Когда отправляется: не отправляется в текущей реализации.
- `dst`: отсутствует, потому что `emit` для этого события в коде нет.
- Код объявления: [contracts/Oracle.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/Oracle.sol:36)

## OracleEventList

### `EventAdded(uint256 event_id, string event_name, uint128 oracle_fee, uint64 deadline)`

- Когда отправляется: в `addEvent()` после записи нового `EventInfo` в `_events[event_id]`.
- `dst`: `address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress)` (`133`).
- Код: [contracts/OracleEventList.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:68), [contracts/OracleEventList.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:91)

### `EventConfirmed(uint256 event_id, address pmpAddress)`

- Когда отправляется: в `confirmEvent()` при успешной валидации oracle fee и `deadline`, после увеличения `eventInfo.count` и вызова `PMP.approveEvent(...)`.
- `dst`: `address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress)` (`106`).
- Код: [contracts/OracleEventList.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:99), [contracts/OracleEventList.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OracleEventList.sol:115)

## PrivateNote

### `OwnerChanged(uint256 oldPubkey, uint256 newPubkey)`

- Когда отправляется: в `changeOwner()` после обновления `_ephemeral_pubkey`.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress)` (`112`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:199), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:207)

### `StakeConfirmed(address stakeController, uint32 outcome, uint128 amount, uint8 bet_type)`

- Когда отправляется: в `onStakeAccepted()` после переноса `candidate_amount` в подтвержденные stake-массивы.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress)` (`113`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:684), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:713)

### `StakeCancelled(address stakeController, uint128 value)`

- Когда отправляется: в `onStakeCancelled()` после удаления stake-record, возврата средств в `_balance` и возврата coupons в `_coupons_value`.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_STAKE_CANCELLED, bitCntAddress)` (`115`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:397), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:408)

### `FullSetStakeConfirmed(address stakeController, uint128[] amount)`

- Когда отправляется: в `onSplitAccepted()` после добавления split amounts в `stake.amount[]`.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress)` (`138`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:473), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:502)

### `FullSetStakeCancelled(address stakeController, uint128 value)`

- Когда отправляется: в `onMergeAccepted()` после списания outcome-token amounts из stake и возврата collateral в `_balance[token_type]`.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_MERGE_CONFIRMED, bitCntAddress)` (`139`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:571), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:601)

### `ClaimAccepted(address stakeController, optional(uint32) outcome, uint128 payout)`

- Когда отправляется: в `onClaimAccepted()` только если `outcome.hasValue() == true`, то есть когда PMP уже resolved и payout реально зачисляется.
- Не отправляется в ветке `!outcome.hasValue()`: там функция просто очищает `_busy` и выходит.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress)` (`114`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:755), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:779)

### `PMPDeployed(uint256 event_id, uint32 token_type, address pmpAddress, address[] oracleEventLists, uint128[] oracleFee)`

- Когда отправляется: в `deployPMP()` после вычисления `pmpAddress`, подготовки `_stakes[hash]` и установки `_busy`, непосредственно перед `new PMP{...}`.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress)` (`111`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:250), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:311)

### `OrderPlacedConfirmed(address orderBook, uint128 orderId)`

- Когда отправляется: в `onOrderPlaced()` после валидации `msg.sender` как ожидаемого `OrderBook` и после записи `_orderFeeReserves[orderId]` для buy-ордера.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress)` (`147`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1166), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1209)

### `OrderFilledConfirmed(address orderBook, uint128 filledAmount, bool isBuy)`

- Когда отправляется: в `onOrderFilled()` после обработки fill в локальном состоянии `PrivateNote`.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress)` (`148`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1381), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:1447)

### `TransferInitiated(address dest, uint32 token_type, uint128 amount)`

- Когда отправляется: в `initTransfer()` после блокировки суммы в `_pendingTransferAmount` и перед отправкой `offerTransfer()` в другой `PrivateNote`.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress)` (`149`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:946), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:967)

### `TransferReceived(address from, uint32 token_type, uint128 amount)`

- Когда отправляется: в `offerTransfer()` после кредитования `_balance[token_type]` у получателя.
- `dst`: `address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress)` (`150`).
- Код: [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:981), [contracts/PrivateNote.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PrivateNote.sol:990)

## PMP

### `StakeAccepted(address indexed note, uint32 outcomeId, uint128 amount, uint8 bet_type)`

- Когда отправляется: в `acceptStake()` после обновления pool accounting и callback `PrivateNote.onStakeAccepted(...)`.
- `dst`: `address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress)` (`118`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:462), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:511)

### `ApprovedByOracle(address oracleEventList, uint256 oraclePubkey)`

- Когда отправляется: в `approveEvent()` в момент, когда собраны все обязательные oracle approvals, то есть на последнем подтверждении.
- `dst`: `address.makeAddrExtern(PMP_APPROVED_BY_ORACLE, bitCntAddress)` (`119`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:346), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:398)

### `Resolved(uint32 outcomeId)`

- Когда отправляется: в `resolve()` после установки `_resolvedOutcome`, вычисления payout coefficients и, при наличии, отправки creator fee.
- `dst`: `address.makeAddrExtern(PMP_RESOLVED, bitCntAddress)` (`120`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:883), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:939)

### `ClaimProcessed(address indexed note, uint128 payout, bool win)`

- Когда отправляется: в `claim()` после callback `PrivateNote.onClaimAccepted(...)`, если рынок уже resolved.
- `dst`: `address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress)` (`121`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:973), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:1064)

### `NetworkFeeBurned(uint64 amount)`

- Когда отправляется: не отправляется в текущей реализации.
- `dst`: отсутствует, потому что `emit` для этого события в коде нет.
- Код объявления: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:219)

### `TimingsSet(uint64 stakeStart, uint64 stakeEnd, uint64 resultStart, uint64 resultEnd)`

- Когда отправляется: в `setTimings()` после фиксации `_stakeStart`, `_resultStart`, выставления `_approved = true` и возможного auto-freeze.
- `dst`: `address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress)` (`124`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:415), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:438)

### `NumOutcomesSet(uint32 numOutcomes)`

- Когда отправляется: не отправляется в текущей реализации.
- `dst`: отсутствует, потому что `emit` для этого события в коде нет.
- Код объявления: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:231)

### `EventCancelled()`

- Когда отправляется: в `cancelEvent()` после установки `_isCancelled = true`.
- `dst`: `address.makeAddrExtern(PMP_EVENT_CANCELLED, bitCntAddress)` (`126`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:443), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:453)

### `PMPCancelled()`

- Когда отправляется: в `rejectEvent()` когда `OracleEventList` отклоняет рынок или при rollback сценарии до selfdestruct.
- `dst`: `address.makeAddrExtern(PMP_CANCELLED_BY_ORACLE, bitCntAddress)` (`132`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:292), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:297)

### `CreatorFeeCollected(uint128 fee)`

- Когда отправляется: в `resolve()` если `_creatorFee > 0`, после отправки fee в `PrivateNote(_deployer).acceptFee(...)`.
- `dst`: `address.makeAddrExtern(PMP_CREATOR_FEE_COLLECTED, bitCntAddress)` (`137`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:928), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:935)

### `PoolsFrozen(uint128 baseTotalPool)`

- Когда отправляется: в `_ensureFrozen()` после freeze snapshot и после деплоя `OrderBook`.
- `dst`: `address.makeAddrExtern(PMP_POOLS_FROZEN, bitCntAddress)` (`140`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:594), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:735)

### `SplitProcessed(address indexed note, uint128 collateral)`

- Когда отправляется: в `splitFullSet()` после callback `PrivateNote.onSplitAccepted(...)`.
- `dst`: `address.makeAddrExtern(PMP_SPLIT_PROCESSED, bitCntAddress)` (`141`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:747), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:797)

### `MergeProcessed(address indexed note, uint128 collateral)`

- Когда отправляется: в `mergeFullSet()` после callback `PrivateNote.onMergeAccepted(...)`.
- `dst`: `address.makeAddrExtern(PMP_MERGE_PROCESSED, bitCntAddress)` (`142`).
- Код: [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:808), [contracts/PMP.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/PMP.sol:877)

## OrderBook

### `OrderPlaced(uint128 orderId, uint32 outcomeId, bool isBuy, uint8 flags, uint256 price, uint128 amount)`

- Когда отправляется: в `_emitOrderPlacedTo()` при успешной постановке ордера, перед callback `PrivateNote.onOrderPlaced(...)`.
- `dst`: `address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress)` (`143`).
- Код: [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:833), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:841)

### `OrderCancelled(uint128 orderId)`

- Когда отправляется:
- в `_doCancel()` при успешной отмене конкретного ордера владельцем;
- в `_doCancelAll()` для каждого ордера в cancel-all проходе;
- в `shutdown()` для каждого ордера, который снимается во время дренажа книги.
- `dst`: `address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress)` (`144`).
- Код: [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:650), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:662), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:848), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:987)

### `OrderFilled(uint128 orderId, uint128 filledAmount, uint256 clearingPrice, uint128 feeAmount, bool isTaker)`

- Когда отправляется: в `_processFillTo()` при каждом match/fill, после расчета `feeAmount` и обновления `_totalMakerFees` / `_totalTakerFees`, перед callback `PrivateNote.onOrderFilled(...)`.
- `dst`: `address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress)` (`146`).
- Код: [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:889), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:912)

### `Queued(uint8 slot, uint32 queueId, uint8 entryType)`

- Когда отправляется:
- в `_enqueuePlace()` после записи `QueueEntry` с `entryType = QENTRY_PLACE`;
- в `_enqueueCancel()` после записи `QueueEntry` с `entryType = QENTRY_CANCEL`;
- в `_enqueueCancelAll()` после записи `QueueEntry` с `entryType = QENTRY_CANCEL_ALL`.
- `dst`: `address.makeAddrExtern(0, bitCntAddress)`.
- Код: [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:343), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:373), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:378), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:402), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:407), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:428)

### `Rejected(uint8 entryType, uint256 deposit_hash)`

- Когда отправляется:
- в `executeBatch()` если place-заявка не проходит локальную валидацию до постановки в очередь;
- в `_enqueuePlace()` если очередь place-потока заполнена;
- в `_enqueueCancel()` если очередь cancel-потока заполнена;
- в `_enqueueCancelAll()` если очередь cancel-потока заполнена.
- `dst`: `address.makeAddrExtern(0, bitCntAddress)`.
- Код: [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:218), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:263), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:353), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:382), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:408)

### `CallbackBounced(address dest, uint64 lt)`

- Когда отправляется: в `onBounce()` при bounce любого исходящего callback из `OrderBook`.
- `dst`: `address.makeAddrExtern(0, bitCntAddress)`.
- Поле `dest` внутри payload — это `msg.sender`, то есть адрес bounced callback receiver.
- Код: [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1033), [contracts/OrderBook.sol](/Users/ekaterinapantaz/Documents/GitHub/DODEX/contracts/OrderBook.sol:1035)

## События без собственной отправки

Ниже события объявлены, но `emit` для них в текущем коде отсутствует:

- `Oracle.EventPublished`
- `PMP.NetworkFeeBurned`
- `PMP.NumOutcomesSet`

`Nullifier` собственных `event`-объявлений не имеет.
