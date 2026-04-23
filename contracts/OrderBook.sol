pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PrivateNote.sol";
import "./PMP.sol";
import "./libraries/DexLib.sol";

/// @title OrderBook — native Solidity dark order book with price-indexed levels.
/// @notice Orders are stored by id in `_orders`. Per-market orders are organised
///         into a price-sorted dictionary of price levels (`_levels`). Each level
///         keeps a FIFO of orders at that price (time priority within level).
///         Matching walks levels in best-first order and stops as soon as it has
///         filled the taker order or hit MAX_MATCHES_PER_CALL.
contract OrderBook is Modifiers {

    /// @notice Contract semantic version.
    string constant version = "1.4.0";

    /// @notice Event identifier associated with this order book.
    uint256 static _eventId;
    /// @notice Oracle list hash associated with this order book.
    uint256 static _oracleListHash;
    /// @notice Token type associated with this order book.
    uint32 static _tokenType;

    /// @notice PrivateNote code used for deterministic wallet address resolution.
    TvmCell _privateNoteCode;

    /// @notice PMP contract that deployed this OrderBook.
    address _pmpAddress;

    /// @notice Result-window start propagated from PMP. Once
    ///         `block.timestamp >= _resultStart`, no new place/cancel actions
    ///         are accepted — the book is closed at the result deadline rather
    ///         than waiting for resolve, so trading cannot continue after the
    ///         outcome is observable off-chain.
    uint64 _resultStart;

    // ===== Order storage =====

    /// @notice Resting order record. Doubly linked into the per-price-level FIFO
    ///         (for time priority) and the per-owner FIFO (for cancel-all/getOrdersByOwner).
    struct Order {
        uint256 depositHash;
        uint256 price;          // basis points — also used as the level key
        uint128 amount;         // remaining
        uint128 minAmount;      // taker pre-check; not used for resting
        uint128 initialAmount;  // original size at placement — for PartialFill math
        uint128 filledAccum;    // cumulative filled size (survives continuations)
        uint128 clientOrderId;  // optional user-supplied id (0 = not set)
        uint64  epochId;
        uint32  outcomeId;
        uint8   flags;
        bool    isBuy;
        // Per-price-level intrusive FIFO links.
        uint128 nextAtPrice;    // 0 = end
        uint128 prevAtPrice;    // 0 = head
        // Per-owner intrusive FIFO links.
        uint128 nextInOwner;
        uint128 prevInOwner;
    }

    mapping(uint128 => Order) _orders;

    /// @notice Per-owner active `clientOrderId → internalOrderId` index.
    ///         Scope: per `depositHash` (PN). Zero means unused. Freed on
    ///         full fill or cancellation. `clientOrderId == 0` is reserved
    ///         as "not set" and bypasses both the uniqueness check and the
    ///         mapping entirely.
    mapping(uint256 => mapping(uint128 => uint128)) _clientOrderIds;

    /// @notice Per-price-level metadata. Keyed by (outcomeId, isBuy, epochId, price).
    ///         epochId is part of the key so orders from different epochs live
    ///         in disjoint FIFOs and never co-mingle. The mapping over `price`
    ///         is sorted (Patricia trie), enabling best-price iteration via
    ///         min()/max()/next()/prev() within a single epoch.
    struct PriceLevel {
        uint128 firstOrderId;   // FIFO head
        uint128 lastOrderId;    // FIFO tail
        uint128 totalAmount;    // sum of order amounts at this level (single-epoch)
    }

    mapping(uint32 => mapping(bool => mapping(uint64 => mapping(uint256 => PriceLevel)))) _levels;

    /// @notice Per-owner FIFO (insertion order across all markets).
    mapping(uint256 => uint128) _ownerHead;
    mapping(uint256 => uint128) _ownerTail;

    /// @notice Monotonic order id (next id to assign; id 0 reserved for "none").
    uint128 _nextOrderId;

    /// @notice Total number of resting orders across all markets.
    uint32 _orderCount;

    /// @notice True once shutdown() has been initiated. While set, no new
    ///         place/cancel operations are accepted; the contract is draining.
    bool _shuttingDown;

    /// @notice Resume cursor for shutdown's batch scan across self-calls.
    ///         Persisted across tx boundaries so each self-call picks up
    ///         where the previous one left off instead of re-scanning
    ///         already-emptied slots 1..cursor. Without this, draining
    ///         N live orders in batches of MAX_SHUTDOWN_BATCH costs
    ///         O(N^2/B) storage reads; with the cursor it is O(N).
    ///         `_nextOrderId` is frozen once `_shuttingDown=true` (no new
    ///         orders can be placed), so the cursor always terminates.
    uint128 _shutdownCursor;

    /// @notice Accumulated maker/taker fees (in collateral units).
    uint128 _totalMakerFees;
    uint128 _totalTakerFees;

    // ===== Matching constants =====

    /// @notice Maximum fill count per single processHead invocation.
    ///         Bounded by TVM's 255 outgoing-action limit: each match emits 4
    ///         actions (2 extern events + 2 internal callbacks). 30 × 4 = 120
    ///         leaves ample room for the worst-case executeBatch interleave
    ///         (5 PLACE × validation/Queued/onOrderRejected actions + the
    ///         processHead invocation that follows in the same tx + the
    ///         onBatchComplete sentinel).
    uint8 constant MAX_MATCHES_PER_CALL = 30;

    /// @notice Maximum cancellations per ACTION_CANCEL_ALL pass.
    ///         Each cancel emits 2 actions; 30 × 2 = 60 with similar headroom.
    uint8 constant MAX_CANCEL_ALL_PER_CALL = 30;

    /// @notice Maximum cancellations per shutdown pass (any owner).
    uint8 constant MAX_SHUTDOWN_BATCH = 10;

    // MAX_BATCH_SIZE is inherited from Modifiers (shared with PrivateNote).

    // ===== Order flags =====
    uint8 constant FLAG_IOC       = 0x01;
    uint8 constant FLAG_FOK       = 0x02;
    uint8 constant FLAG_MARKET    = 0x04;
    uint8 constant FLAG_POST_ONLY = 0x08;
    uint8 constant TAKER_FLAGS_MASK = 0x07;

    /// @notice Parameters for placing a single order.
    /// @dev `clientOrderId` is optional (0 = not set). When non-zero it is
    ///      validated as unique among the caller's currently-active orders.
    ///      A duplicate cid reverts the whole batch (no silent override).
    struct PlaceParams {
        uint32  outcomeId;
        bool    isBuy;
        uint8   flags;
        uint256 price;
        uint128 amount;
        uint128 minAmount;
        uint64  epochId;
        uint128 clientOrderId;
    }

    // ===== Queue (circular, slot 0..99) =====

    uint8 constant QENTRY_PLACE      = 1;
    uint8 constant QENTRY_CANCEL     = 2;
    uint8 constant QENTRY_CANCEL_ALL = 3;

    uint8 constant QUEUE_CAPACITY = 100;
    uint8 constant QUEUE_PLACE_LIMIT = 90;

    /// @notice Maximum levels the FOK / minAmount pre-check may walk in
    ///         a single tx before yielding to a continuation. Bounds gas
    ///         on extremely deep books (otherwise a sufficiently large
    ///         book would let pre-check exhaust gas mid-tx, wedging the
    ///         queue at the head entry).
    uint32 constant MAX_PRECHECK_ITERATIONS = 3000;

    struct QueueEntry {
        uint8   entryType;
        bool    cancelled;
        uint32  queueId;
        uint256 depositHash;
        // Place fields:
        uint32  outcomeId;
        bool    isBuy;
        uint8   flags;
        uint256 price;
        uint128 amount;
        uint128 minAmount;
        uint64  epochId;
        uint128 clientOrderId;
        // Continuation field for matching after MAX_MATCHES cap.
        uint128 targetOrderId;
        // Cumulative taker-side fill across continuations. PartialFill /
        // FullyFilled is emitted ONCE at final completion (no more cont) using
        // this aggregate.
        uint128 filledAccum;
        // Continuation fields for the pre-check phase. precheckDone=false
        // means the pre-check still has more levels to walk; precheckAccum
        // and precheckLastPrice are the resume cursor.
        bool    precheckDone;
        uint128 precheckAccum;
        uint256 precheckLastPrice;
    }

    mapping(uint8 => QueueEntry) _queue;

    uint8  _queueHead;
    uint8  _queueTail;
    uint8  _queueSize;
    uint32 _nextQueueId;

    // ===== Events =====

    event OrderPlaced(uint128 orderId, uint32 outcomeId, bool isBuy, uint8 flags, uint256 price, uint128 amount, uint128 clientOrderId);
    event OrderCancelled(uint128 orderId, uint128 clientOrderId);
    event OrderFilled(uint128 orderId, uint128 filledAmount, uint256 clearingPrice, uint128 feeAmount, bool isTaker);
    /// @notice Aggregated MM-friendly fill events. Emitted ONCE per order
    ///         after matching for that order completes (across continuations):
    ///         - `PartialFill` if the order remains in the book with leftover
    ///         - `FullyFilled` if the order is fully consumed
    ///         Per-fill `OrderFilled` continues to fire for raw analytics.
    event PartialFill(uint128 orderId, uint128 clientOrderId, uint128 filledAmount, uint128 remainingAmount);
    event FullyFilled(uint128 orderId, uint128 clientOrderId, uint128 filledAmount);
    event Queued(uint8 slot, uint32 queueId, uint8 entryType);
    event Rejected(uint8 entryType, uint256 depositHash);
    /// @notice Emitted when an outgoing callback (onOrderFilled / onOrderCancelled / onOrderPlaced
    ///         / onBatchComplete / onOrderRejected) bounces back. Off-chain monitors should pick
    ///         this up and reconcile the affected PN. State on OB is NOT auto-rolled back —
    ///         an order removed during matching whose Filled callback later bounces stays
    ///         removed; the bounced credit needs operator-driven recovery.
    event CallbackBounced(address dest, uint64 lt);

    // ===== Constructor =====

    constructor(
        uint256 pmpSaltedCodeHash,
        uint16 pmpSaltedCodeDepth,
        uint64 resultStart
    ) {
        tvm.accept();
        ensureBalance();

        TvmCell salt = abi.codeSalt(tvm.code()).get();
        (TvmCell PrivateNoteCode) = abi.decode(salt, (TvmCell));
        _privateNoteCode = PrivateNoteCode;

        _pmpAddress = msg.sender;
        require(msg.sender == DexLib.computePMPAddressFromHash(
            pmpSaltedCodeHash, pmpSaltedCodeDepth,
            _eventId, _oracleListHash, _tokenType
        ), ERR_INVALID_SENDER);

        _resultStart = resultStart;
        _nextOrderId = 1;
        _orderCount = 0;

        _queueHead = 0;
        _queueTail = 0;
        _queueSize = 0;
        _nextQueueId = 1;
    }

    /// @notice PMP propagates a new resultStart when oracles update timings
    ///         before the prior result window has elapsed.
    function setResultStart(uint64 resultStart) public {
        require(msg.sender == _pmpAddress, ERR_INVALID_SENDER);
        require(block.timestamp < _resultStart, ERR_RESULT_NOT_STARTED);
        require(resultStart > block.timestamp, ERR_INVALID_PARAMS);
        tvm.accept();
        ensureBalance();
        _resultStart = resultStart;
    }

    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    function _notifyRejectedPlace(
        uint256 depositHash,
        PlaceParams op
    ) private view {
        address pn = DexLib.computePrivateNoteAddress(_privateNoteCode, depositHash);
        PrivateNote(pn).onOrderRejected{
            value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_eventId, _oracleListHash, _tokenType,
          op.outcomeId, op.isBuy, op.flags, op.price, op.amount);
    }

    // ===== Unified entry point: enqueue + processHead =====

    function executeBatch(
        uint256 depositIdentifierHash,
        PlaceParams[] orders,
        uint128[] cancelIds
    ) public {
        require(!_shuttingDown, ERR_ALREADY_CANCELLED);
        // Book is closed at the result deadline.
        require(block.timestamp < _resultStart, ERR_RESULT_NOT_STARTED);
        address wallet = DexLib.computePrivateNoteAddress(
            _privateNoteCode,
            depositIdentifierHash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();

        uint32 nPlace  = uint32(orders.length)    > MAX_BATCH_SIZE ? MAX_BATCH_SIZE : uint32(orders.length);
        uint32 nCancel = uint32(cancelIds.length) > MAX_BATCH_SIZE ? MAX_BATCH_SIZE : uint32(cancelIds.length);

        uint128 minNotional = minOrderNotional(_tokenType);
        uint128 lot = lotSize(_tokenType);
        for (uint32 i = 0; i < nPlace; i++) {
            PlaceParams op = orders[i];
            bool valid = true;

            // Validate at queue-insertion time. Every revert path inside
            // _doPlace must be pre-caught here so a bad entry can never
            // enter the queue — a later _processHeadCore pass that reverts
            // on a stuck head would permanently deadlock the book.
            bool opIsPostOnly = (op.flags & FLAG_POST_ONLY) != 0;
            bool opIsMarket   = (op.flags & FLAG_MARKET)    != 0;
            bool opIsIoc      = (op.flags & FLAG_IOC)       != 0;
            bool opIsFok      = (op.flags & FLAG_FOK)       != 0;

            if (op.amount == 0) valid = false;
            else if (op.amount % lot != 0) valid = false;
            else if (op.minAmount > op.amount) valid = false;
            else if (opIsPostOnly && (opIsMarket || opIsIoc || opIsFok)) valid = false;
            else if (opIsIoc && opIsFok) valid = false;
            else if (opIsMarket && (opIsIoc || opIsFok)) valid = false;
            else if (opIsMarket) {
                if (op.isBuy && op.amount < minNotional) valid = false;
            } else {
                if (op.price == 0) valid = false;
                else if (op.price % TICK_SIZE != 0) valid = false;
                else {
                    uint128 notional = uint128(
                        (uint256(op.amount) * uint256(op.price)) / uint256(FULL_PERCENT)
                    );
                    if (notional < minNotional) valid = false;
                }
            }
            // clientOrderId uniqueness gate (per-PN, opt-out via cid=0).
            if (valid && op.clientOrderId != 0
                && _clientOrderIds[depositIdentifierHash][op.clientOrderId] != 0) {
                valid = false;
            }

            if (valid) {
                bool ok = _enqueuePlace(
                    depositIdentifierHash, op.outcomeId, op.isBuy,
                    op.flags, op.price, op.amount, op.minAmount, op.epochId, op.clientOrderId
                );
                if (!ok) {
                    _notifyRejectedPlace(depositIdentifierHash, op);
                } else if (op.clientOrderId != 0) {
                    // Reserve the cid slot now (sentinel = max uint128, replaced
                    // with real orderId once _doPlace assigns one). Prevents a
                    // second batch entry from claiming the same cid before
                    // _doPlace runs from the queue.
                    _clientOrderIds[depositIdentifierHash][op.clientOrderId] = type(uint128).max;
                }
            } else {
                address addrExtern = address.makeAddrExtern(0, bitCntAddress);
                emit Rejected{dest: addrExtern}(QENTRY_PLACE, depositIdentifierHash);
                _notifyRejectedPlace(depositIdentifierHash, op);
            }
        }

        for (uint32 j = 0; j < nCancel; j++) {
            _enqueueCancel(depositIdentifierHash, cancelIds[j]);
        }

        _processHeadCore();
        _notifyBatchAccepted(depositIdentifierHash);
    }

    function cancelAllOrders(uint256 depositIdentifierHash) public {
        require(!_shuttingDown, ERR_ALREADY_CANCELLED);
        require(block.timestamp < _resultStart, ERR_RESULT_NOT_STARTED);
        address wallet = DexLib.computePrivateNoteAddress(
            _privateNoteCode,
            depositIdentifierHash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();

        _enqueueCancelAll(depositIdentifierHash);
        _processHeadCore();
        _notifyBatchAccepted(depositIdentifierHash);
    }

    function cancelQueued(uint8 slot, uint32 queueId, uint256 depositIdentifierHash) public {
        require(!_shuttingDown, ERR_ALREADY_CANCELLED);
        require(block.timestamp < _resultStart, ERR_RESULT_NOT_STARTED);
        address wallet = DexLib.computePrivateNoteAddress(
            _privateNoteCode,
            depositIdentifierHash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();

        _markQueueCancelled(slot, queueId, depositIdentifierHash);
        _processHeadCore();
        _notifyBatchAccepted(depositIdentifierHash);
    }

    function _notifyBatchAccepted(uint256 depositIdentifierHash) private view {
        address pn = DexLib.computePrivateNoteAddress(
            _privateNoteCode, depositIdentifierHash
        );
        PrivateNote(pn).onBatchComplete{
            value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_eventId, _oracleListHash, _tokenType);
    }

    // ===== Queue helpers =====

    function _canQueuePlace() private view returns (bool) {
        return _queueSize < QUEUE_PLACE_LIMIT;
    }

    function _canQueueCancel() private view returns (bool) {
        return _queueSize < QUEUE_CAPACITY;
    }

    function _allocSlot() private returns (uint8 slot, uint32 queueId) {
        slot = _queueTail;
        queueId = _nextQueueId;
        _nextQueueId++;
        if (_nextQueueId == 0) {
            _nextQueueId = 1;
        }
        _queueTail = uint8((uint32(_queueTail) + 1) % uint32(QUEUE_CAPACITY));
        _queueSize++;
    }

    function _advanceHead() private {
        delete _queue[_queueHead];
        _queueHead = uint8((uint32(_queueHead) + 1) % uint32(QUEUE_CAPACITY));
        _queueSize--;
    }

    function _enqueuePlace(
        uint256 depositHash,
        uint32  outcomeId,
        bool    isBuy,
        uint8   flags,
        uint256 price,
        uint128 amount,
        uint128 minAmount,
        uint64  epochId,
        uint128 clientOrderId
    ) private returns (bool ok) {
        if (!_canQueuePlace()) {
            address addrExtern = address.makeAddrExtern(0, bitCntAddress);
            emit Rejected{dest: addrExtern}(QENTRY_PLACE, depositHash);
            return false;
        }
        (uint8 slot, uint32 queueId) = _allocSlot();
        _queue[slot] = QueueEntry({
            entryType: QENTRY_PLACE,
            cancelled: false,
            queueId: queueId,
            depositHash: depositHash,
            outcomeId: outcomeId,
            isBuy: isBuy,
            flags: flags,
            price: price,
            amount: amount,
            minAmount: minAmount,
            epochId: epochId,
            clientOrderId: clientOrderId,
            targetOrderId: 0,
            filledAccum: 0,
            precheckDone: false,
            precheckAccum: 0,
            precheckLastPrice: 0
        });
        // (placeholder anchor — _enqueueCancel/_enqueueCancelAll use clientOrderId: 0)
        address addrExtern2 = address.makeAddrExtern(0, bitCntAddress);
        emit Queued{dest: addrExtern2}(slot, queueId, QENTRY_PLACE);
        return true;
    }

    function _enqueueCancel(
        uint256 depositHash,
        uint128 targetOrderId
    ) private returns (bool ok) {
        if (!_canQueueCancel()) {
            address addrExtern = address.makeAddrExtern(0, bitCntAddress);
            emit Rejected{dest: addrExtern}(QENTRY_CANCEL, depositHash);
            return false;
        }
        (uint8 slot, uint32 queueId) = _allocSlot();
        _queue[slot] = QueueEntry({
            entryType: QENTRY_CANCEL,
            cancelled: false,
            queueId: queueId,
            depositHash: depositHash,
            outcomeId: 0,
            isBuy: false,
            flags: 0,
            price: 0,
            amount: 0,
            minAmount: 0,
            epochId: 0,
            clientOrderId: 0,
            targetOrderId: targetOrderId,
            filledAccum: 0,
            precheckDone: false,
            precheckAccum: 0,
            precheckLastPrice: 0
        });
        address addrExtern2 = address.makeAddrExtern(0, bitCntAddress);
        emit Queued{dest: addrExtern2}(slot, queueId, QENTRY_CANCEL);
        return true;
    }

    function _enqueueCancelAll(uint256 depositHash) private returns (bool ok) {
        if (!_canQueueCancel()) {
            address addrExtern = address.makeAddrExtern(0, bitCntAddress);
            emit Rejected{dest: addrExtern}(QENTRY_CANCEL_ALL, depositHash);
            return false;
        }
        (uint8 slot, uint32 queueId) = _allocSlot();
        _queue[slot] = QueueEntry({
            entryType: QENTRY_CANCEL_ALL,
            cancelled: false,
            queueId: queueId,
            depositHash: depositHash,
            outcomeId: 0,
            isBuy: false,
            flags: 0,
            price: 0,
            amount: 0,
            minAmount: 0,
            epochId: 0,
            clientOrderId: 0,
            targetOrderId: 0,
            filledAccum: 0,
            precheckDone: false,
            precheckAccum: 0,
            precheckLastPrice: 0
        });
        address addrExtern2 = address.makeAddrExtern(0, bitCntAddress);
        emit Queued{dest: addrExtern2}(slot, queueId, QENTRY_CANCEL_ALL);
        return true;
    }

    function _markQueueCancelled(uint8 slot, uint32 queueId, uint256 depositHash) private returns (bool ok) {
        if (_queue[slot].queueId != queueId) return false;
        if (_queue[slot].depositHash != depositHash) return false;
        if (_queue[slot].cancelled) return true;
        _queue[slot].cancelled = true;
        return true;
    }

    function _selfCallProcessHead() private pure {
        OrderBook(address(this)).processHead{
            value: 1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }();
    }

    /// @notice Public entry: accepts the call (anyone can poke the queue) and
    ///         delegates to the internal core. Same-tx invocations from
    ///         executeBatch/cancelAll/cancelQueued should call _processHeadCore
    ///         directly to avoid a redundant tvm.accept().
    function processHead() public {
        tvm.accept();
        ensureBalance();
        _processHeadCore();
    }

    function _processHeadCore() private {
        // Once shutdown is latched, do NOT process queued PLACE entries — they
        // could re-insert orders into the book while shutdown is draining it.
        // CANCEL/CANCEL_ALL are also skipped: shutdown will clear the book
        // wholesale anyway. PNs whose ops were in flight will not get callbacks
        // here; the event is being torn down so this is acceptable.
        if (_shuttingDown) return;
        if (_queueSize == 0) return;

        // Skip up to 5 cancelled queue entries in the same tx; advancing the
        // head is cheap (storage delete + counter), no need to defer.
        uint8 skipped = 0;
        while (_queueSize > 0 && _queue[_queueHead].cancelled && skipped < 5) {
            _advanceHead();
            skipped++;
        }

        if (_queueSize == 0) return;

        // If we hit the skip-limit with the head still cancelled, defer the
        // next pass to a fresh tx via self-call instead of executing a
        // cancelled entry here. Without this, a 6th consecutive cancelled
        // entry would fall through to the entryType dispatch below and be
        // processed — for a PLACE that means the order gets inserted into
        // the book despite the cancelQueued marker, leaving the caller's
        // lock stuck in `_lockedInOrders` until they manually cancelOrder.
        if (_queue[_queueHead].cancelled) {
            _selfCallProcessHead();
            return;
        }

        QueueEntry head = _queue[_queueHead];
        bool keepHead = false;

        if (head.entryType == QENTRY_CANCEL) {
            _doCancel(head.depositHash, head.targetOrderId);
        } else if (head.entryType == QENTRY_CANCEL_ALL) {
            uint32 cancelled = _doCancelAll(head.depositHash);
            keepHead = (cancelled >= MAX_CANCEL_ALL_PER_CALL);
        } else {
            PlaceParams p = PlaceParams(
                head.outcomeId, head.isBuy, head.flags, head.price,
                head.amount, head.minAmount, head.epochId, head.clientOrderId
            );
            (bool cont, uint128 contOrderId, uint128 contRemaining,
             bool ctPrecheckDone, uint128 ctPrecheckAccum, uint256 ctPrecheckLastPrice,
             uint128 ctFilledAccum) =
                _doPlace(
                    head.depositHash, p, head.targetOrderId,
                    head.precheckDone, head.precheckAccum, head.precheckLastPrice,
                    head.filledAccum
                );
            if (cont) {
                _queue[_queueHead].amount = contRemaining;
                _queue[_queueHead].targetOrderId = contOrderId;
                _queue[_queueHead].precheckDone = ctPrecheckDone;
                _queue[_queueHead].precheckAccum = ctPrecheckAccum;
                _queue[_queueHead].precheckLastPrice = ctPrecheckLastPrice;
                _queue[_queueHead].filledAccum = ctFilledAccum;
                keepHead = true;
            }
        }

        if (!keepHead) _advanceHead();
        if (_queueSize > 0) _selfCallProcessHead();
    }

    // ===== Native matching engine =====

    /// @notice Place a single order, optionally as a continuation.
    ///         This function must NEVER revert — it runs from _processHeadCore
    ///         after a queue entry has been committed. A revert here would
    ///         abort every future _processHeadCore tx and permanently wedge
    ///         the queue (all subsequent executeBatch/cancel operations would
    ///         also revert because they end with _processHeadCore).
    ///         Input validity is enforced at queue-insertion time in
    ///         executeBatch (the boundary).
    ///
    ///         The function has two continuation modes, both signaled by
    ///         returning cont=true:
    ///         1. Pre-check continuation (precheckDone=false): the FOK /
    ///            minAmount level walk hit MAX_PRECHECK_ITERATIONS without
    ///            reaching its target. accum + lastPrice are the resume
    ///            cursor for the next pass.
    ///         2. Match continuation (precheckDone=true): pre-check is done,
    ///            the match loop hit MAX_MATCHES_PER_CALL with leftover.
    ///            contRemaining + contOrderId are the resume cursor.
    function _doPlace(
        uint256 callerHash,
        PlaceParams p,
        uint128 existingOrderId,
        bool    inPrecheckDone,
        uint128 inPrecheckAccum,
        uint256 inPrecheckLastPrice,
        uint128 inFilledAccum
    ) private returns (
        bool cont,
        uint128 contOrderId,
        uint128 contRemaining,
        bool contPrecheckDone,
        uint128 contPrecheckAccum,
        uint256 contPrecheckLastPrice,
        uint128 contFilledAccum
    ) {
        bool isContinuation = existingOrderId != 0;
        // Taker-side fill aggregator (carried across continuations via the
        // queue entry). Used to emit a single PartialFill / FullyFilled event
        // when matching for this order is fully done.
        uint128 takerFilledAccum = inFilledAccum;

        uint8 fl = p.flags;
        bool isMarket = (fl & FLAG_MARKET) != 0;
        bool isIoc    = (fl & FLAG_IOC) != 0;
        bool isFok    = (fl & FLAG_FOK) != 0;
        bool isPostOnly = (fl & FLAG_POST_ONLY) != 0;

        uint256 storedPrice;
        if (isMarket) {
            storedPrice = p.isBuy ? type(uint256).max : 0;
        } else {
            storedPrice = p.price;
        }

        uint128 orderId = isContinuation ? existingOrderId : _nextOrderId++;

        // Compute caller PN address once and reuse across all callbacks.
        address callerPn = DexLib.computePrivateNoteAddress(_privateNoteCode, callerHash);

        if (!isContinuation) _emitOrderPlacedTo(callerPn, orderId, p);

        // ── POST_ONLY check: only on first call. (Cheap — single _bestOpposite.)
        if (!isContinuation && isPostOnly) {
            optional(uint256, PriceLevel) bestLevel = _bestOpposite(p.outcomeId, p.isBuy, p.epochId);
            if (bestLevel.hasValue()) {
                (uint256 bp, ) = bestLevel.get();
                if (_pricesCross(p.isBuy, storedPrice, false, bp)) {
                    uint128 returnAmt = _collateralFor(p.isBuy, storedPrice, p.amount);
                    _cancelNoBookEntryTo(callerPn, callerHash, orderId, p.outcomeId, p.isBuy, returnAmt, p.clientOrderId);
                    return (false, 0, 0, false, 0, 0, 0);
                }
            }
        }

        // ── FOK / minAmount pre-check (resumable, capped at MAX_PRECHECK_ITERATIONS).
        //    On the first call inPrecheckDone is false and the cursor is (0, 0).
        //    On a pre-check continuation we resume from inPrecheckAccum / inPrecheckLastPrice.
        //    Once pre-check passes (or there's no minFill), inPrecheckDone is true on
        //    subsequent (match) continuations and this block is skipped entirely.
        uint128 minFill = isFok ? p.amount : p.minAmount;
        if (minFill > 0 && !inPrecheckDone) {
            uint128 accum = inPrecheckAccum;
            uint256 lastPrice = inPrecheckLastPrice;
            optional(uint256, PriceLevel) pcIter;
            // Resume from where we left off (inPrecheckAccum > 0 means we've walked at least one level).
            if (inPrecheckAccum == 0) {
                pcIter = _bestOpposite(p.outcomeId, p.isBuy, p.epochId);
            } else {
                pcIter = _nextOpposite(p.outcomeId, p.isBuy, p.epochId, lastPrice);
            }
            uint32 walked = 0;
            bool reached = false;
            while (pcIter.hasValue()) {
                (uint256 pcLevelPrice, PriceLevel pcLvl) = pcIter.get();
                if (!_pricesCross(p.isBuy, storedPrice, isMarket, pcLevelPrice)) {
                    // No more crossing levels in our epoch.
                    break;
                }
                accum += pcLvl.totalAmount;
                lastPrice = pcLevelPrice;
                walked++;
                if (accum >= minFill) {
                    reached = true;
                    break;
                }
                if (walked >= MAX_PRECHECK_ITERATIONS) {
                    // Out of budget for this tx — yield and resume next pass.
                    return (true, orderId, p.amount, false, accum, lastPrice, takerFilledAccum);
                }
                pcIter = _nextOpposite(p.outcomeId, p.isBuy, p.epochId, pcLevelPrice);
            }
            if (!reached) {
                // Walked the full crossing region but didn't reach minFill → reject.
                uint128 returnAmt = _collateralFor(p.isBuy, storedPrice, p.amount);
                _cancelNoBookEntryTo(callerPn, callerHash, orderId, p.outcomeId, p.isBuy, returnAmt, p.clientOrderId);
                return (false, 0, 0, false, 0, 0, 0);
            }
            // Pre-check passed: fall through into the match loop.
        }

        // ── Match: walk levels best-first within our epoch, FIFO inside each ──
        uint128 remaining = p.amount;
        uint8 matchesDone = 0;
        bool hitCapWithMore = false;

        optional(uint256, PriceLevel) iter = _bestOpposite(p.outcomeId, p.isBuy, p.epochId);
        while (iter.hasValue() && remaining > 0) {
            (uint256 levelPrice, PriceLevel lvl) = iter.get();
            if (!_pricesCross(p.isBuy, storedPrice, isMarket, levelPrice)) break;

            uint128 cur = lvl.firstOrderId;
            while (cur != 0 && remaining > 0) {
                if (matchesDone >= MAX_MATCHES_PER_CALL) {
                    hitCapWithMore = true;
                    break;
                }
                Order cp = _orders[cur];
                uint128 nextOrd = cp.nextAtPrice;

                uint256 clearingPrice = levelPrice;

                // Compute trade (base units).
                //
                // LIMIT BUY / any SELL: remaining is already in base; trade = min.
                //
                // MARKET BUY: remaining is in QUOTE (lock = amount, not scaled
                //   by price). Cap trade by what the remaining quote can
                //   afford at this clearingPrice. Without this cap, an ask at
                //   clearingPrice > FULL_PERCENT produces actualCost >
                //   remaining and the seller's credited proceeds
                //   (= filled * clearingPrice / FULL_PERCENT) exceed the
                //   buyer's lock, inflating the sum of PN `_balance` across
                //   the two sides and letting the seller later drain pool
                //   share beyond the quote that physically entered the
                //   trade (breaks the conservation invariant vs
                //   RootPN.currencies / _deployedValues).
                uint128 trade;
                if (p.isBuy && isMarket) {
                    uint128 affordBase = clearingPrice > 0
                        ? uint128((uint256(remaining) * uint256(FULL_PERCENT)) / clearingPrice)
                        : cp.amount;
                    trade = cp.amount < affordBase ? cp.amount : affordBase;
                    if (trade == 0) {
                        // Remaining quote can't cover one base unit at this
                        // clearingPrice. Subsequent levels are worse (outer
                        // loop walks best-first) — stop matching entirely.
                        break;
                    }
                } else {
                    trade = remaining < cp.amount ? remaining : cp.amount;
                }

                // Quote actually spent this fill (market-buy only).
                uint128 spentQuote = 0;
                if (p.isBuy && isMarket) {
                    spentQuote = uint128((uint256(trade) * clearingPrice) / uint256(FULL_PERCENT));
                }

                uint128 newBuyerRefund = 0;
                if (p.isBuy) {
                    if (isMarket) {
                        // Market-buy: trade already capped by affordBase so
                        // spentQuote ≤ remaining. Buyer pays exactly
                        // spentQuote per fill → no per-fill refund. Unused
                        // quote at end of match is returned in bulk via the
                        // IOC/FOK/MARKET cancel-no-book branch below.
                        newBuyerRefund = 0;
                    } else {
                        uint256 diff = storedPrice > clearingPrice ? storedPrice - clearingPrice : 0;
                        newBuyerRefund = uint128((uint256(trade) * diff) / FULL_PERCENT);
                    }
                }
                uint128 cpBuyerRefund = 0;
                if (!p.isBuy) {
                    uint256 diff = cp.price > clearingPrice ? cp.price - clearingPrice : 0;
                    cpBuyerRefund = uint128((uint256(trade) * diff) / FULL_PERCENT);
                }

                // isFinal: true when this fill completes the order on that side.
                //   Maker:  resting order fully consumed  → cp.amount == trade.
                //   Taker:  for MARKET BUY, when spentQuote == remaining
                //           (all locked quote consumed by this fill).
                //           For LIMIT BUY / SELL, when remaining (base) ==
                //           trade (base) as before.
                bool makerFinal = (cp.amount == trade);
                bool takerFinal = (p.isBuy && isMarket) ? (spentQuote == remaining) : (remaining == trade);
                // Maker callback: maker's address differs per fill, must compute.
                _processFill(cp.depositHash, cur, p.outcomeId, trade, clearingPrice, cp.isBuy, cpBuyerRefund, false, makerFinal);
                // Taker callback: reuse cached caller address.
                _processFillTo(callerPn, orderId, p.outcomeId, trade, clearingPrice, p.isBuy, newBuyerRefund, true, takerFinal);

                // Maker-side aggregated MM event — emitted right at the fill
                // (maker is touched at most once per overall taker placement).
                if (makerFinal) {
                    _emitFullyFilled(cur, cp.clientOrderId, _orders[cur].filledAccum + trade);
                } else {
                    _emitPartialFill(cur, cp.clientOrderId, trade, cp.amount - trade);
                }

                // Track maker fill total on the resting order itself (kept on
                // the Order so future taker matches against the same maker
                // accumulate cleanly).
                _orders[cur].filledAccum += trade;
                takerFilledAccum += trade;

                if (cp.amount == trade) {
                    _removeFromBook(cur);
                } else {
                    _orders[cur].amount = cp.amount - trade;
                    _levels[p.outcomeId][cp.isBuy][cp.epochId][cp.price].totalAmount -= trade;
                }

                // Advance taker cursor in the taker's native unit:
                //   MARKET BUY → quote spent this fill.
                //   LIMIT BUY / SELL → base traded this fill.
                if (p.isBuy && isMarket) {
                    remaining = remaining > spentQuote ? remaining - spentQuote : 0;
                } else {
                    remaining -= trade;
                }
                matchesDone++;
                cur = nextOrd;
            }
            if (hitCapWithMore) break;

            // Move to next level in our epoch (worse price). Re-fetch since storage may have changed.
            iter = _nextOpposite(p.outcomeId, p.isBuy, p.epochId, levelPrice);
        }

        if (hitCapWithMore) {
            // Match continuation: pre-check is done; carry over remaining + orderId + accum.
            return (true, orderId, remaining, true, 0, 0, takerFilledAccum);
        }

        // No post-match minAmount check: the FOK / minAmount pre-check above
        // already verified totalAvailable >= minFill against the crossing price
        // levels in our epoch. Queue serialization (we hold the head across
        // continuations, no other place/cancel runs in between) guarantees that
        // the available liquidity does not shrink before we consume it. Levels
        // are now keyed by epochId, so foreign-epoch orders cannot inflate
        // totalAmount nor force phantom-skip walks.

        if (remaining > 0) {
            if (isIoc || isFok || isMarket) {
                uint128 returnAmt = (isMarket && p.isBuy) ? remaining : _collateralFor(p.isBuy, storedPrice, remaining);
                _cancelNoBookEntryTo(callerPn, callerHash, orderId, p.outcomeId, p.isBuy, returnAmt, p.clientOrderId);
            } else {
                _insertIntoBook(orderId, callerHash, p, storedPrice, remaining, takerFilledAccum);
            }
        }

        // Taker-side aggregated MM event — emitted ONCE after all matching
        // (across continuations) is done. PartialFill if leftover remains
        // (book or cancelled); FullyFilled if the order was fully consumed.
        if (takerFilledAccum > 0) {
            if (remaining == 0) {
                _emitFullyFilled(orderId, p.clientOrderId, takerFilledAccum);
            } else {
                _emitPartialFill(orderId, p.clientOrderId, takerFilledAccum, remaining);
            }
        }
        // Taker fully consumed without resting → release any cid reservation
        // we made in executeBatch (no _insertIntoBook / _cancelNoBookEntryTo
        // path took care of it).
        if (remaining == 0 && p.clientOrderId != 0) {
            delete _clientOrderIds[callerHash][p.clientOrderId];
        }
        return (false, 0, 0, false, 0, 0, 0);
    }

    function _doCancel(uint256 callerHash, uint128 orderId) private {
        Order o = _orders[orderId];
        if (o.amount == 0 && o.depositHash == 0) return;
        if (o.depositHash != callerHash) return;
        uint128 returnAmt = _collateralFor(o.isBuy, o.price, o.amount);
        uint32 outcomeId = o.outcomeId;
        bool isBuy = o.isBuy;
        uint128 cid = o.clientOrderId;
        _removeFromBook(orderId);  // also frees _clientOrderIds[depositHash][cid]
        _emitOrderCancelled(orderId, cid);
        _notifyOrderCancelled(callerHash, orderId, outcomeId, isBuy, returnAmt);
    }

    function _doCancelAll(uint256 callerHash) private returns (uint32 cancelled) {
        cancelled = 0;
        uint128 cur = _ownerHead[callerHash];
        while (cur != 0 && cancelled < MAX_CANCEL_ALL_PER_CALL) {
            Order o = _orders[cur];
            uint128 next = o.nextInOwner;
            uint128 returnAmt = _collateralFor(o.isBuy, o.price, o.amount);
            uint32 outcomeId = o.outcomeId;
            bool isBuy = o.isBuy;
            uint128 cid = o.clientOrderId;
            _removeFromBook(cur);  // frees cid mapping
            _emitOrderCancelled(cur, cid);
            _notifyOrderCancelled(callerHash, cur, outcomeId, isBuy, returnAmt);
            cancelled++;
            cur = next;
        }
    }

    // ===== Level navigation =====

    /// @notice Best price level on the opposite side within the taker's epoch
    ///         (asks for buyers, bids for sellers). Levels of other epochs are
    ///         in disjoint storage subtrees and are never visited.
    function _bestOpposite(uint32 outcomeId, bool newIsBuy, uint64 epochId) private view returns (optional(uint256, PriceLevel)) {
        if (newIsBuy) {
            return _levels[outcomeId][false][epochId].min();
        }
        return _levels[outcomeId][true][epochId].max();
    }

    /// @notice Next price level on the opposite side after `currentPrice` within
    ///         the taker's epoch (worse direction).
    function _nextOpposite(uint32 outcomeId, bool newIsBuy, uint64 epochId, uint256 currentPrice) private view returns (optional(uint256, PriceLevel)) {
        if (newIsBuy) {
            return _levels[outcomeId][false][epochId].next(currentPrice);
        }
        return _levels[outcomeId][true][epochId].prev(currentPrice);
    }

    function _pricesCross(
        bool newIsBuy, uint256 newPrice, bool newIsMarket, uint256 cpPrice
    ) private pure returns (bool) {
        if (newIsMarket) return true;
        if (newIsBuy) return newPrice >= cpPrice;
        return cpPrice >= newPrice;
    }

    function _collateralFor(bool isBuy, uint256 price, uint128 amount) private pure returns (uint128) {
        if (isBuy) {
            return uint128((uint256(amount) * price) / uint256(FULL_PERCENT));
        }
        return amount;
    }

    // ===== Book mutation =====

    function _insertIntoBook(
        uint128 orderId,
        uint256 callerHash,
        PlaceParams p,
        uint256 storedPrice,
        uint128 amount,
        uint128 priorFilledAccum
    ) private {
        uint128 oTail = _ownerTail[callerHash];
        PriceLevel level = _levels[p.outcomeId][p.isBuy][p.epochId][storedPrice];
        uint128 priceTail = level.lastOrderId;

        _orders[orderId] = Order({
            depositHash: callerHash,
            price: storedPrice,
            amount: amount,
            minAmount: 0,
            initialAmount: p.amount,
            filledAccum: priorFilledAccum,
            clientOrderId: p.clientOrderId,
            epochId: p.epochId,
            outcomeId: p.outcomeId,
            flags: p.flags,
            isBuy: p.isBuy,
            nextAtPrice: 0,
            prevAtPrice: priceTail,
            nextInOwner: 0,
            prevInOwner: oTail
        });

        // Bind the cid to the real orderId now that we have one (replaces the
        // sentinel set at executeBatch validation time).
        if (p.clientOrderId != 0) {
            _clientOrderIds[callerHash][p.clientOrderId] = orderId;
        }

        if (priceTail == 0) {
            // First order at this (epoch, price) level
            _levels[p.outcomeId][p.isBuy][p.epochId][storedPrice] = PriceLevel({
                firstOrderId: orderId,
                lastOrderId: orderId,
                totalAmount: amount
            });
        } else {
            _orders[priceTail].nextAtPrice = orderId;
            level.lastOrderId = orderId;
            level.totalAmount += amount;
            _levels[p.outcomeId][p.isBuy][p.epochId][storedPrice] = level;
        }

        if (oTail == 0) {
            _ownerHead[callerHash] = orderId;
        } else {
            _orders[oTail].nextInOwner = orderId;
        }
        _ownerTail[callerHash] = orderId;

        _orderCount++;
    }

    function _removeFromBook(uint128 orderId) private {
        Order o = _orders[orderId];

        // Free the per-PN clientOrderId slot now that the order is leaving
        // the book (fully filled / cancelled / shutdown).
        if (o.clientOrderId != 0) {
            delete _clientOrderIds[o.depositHash][o.clientOrderId];
        }

        // Price-level FIFO (within order's epoch)
        uint128 prevP = o.prevAtPrice;
        uint128 nextP = o.nextAtPrice;
        PriceLevel level = _levels[o.outcomeId][o.isBuy][o.epochId][o.price];

        if (prevP == 0) {
            level.firstOrderId = nextP;
        } else {
            _orders[prevP].nextAtPrice = nextP;
        }
        if (nextP == 0) {
            level.lastOrderId = prevP;
        } else {
            _orders[nextP].prevAtPrice = prevP;
        }
        if (level.totalAmount >= o.amount) {
            level.totalAmount -= o.amount;
        } else {
            level.totalAmount = 0;
        }

        if (level.firstOrderId == 0) {
            delete _levels[o.outcomeId][o.isBuy][o.epochId][o.price];
        } else {
            _levels[o.outcomeId][o.isBuy][o.epochId][o.price] = level;
        }

        // Owner FIFO
        uint128 oPrev = o.prevInOwner;
        uint128 oNext = o.nextInOwner;
        if (oPrev == 0) {
            _ownerHead[o.depositHash] = oNext;
        } else {
            _orders[oPrev].nextInOwner = oNext;
        }
        if (oNext == 0) {
            _ownerTail[o.depositHash] = oPrev;
        } else {
            _orders[oNext].prevInOwner = oPrev;
        }

        delete _orders[orderId];
        _orderCount--;
    }

    // ===== Effect emit & callbacks =====

    /// @notice Variant that takes the resolved PN address directly. Used by
    ///         _doPlace to avoid recomputing the caller's PN address per fill.
    function _emitOrderPlacedTo(address pn, uint128 orderId, PlaceParams p) private view {
        uint128 feeReserve = 0;
        uint128 lock = 0;
        if (p.isBuy) {
            uint128 cost = (p.flags & FLAG_MARKET != 0)
                ? p.amount
                : uint128((uint256(p.amount) * p.price) / uint256(FULL_PERCENT));
            feeReserve = uint128((uint256(cost) * uint256(TAKER_FEE_RATE)) / uint256(FEE_DENOMINATOR));
            // Full buy-side lock = cost + feeReserve. Authoritative value so
            // PN can track per-order lock and drain floor-accumulated
            // residuals back to `_balance` on isFinal fill or cancel.
            lock = cost + feeReserve;
        }
        address addrExtern = address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress);
        emit OrderPlaced{dest: addrExtern}(orderId, p.outcomeId, p.isBuy, p.flags, p.price, p.amount, p.clientOrderId);
        PrivateNote(pn).onOrderPlaced{
            value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_eventId, _oracleListHash, _tokenType, orderId, feeReserve, lock);
    }

    function _emitOrderCancelled(uint128 orderId, uint128 clientOrderId) private pure {
        address addrExtern = address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress);
        emit OrderCancelled{dest: addrExtern}(orderId, clientOrderId);
    }

    function _notifyOrderCancelled(
        uint256 callerHash, uint128 orderId, uint32 outcomeId, bool isBuy, uint128 returnAmt
    ) private view {
        address pn = DexLib.computePrivateNoteAddress(_privateNoteCode, callerHash);
        _notifyOrderCancelledTo(pn, orderId, outcomeId, isBuy, returnAmt);
    }

    function _notifyOrderCancelledTo(
        address pn, uint128 orderId, uint32 outcomeId, bool isBuy, uint128 returnAmt
    ) private view {
        PrivateNote(pn).onOrderCancelled{
            value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_eventId, _oracleListHash, _tokenType, orderId, outcomeId, isBuy, returnAmt);
    }

    function _cancelNoBookEntryTo(
        address pn, uint256 callerHash, uint128 orderId, uint32 outcomeId, bool isBuy,
        uint128 returnAmt, uint128 clientOrderId
    ) private {
        if (clientOrderId != 0) {
            delete _clientOrderIds[callerHash][clientOrderId];
        }
        _emitOrderCancelled(orderId, clientOrderId);
        _notifyOrderCancelledTo(pn, orderId, outcomeId, isBuy, returnAmt);
    }

    /// @notice Aggregated fill events for MM-friendly subscribers. Emitted
    ///         once per order completion (PartialFill on any leftover,
    ///         FullyFilled on full consumption). Per-fill `OrderFilled`
    ///         continues to fire alongside for raw analytics.
    function _emitPartialFill(uint128 orderId, uint128 clientOrderId, uint128 filledAmount, uint128 remainingAmount) private pure {
        address addrExtern = address.makeAddrExtern(0, bitCntAddress);
        emit PartialFill{dest: addrExtern}(orderId, clientOrderId, filledAmount, remainingAmount);
    }

    function _emitFullyFilled(uint128 orderId, uint128 clientOrderId, uint128 filledAmount) private pure {
        address addrExtern = address.makeAddrExtern(0, bitCntAddress);
        emit FullyFilled{dest: addrExtern}(orderId, clientOrderId, filledAmount);
    }

    function _processFill(
        uint256 pnHash,
        uint128 orderId,
        uint32  outcomeId,
        uint128 filledAmount,
        uint256 clearingPrice,
        bool    isBuy,
        uint128 buyerRefund,
        bool    isTaker,
        bool    isFinal
    ) private {
        address pn = DexLib.computePrivateNoteAddress(_privateNoteCode, pnHash);
        _processFillTo(pn, orderId, outcomeId, filledAmount, clearingPrice, isBuy, buyerRefund, isTaker, isFinal);
    }

    function _processFillTo(
        address pn,
        uint128 orderId,
        uint32  outcomeId,
        uint128 filledAmount,
        uint256 clearingPrice,
        bool    isBuy,
        uint128 buyerRefund,
        bool    isTaker,
        bool    isFinal
    ) private {
        uint128 feeRate = isTaker ? TAKER_FEE_RATE : MAKER_FEE_RATE;
        uint128 notional = uint128(
            (uint256(filledAmount) * clearingPrice) / uint256(FULL_PERCENT)
        );
        uint128 feeAmount = uint128(
            (uint256(notional) * uint256(feeRate)) / uint256(FEE_DENOMINATOR)
        );
        if (isTaker) {
            _totalTakerFees += feeAmount;
        } else {
            _totalMakerFees += feeAmount;
        }

        address addrExtern = address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress);
        emit OrderFilled{dest: addrExtern}(orderId, filledAmount, clearingPrice, feeAmount, isTaker);

        PrivateNote(pn).onOrderFilled{
            value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_eventId, _oracleListHash, _tokenType, outcomeId, filledAmount, clearingPrice, isBuy, buyerRefund, feeAmount, orderId, isFinal);
    }

    // ===== Getters =====

    function getDetails() external view returns (
        uint256 eventId,
        uint256 oracleListHash,
        uint32  tokenType,
        uint128 nextOrderId,
        uint128 orderCount,
        uint128 totalMakerFees,
        uint128 totalTakerFees
    ) {
        return (
            _eventId,
            _oracleListHash,
            _tokenType,
            _nextOrderId,
            uint128(_orderCount),
            _totalMakerFees,
            _totalTakerFees
        );
    }

    function getQueueSize() external view returns (uint8 size) {
        return _queueSize;
    }

    function getOrder(uint128 orderId) external view returns (
        uint256 depositIdentifierHash,
        uint32  outcomeId,
        bool    isBuy,
        uint8   flags,
        uint256 price,
        uint128 amount,
        uint128 minAmount,
        uint64  epochId
    ) {
        Order o = _orders[orderId];
        if (o.amount == 0 && o.depositHash == 0) revert(ERR_ORDER_NOT_FOUND);
        return (
            o.depositHash, o.outcomeId, o.isBuy, o.flags,
            o.price, o.amount, o.minAmount, o.epochId
        );
    }

    function getOrdersByOwner(uint256 depositHash) external view returns (
        uint128[] orderIds,
        uint32[]  outcomeIds,
        bool[]    isBuys,
        uint256[] prices,
        uint128[] amounts,
        uint64[]  epochIds,
        uint128[] clientOrderIds
    ) {
        uint128 cur = _ownerHead[depositHash];
        while (cur != 0) {
            Order o = _orders[cur];
            orderIds.push(cur);
            outcomeIds.push(o.outcomeId);
            isBuys.push(o.isBuy);
            prices.push(o.price);
            amounts.push(o.amount);
            epochIds.push(o.epochId);
            clientOrderIds.push(o.clientOrderId);
            cur = o.nextInOwner;
        }
    }

    function getOrderIdByClient(uint256 depositHash, uint128 clientOrderId) external view returns (uint128 orderId) {
        orderId = _clientOrderIds[depositHash][clientOrderId];
    }

    function cancelByClientId(uint256 depositIdentifierHash, uint128 clientOrderId) public {
        require(!_shuttingDown, ERR_ALREADY_CANCELLED);
        address wallet = DexLib.computePrivateNoteAddress(_privateNoteCode, depositIdentifierHash);
        require(msg.sender == wallet, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();
        uint128 orderId = _clientOrderIds[depositIdentifierHash][clientOrderId];
        if (orderId == 0) return;
        _doCancel(depositIdentifierHash, orderId);
    }

    // ===== Shutdown =====

    function shutdown() public {
        require(msg.sender == _pmpAddress || msg.sender == address(this), ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();

        // Latch shutdown on the first call so no new place/cancel may slip in
        // while the contract is draining its order map across self-call passes.
        _shuttingDown = true;

        uint8 cancelled = 0;
        uint128[] toCancel;
        uint128 i = _shutdownCursor == 0 ? 1 : _shutdownCursor;
        while (i < _nextOrderId && cancelled < MAX_SHUTDOWN_BATCH) {
            if (_orders[i].amount > 0) {
                toCancel.push(i);
                cancelled++;
            }
            i++;
        }
        _shutdownCursor = i;

        for (uint k = 0; k < toCancel.length; k++) {
            uint128 oid = toCancel[k];
            Order o = _orders[oid];
            uint128 returnAmt = _collateralFor(o.isBuy, o.price, o.amount);
            uint256 pnHash = o.depositHash;
            uint32 outcomeId = o.outcomeId;
            bool isBuy = o.isBuy;
            uint128 cid = o.clientOrderId;
            _removeFromBook(oid);  // frees cid mapping
            _emitOrderCancelled(oid, cid);
            _notifyOrderCancelled(pnHash, oid, outcomeId, isBuy, returnAmt);
        }

        if (_orderCount > 0) {
            OrderBook(address(this)).shutdown{
                value: 1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
            }();
        } else {
            selfdestruct(ROOT_PN_ADDRESS);
        }
    }

    /// @notice Observability hook for bounced outgoing callbacks. We deliberately
    ///         don't try to auto-recover state here — order/queue mutations done
    ///         by the dispatch are committed and a bounce arrives in a later block.
    ///         External monitors must reconcile the affected PN. Without this hook
    ///         the bounce would silently be a no-op; here it surfaces as an event.
    onBounce(TvmSlice body) external pure {
        body;
        address addrExtern = address.makeAddrExtern(0, bitCntAddress);
        emit CallbackBounced{dest: addrExtern}(msg.sender, tx.logicaltime);
    }

    function getVersion() external pure returns (string, string) {
        return (version, "OrderBook");
    }
}
