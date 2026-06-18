pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./interfaces.sol";
// Deploy guard only: the real PrivateNote type gives the stateInit data layout so
// we can recompute a note's address from its pinned CODE HASH.
import "../dex/PrivateNote.sol";

/// @notice Handover: the matched seller's `token_contract` receives the deal's
///         SHELL and binds the buyer note (spec §2.3).
interface ITokenContractDeal {
    function fundFromOrderBook(address buyerNote) external;
}

/// @notice Async pull sink (spec §6.2): the weekly median is handed back here.
interface IWeeklyMedianSink {
    function onWeeklyMedian(uint256 eventId, uint256 oracleListHash, uint32 tokenType, uint256 price) external;
}

/// @title InferenceOrderBook (spec §2 + §8) — full price→time CLOB with a queued,
///        resumable matching engine (ported from dex/OrderBook.sol).
/// @notice Both sides rest (SELL offer = one-deal slot; BUY = §8.1 limit/subscription
///         with SHELL budget held here). Order entry goes through a circular QUEUE:
///         each op is enqueued and `processHead` drains it one-at-a-time, self-calling
///         across txs. Matching that crosses more than `MAX_MATCHES_PER_CALL` resting
///         orders YIELDS a continuation cursor on the head entry and resumes next tx —
///         so large orders fully take and FOK never partial-fills. The queue serializes
///         all ops, so liquidity can't shift under an in-flight taker.
///
///         On each fill the matched SHELL (trade·(clearing+fee)) is forwarded to the
///         SELL side's token_contract with the BUY side's note bound (§2.3) → the deal
///         streams off-book (§3). The book also keeps a daily-VWAP / weekly-median
///         reference price (§6/§7).
///
///         Dropped vs dex OrderBook (irrelevant to inference): outcomes, epochs,
///         quote/base + collateral, event-resolution shutdown, and PN callbacks
///         (inference order entry is fire-and-forget; the note tracks via getters).
contract InferenceOrderBook is AiRegistryModifiers {
    string constant version = "4.0.0";

    // ⚠ Re-pin whenever dex/PrivateNote is recompiled (note↔OB layout coupling:
    //   the note bakes this book's state layout via `new InferenceOrderBook`, so any
    //   OB layout change forces a note rebuild → new note hash → re-pin → OB rebuild).
    uint256 constant NOTE_CODE_HASH  = 0x8df6f988e99b973c1d90362bf1e03f9e6992861cd5133d46ec6d600018f5d0ed;
    uint16  constant NOTE_CODE_DEPTH = 19;

    // Local errors (NOT in shared AiRegistryErrors — avoids rippling RootModel/TC/SuperRoot pins).
    uint16 constant ERR_NOT_DEPLOYER_NOTE = 333;
    uint16 constant ERR_NO_LIQUIDITY      = 334;
    uint16 constant ERR_BAD_FLAGS         = 335;
    uint16 constant ERR_EXPIRED           = 336;
    uint16 constant ERR_FOK_UNFILLED      = 337;
    uint16 constant ERR_NOT_SUB           = 338;
    uint16 constant ERR_NOTHING_TO_CLAIM  = 339;
    uint16 constant ERR_QUEUE_FULL        = 340;
    uint16 constant ERR_NOT_SELF          = 341;

    // Order-type flags (spec §8 / dex parity).
    uint8 constant FLAG_IOC       = 0x01;
    uint8 constant FLAG_FOK       = 0x02;
    uint8 constant FLAG_MARKET    = 0x04;
    uint8 constant FLAG_POST_ONLY = 0x08;
    uint8 constant TAKER_FLAGS    = 0x07;

    // Gas / walk caps.
    uint8  constant MAX_MATCHES_PER_CALL = 30;
    uint8  constant MAX_CANCEL_PER_CALL  = 30;
    uint8  constant MAX_PRECHECK_LEVELS  = 40;

    // Queue (circular).
    uint8 constant QENTRY_PLACE      = 1;
    uint8 constant QENTRY_CANCEL     = 2;
    uint8 constant QENTRY_CANCEL_ALL = 3;
    uint8 constant QUEUE_CAPACITY    = 100;
    uint8 constant QUEUE_PLACE_LIMIT = 90;

    // §8 subscription.
    uint64 constant SUB_PERIOD    = 2_419_200;   // ≈ month (4 weeks)
    uint8  constant SUB_CYCLES    = 4;
    uint64 constant SUB_CYCLE_LEN = 604_800;     // 1 week

    // Reference-price stats.
    uint64  constant SECS_PER_DAY  = 86400;
    uint128 constant MIN_LIQUIDITY = 1;

    // Statics — address derivation: book = (model, tick).
    uint256 static _modelHash;
    uint128 static _tickSize;

    // ── Order book ──
    struct Order {
        address note;           // owner note; cancel auth + handover
        address tokenContract;  // SELL: seller's deal contract; BUY: 0
        uint256 price;          // price per tick P
        uint128 amount;         // remaining ticks
        uint128 initialAmount;
        uint128 filledAccum;
        uint128 escrow;         // BUY: SHELL budget held; SELL: 0
        uint64  deadline;       // BUY GTD (0 = GTC)
        uint64  ts;
        uint8   flags;
        bool    isBuy;
        uint128 nextAtPrice;
        uint128 prevAtPrice;
        uint128 nextInOwner;
        uint128 prevInOwner;
    }
    mapping(uint128 => Order) _orders;
    uint128 _nextOrderId;
    uint128 _orderCount;

    struct PriceLevel { uint128 firstOrderId; uint128 lastOrderId; uint128 totalAmount; }
    mapping(bool => mapping(uint256 => PriceLevel)) _levels;   // isBuy → price → level

    mapping(address => uint128) _ownerHead;
    mapping(address => uint128) _ownerTail;

    uint128 _executedNotional;
    uint128 _executedTicks;
    uint64  _matchSeq;

    // ── Queue ──
    struct QueueEntry {
        uint8   entryType;
        address owner;          // the note (msg.sender at entry)
        bool    isBuy;
        uint8   flags;
        uint256 price;          // stored price (market: 0 sell / max buy)
        uint128 amount;         // original ticks
        uint128 escrow;         // BUY budget received; SELL 0
        address tokenContract;  // SELL TC
        uint64  deadline;
        uint128 targetOrderId;  // CANCEL target
        // Match continuation cursor (BUY taker crossing > one tx of liquidity):
        uint128 contOrderId;    // assigned on first run; 0 = not started
        uint128 contRemaining;
        uint128 contLeftover;
    }
    mapping(uint8 => QueueEntry) _queue;
    uint8 _queueHead;
    uint8 _queueTail;
    uint8 _queueSize;

    // ── Reference-price stats ──
    struct DayData { uint64 day; uint256 priceVolSum; uint256 volSum; }
    mapping(uint8 => DayData) _daily;
    uint8 _curSlot;

    // ── §8 subscription state (keyed by resting BUY order id) ──
    struct Sub {
        uint64  periodStart;
        uint8   curCycle;
        uint128 cycleBudget;
        uint128 cycleSpent;
        bool    autoRenew;
        bool    exists;
    }
    mapping(uint128 => Sub) _subs;
    mapping(uint128 => mapping(uint8 => uint128)) _forfeitPool;
    mapping(uint128 => mapping(uint8 => uint128)) _cycleFundedTicks;
    mapping(uint128 => mapping(uint8 => mapping(address => uint128))) _cycleSellerTicks;

    address _deployerNote;

    event OrderPlaced(uint128 orderId, bool isBuy, uint256 price, uint128 ticks, address note, address tokenContract, uint64 deadline);
    event OrderCancelled(uint128 orderId, uint128 refundedShell);
    event Filled(uint128 makerId, uint128 takerId, uint128 ticks, uint256 clearingPrice, address sellerTC, address buyerNote);
    event Executed(uint128 ticks, uint256 clearingPrice, uint128 cost);
    event Refunded(address note, uint128 amount);
    event SubscriptionPlaced(uint128 orderId, address buyerNote, uint128 maxPrice, uint128 ticks, uint128 cycleBudget, bool autoRenew);
    event CycleForfeited(uint128 orderId, uint8 cycle, uint128 forfeited, uint128 fundedTicks);
    event ForfeitClaimed(uint128 orderId, uint8 cycle, address sellerNote, uint128 amount);

    // ========================================================
    // Deploy guard (spec §2/§8)
    // ========================================================

    function _noteAddrFromHash(uint256 depositHash) private returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: PrivateNote, code: dummyCode,
            varInit: { _depositIdentifierHash: depositHash }
        });
        TvmSlice s = si.toSlice();
        s.skip(5);
        s.loadRef();
        TvmCell dataCell = s.loadRef();
        return address.makeAddrStd(
            0, abi.stateInitHash(NOTE_CODE_HASH, tvm.hash(dataCell), NOTE_CODE_DEPTH, dataCell.depth()));
    }

    constructor(uint256 depositHash) {
        require(msg.sender == _noteAddrFromHash(depositHash), ERR_NOT_DEPLOYER_NOTE);
        tvm.accept();
        _deployerNote = msg.sender;
        _nextOrderId = 1;
        ensureBalance();
    }

    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) { return; }
        gosh.mintshellq(MIN_BALANCE);
    }

    function _payShell(address to, uint128 amount) private pure {
        if (amount == 0) { return; }
        mapping(uint32 => varuint32) ecc;
        ecc[SHELL_ECC_ID] = varuint32(amount);
        to.transfer({value: 1 vmshell, bounce: false, flag: 1, currencies: ecc});
    }

    function _tickFee(uint256 p) private pure returns (uint256) {
        return (p * uint256(PLATFORM_FEE_BPS)) / uint256(BPS_DENOMINATOR);
    }
    function _unit(uint256 p) private pure returns (uint256) { return p + _tickFee(p); }

    // ========================================================
    // Level navigation
    // ========================================================

    function _bestOpposite(bool takerIsBuy) private view returns (optional(uint256, PriceLevel)) {
        return takerIsBuy ? _levels[false].min() : _levels[true].max();
    }
    function _nextOpposite(bool takerIsBuy, uint256 cur) private view returns (optional(uint256, PriceLevel)) {
        return takerIsBuy ? _levels[false].next(cur) : _levels[true].prev(cur);
    }
    function _crosses(bool takerIsBuy, uint256 takerPrice, bool isMarket, uint256 makerPrice) private pure returns (bool) {
        if (isMarket) { return true; }
        return takerIsBuy ? (takerPrice >= makerPrice) : (makerPrice >= takerPrice);
    }

    // ========================================================
    // Book mutation
    // ========================================================

    function _insertIntoBook(uint128 orderId, Order o) private {
        uint128 oTail = _ownerTail[o.note];
        PriceLevel level = _levels[o.isBuy][o.price];
        uint128 priceTail = level.lastOrderId;

        o.nextAtPrice = 0; o.prevAtPrice = priceTail; o.nextInOwner = 0; o.prevInOwner = oTail;
        _orders[orderId] = o;

        if (priceTail == 0) {
            _levels[o.isBuy][o.price] = PriceLevel({firstOrderId: orderId, lastOrderId: orderId, totalAmount: o.amount});
        } else {
            _orders[priceTail].nextAtPrice = orderId;
            level.lastOrderId = orderId;
            level.totalAmount += o.amount;
            _levels[o.isBuy][o.price] = level;
        }
        if (oTail == 0) { _ownerHead[o.note] = orderId; } else { _orders[oTail].nextInOwner = orderId; }
        _ownerTail[o.note] = orderId;
        _orderCount++;
    }

    function _removeFromBook(uint128 orderId) private {
        Order o = _orders[orderId];
        uint128 prevP = o.prevAtPrice;
        uint128 nextP = o.nextAtPrice;
        PriceLevel level = _levels[o.isBuy][o.price];
        if (prevP == 0) { level.firstOrderId = nextP; } else { _orders[prevP].nextAtPrice = nextP; }
        if (nextP == 0) { level.lastOrderId = prevP; } else { _orders[nextP].prevAtPrice = prevP; }
        if (level.totalAmount >= o.amount) { level.totalAmount -= o.amount; } else { level.totalAmount = 0; }
        if (level.firstOrderId == 0) { delete _levels[o.isBuy][o.price]; } else { _levels[o.isBuy][o.price] = level; }

        uint128 oPrev = o.prevInOwner;
        uint128 oNext = o.nextInOwner;
        if (oPrev == 0) { _ownerHead[o.note] = oNext; } else { _orders[oPrev].nextInOwner = oNext; }
        if (oNext == 0) { _ownerTail[o.note] = oPrev; } else { _orders[oNext].prevInOwner = oPrev; }

        delete _orders[orderId];
        _orderCount--;
    }

    // ========================================================
    // Fill settlement (spec §2.3 → §3)
    // ========================================================

    function _settleFill(uint128 makerId, uint128 takerId, address buyerNote, address sellerTC, uint128 trade, uint256 clearing) private returns (uint128) {
        uint128 cost = uint128(uint256(trade) * _unit(clearing));
        mapping(uint32 => varuint32) ecc;
        ecc[SHELL_ECC_ID] = varuint32(cost);
        ITokenContractDeal(sellerTC).fundFromOrderBook{
            value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false, currencies: ecc
        }(buyerNote);
        _executedNotional += uint128(uint256(trade) * clearing);
        _executedTicks    += trade;
        _matchSeq += 1;
        _recordTrade(uint128(clearing), trade);
        emit Filled{dest: address.makeAddrExtern(MatchedEmit, bitCntAddress)}(makerId, takerId, trade, clearing, sellerTC, buyerNote);
        emit Executed{dest: address.makeAddrExtern(ExecutedEmit, bitCntAddress)}(trade, clearing, cost);
        return cost;
    }

    function _enoughLiquidity(bool takerIsBuy, uint256 takerPrice, bool isMarket, uint128 need) private view returns (bool) {
        uint128 accum = 0;
        uint8 walked = 0;
        optional(uint256, PriceLevel) it = _bestOpposite(takerIsBuy);
        while (it.hasValue()) {
            (uint256 lp, PriceLevel lvl) = it.get();
            if (!_crosses(takerIsBuy, takerPrice, isMarket, lp)) { break; }
            accum += lvl.totalAmount;
            if (accum >= need) { return true; }
            walked++;
            if (walked >= MAX_PRECHECK_LEVELS) { break; }
            it = _nextOpposite(takerIsBuy, lp);
        }
        return accum >= need;
    }

    // ========================================================
    // Matching engine (best-first, price→time, bounded; resumes via the queue)
    // ========================================================

    /// @return remaining unfilled ticks, leftoverEscrow, capped (true = hit the
    ///         per-tx cap with crossing liquidity left → caller continues).
    function _match(
        uint128 takerId, bool takerIsBuy, uint256 takerPrice, bool isMarket,
        address takerNote, address takerTC, uint128 amount, uint128 buyEscrow
    ) private returns (uint128 remaining, uint128 leftoverEscrow, bool capped) {
        remaining = amount;
        leftoverEscrow = buyEscrow;
        capped = false;
        uint8 matches = 0;

        optional(uint256, PriceLevel) it = _bestOpposite(takerIsBuy);
        while (it.hasValue() && remaining > 0) {
            (uint256 lp, ) = it.get();
            if (!_crosses(takerIsBuy, takerPrice, isMarket, lp)) { break; }

            uint128 cur = _levels[!takerIsBuy][lp].firstOrderId;
            while (cur != 0 && remaining > 0) {
                if (matches >= MAX_MATCHES_PER_CALL) { return (remaining, leftoverEscrow, true); }
                Order mk = _orders[cur];
                uint128 nextOrd = mk.nextAtPrice;
                uint256 clearing = lp;

                // §8 subscription maker (resting buy): roll cycles + cap by cycle budget.
                bool makerSub = (!takerIsBuy) && _subs[cur].exists;
                if (makerSub) {
                    _subTouch(cur);
                    if (!_subs[cur].exists) { cur = nextOrd; continue; }   // expired
                    mk = _orders[cur];
                    nextOrd = mk.nextAtPrice;
                }

                uint128 trade = remaining < mk.amount ? remaining : mk.amount;

                address buyerNote;
                address sellerTC;
                if (takerIsBuy) { buyerNote = takerNote; sellerTC = mk.tokenContract; }
                else            { buyerNote = mk.note;   sellerTC = takerTC; }

                uint256 unit = _unit(clearing);
                uint128 budget = takerIsBuy ? leftoverEscrow : mk.escrow;
                if (makerSub) {
                    Sub s = _subs[cur];
                    uint128 cycRem = s.cycleBudget > s.cycleSpent ? s.cycleBudget - s.cycleSpent : 0;
                    if (cycRem < budget) { budget = cycRem; }
                }
                uint128 afford = unit > 0 ? uint128(uint256(budget) / unit) : trade;
                if (afford < trade) { trade = afford; }
                if (trade == 0) {
                    if (takerIsBuy) { return (remaining, leftoverEscrow, false); }
                    if (makerSub) { cur = nextOrd; continue; }   // cycle budget spent; keep for next cycle
                    _refundAndRemove(cur);
                    cur = nextOrd;
                    continue;
                }

                uint128 cost = _settleFill(takerIsBuy ? cur : takerId, takerIsBuy ? takerId : cur, buyerNote, sellerTC, trade, clearing);

                if (takerIsBuy) { leftoverEscrow -= cost; } else { _orders[cur].escrow = mk.escrow - cost; }

                if (makerSub) {
                    _subs[cur].cycleSpent += cost;
                    uint8 cy = _subs[cur].curCycle;
                    _cycleFundedTicks[cur][cy] += trade;
                    _cycleSellerTicks[cur][cy][takerNote] += trade;   // takerNote = seller note
                }

                _orders[cur].filledAccum += trade;
                // SELL offer = one-deal slot → consumed on match (taker BUY), even
                // on partial. BUY maker (taker SELL) is reduced (spans deals).
                if (takerIsBuy || mk.amount == trade) { _removeFromBook(cur); }
                else {
                    _orders[cur].amount = mk.amount - trade;
                    _levels[!takerIsBuy][lp].totalAmount -= trade;
                }

                remaining -= trade;
                matches++;
                cur = nextOrd;

                // Taker SELL is itself one deal → one fill, then consumed.
                if (!takerIsBuy) { remaining = 0; break; }
            }
            it = _nextOpposite(takerIsBuy, lp);
        }
        return (remaining, leftoverEscrow, false);
    }

    function _refundAndRemove(uint128 orderId) private {
        Order o = _orders[orderId];
        uint128 refund = o.escrow;
        _removeFromBook(orderId);
        if (refund > 0) { _payShell(o.note, refund); emit Refunded{dest: address.makeAddrExtern(BuyUnmatchedEmit, bitCntAddress)}(o.note, refund); }
    }

    /// @notice Rest leftover (limit) or refund (taker-only / market) after a match completes.
    function _finalizeTaker(
        uint128 orderId, bool isBuy, uint256 storedPrice, address note, address tc,
        uint128 remaining, uint128 leftover, uint128 initialAmount, uint8 flags, uint64 deadline
    ) private {
        bool takerOnly = (flags & FLAG_MARKET) != 0 || (flags & (FLAG_IOC | FLAG_FOK)) != 0;
        if (isBuy) {
            if (remaining == 0 || takerOnly) {
                if (leftover > 0) { _payShell(note, leftover); emit Refunded{dest: address.makeAddrExtern(BuyUnmatchedEmit, bitCntAddress)}(note, leftover); }
                return;
            }
            _insertIntoBook(orderId, Order({
                note: note, tokenContract: address(0), price: storedPrice,
                amount: remaining, initialAmount: initialAmount, filledAccum: initialAmount - remaining,
                escrow: leftover, deadline: deadline, ts: uint64(block.timestamp),
                flags: flags, isBuy: true,
                nextAtPrice: 0, prevAtPrice: 0, nextInOwner: 0, prevInOwner: 0
            }));
        } else {
            if (remaining > 0 && !takerOnly) {
                _insertIntoBook(orderId, Order({
                    note: note, tokenContract: tc, price: storedPrice,
                    amount: remaining, initialAmount: initialAmount, filledAccum: initialAmount - remaining,
                    escrow: 0, deadline: 0, ts: uint64(block.timestamp),
                    flags: flags, isBuy: false,
                    nextAtPrice: 0, prevAtPrice: 0, nextInOwner: 0, prevInOwner: 0
                }));
            }
        }
    }

    // ========================================================
    // Queue (circular) — ported from dex OrderBook
    // ========================================================

    function _allocSlot() private returns (uint8 slot) {
        slot = _queueTail;
        _queueTail = uint8((uint32(_queueTail) + 1) % uint32(QUEUE_CAPACITY));
        _queueSize++;
    }

    function _advanceHead() private {
        delete _queue[_queueHead];
        _queueHead = uint8((uint32(_queueHead) + 1) % uint32(QUEUE_CAPACITY));
        _queueSize--;
    }

    function _enqueuePlace(address owner, bool isBuy, uint8 flags, uint256 price, uint128 amount, uint128 escrow, address tc, uint64 deadline) private {
        require(_queueSize < QUEUE_PLACE_LIMIT, ERR_QUEUE_FULL);
        uint8 slot = _allocSlot();
        _queue[slot] = QueueEntry({
            entryType: QENTRY_PLACE, owner: owner, isBuy: isBuy, flags: flags, price: price,
            amount: amount, escrow: escrow, tokenContract: tc, deadline: deadline,
            targetOrderId: 0, contOrderId: 0, contRemaining: 0, contLeftover: 0
        });
    }

    function _enqueueCancel(address owner, uint128 targetOrderId) private {
        require(_queueSize < QUEUE_CAPACITY, ERR_QUEUE_FULL);
        uint8 slot = _allocSlot();
        _queue[slot] = QueueEntry({
            entryType: QENTRY_CANCEL, owner: owner, isBuy: false, flags: 0, price: 0,
            amount: 0, escrow: 0, tokenContract: address(0), deadline: 0,
            targetOrderId: targetOrderId, contOrderId: 0, contRemaining: 0, contLeftover: 0
        });
    }

    function _enqueueCancelAll(address owner) private {
        require(_queueSize < QUEUE_CAPACITY, ERR_QUEUE_FULL);
        uint8 slot = _allocSlot();
        _queue[slot] = QueueEntry({
            entryType: QENTRY_CANCEL_ALL, owner: owner, isBuy: false, flags: 0, price: 0,
            amount: 0, escrow: 0, tokenContract: address(0), deadline: 0,
            targetOrderId: 0, contOrderId: 0, contRemaining: 0, contLeftover: 0
        });
    }

    function _selfCallProcessHead() private pure {
        InferenceOrderBook(address(this)).processHead{value: 3 vmshell, flag: 1, bounce: false}();
    }

    /// @notice Public drain entry: anyone can poke (and the engine self-calls it
    ///         to continue an in-flight match across txs).
    function processHead() public {
        tvm.accept();
        ensureBalance();
        _processHeadCore();
    }

    function _processHeadCore() private {
        if (_queueSize == 0) { return; }
        QueueEntry e = _queue[_queueHead];
        bool keepHead = false;

        if (e.entryType == QENTRY_CANCEL) {
            _doCancel(e.owner, e.targetOrderId);
        } else if (e.entryType == QENTRY_CANCEL_ALL) {
            uint8 cancelled = _doCancelAll(e.owner);
            keepHead = (cancelled >= MAX_CANCEL_PER_CALL);
        } else {
            keepHead = _doPlaceHead();
        }

        if (!keepHead) { _advanceHead(); }
        if (_queueSize > 0) { _selfCallProcessHead(); }
    }

    /// @notice Process (or resume) the head PLACE entry. Returns true to keep the
    ///         head (a match continuation is needed — resumes next tx).
    function _doPlaceHead() private returns (bool) {
        QueueEntry e = _queue[_queueHead];
        bool firstRun = (e.contOrderId == 0);
        uint128 orderId = firstRun ? _nextOrderId++ : e.contOrderId;
        bool isMarket = (e.flags & FLAG_MARKET) != 0;

        if (firstRun) {
            emit OrderPlaced{dest: address.makeAddrExtern(OfferPlacedEmit, bitCntAddress)}(
                orderId, e.isBuy, e.price, e.amount, e.owner, e.tokenContract, e.deadline);

            // POST_ONLY: reject if it would cross.
            if ((e.flags & FLAG_POST_ONLY) != 0) {
                optional(uint256, PriceLevel) best = _bestOpposite(e.isBuy);
                if (best.hasValue()) {
                    (uint256 bp, ) = best.get();
                    if (_crosses(e.isBuy, e.price, false, bp)) {
                        if (e.isBuy && e.escrow > 0) { _payShell(e.owner, e.escrow); }
                        return false;
                    }
                }
            }
            // FOK: all-or-nothing pre-check (bounded level walk).
            if ((e.flags & FLAG_FOK) != 0 && !_enoughLiquidity(e.isBuy, e.price, isMarket, e.amount)) {
                if (e.isBuy && e.escrow > 0) { _payShell(e.owner, e.escrow); }
                return false;
            }
        }

        uint128 inAmount   = firstRun ? e.amount  : e.contRemaining;
        uint128 inEscrow   = firstRun ? e.escrow  : e.contLeftover;
        (uint128 remaining, uint128 leftover, bool capped) =
            _match(orderId, e.isBuy, e.price, isMarket, e.owner, e.tokenContract, inAmount, inEscrow);

        if (capped) {
            // BUY taker crossed > one tx of liquidity → persist cursor, resume next tx.
            e.contOrderId = orderId;
            e.contRemaining = remaining;
            e.contLeftover = leftover;
            _queue[_queueHead] = e;
            return true;
        }
        _finalizeTaker(orderId, e.isBuy, e.price, e.owner, e.tokenContract, remaining, leftover, e.amount, e.flags, e.deadline);
        return false;
    }

    function _doCancel(address owner, uint128 orderId) private {
        Order o = _orders[orderId];
        if (o.amount == 0 && o.note == address(0)) { return; }
        if (o.note != owner) { return; }
        uint128 refund = o.escrow;
        _removeFromBook(orderId);
        emit OrderCancelled{dest: address.makeAddrExtern(OfferCancelledEmit, bitCntAddress)}(orderId, refund);
        _payShell(owner, refund);
    }

    function _doCancelAll(address owner) private returns (uint8 cancelled) {
        cancelled = 0;
        uint128 cur = _ownerHead[owner];
        while (cur != 0 && cancelled < MAX_CANCEL_PER_CALL) {
            Order o = _orders[cur];
            uint128 next = o.nextInOwner;
            uint128 refund = o.escrow;
            _removeFromBook(cur);
            emit OrderCancelled{dest: address.makeAddrExtern(OfferCancelledEmit, bitCntAddress)}(cur, refund);
            if (refund > 0) { _payShell(owner, refund); }
            cur = next;
            cancelled++;
        }
    }

    // ========================================================
    // Public order entry (enqueue + drain)
    // ========================================================

    function placeSellOffer(uint128 pricePerTick, uint128 maxTicks, address tokenContract, uint8 flags) public {
        ensureBalance();
        require(maxTicks > 0, ERR_BAD_PARAM);
        require((flags & FLAG_POST_ONLY) == 0 || (flags & TAKER_FLAGS) == 0, ERR_BAD_FLAGS);
        require((flags & FLAG_IOC) == 0 || (flags & FLAG_FOK) == 0, ERR_BAD_FLAGS);
        bool isMarket = (flags & FLAG_MARKET) != 0;
        require(isMarket || pricePerTick > 0, ERR_BAD_PARAM);
        tvm.accept();
        _enqueuePlace(msg.sender, false, flags, isMarket ? 0 : pricePerTick, maxTicks, 0, tokenContract, 0);
        _processHeadCore();
    }

    function placeBuyOrder(uint128 maxPricePerTick, uint128 ticks, uint8 flags, uint64 deadline) public {
        ensureBalance();
        require(ticks > 0, ERR_BAD_PARAM);
        require((flags & FLAG_POST_ONLY) == 0 || (flags & TAKER_FLAGS) == 0, ERR_BAD_FLAGS);
        require((flags & FLAG_IOC) == 0 || (flags & FLAG_FOK) == 0, ERR_BAD_FLAGS);
        require(deadline == 0 || deadline > block.timestamp, ERR_EXPIRED);
        bool isMarket = (flags & FLAG_MARKET) != 0;
        require(isMarket || maxPricePerTick > 0, ERR_BAD_PARAM);

        mapping(uint32 => varuint32) currencies = msg.currencies;
        require(currencies.exists(SHELL_ECC_ID), ERR_NO_SHELL);
        uint128 escrow = uint128(currencies[SHELL_ECC_ID]);
        if (!isMarket) { require(escrow >= uint128(uint256(ticks) * _unit(maxPricePerTick)), ERR_INSUFFICIENT_DEPOSIT); }
        tvm.accept();
        _enqueuePlace(msg.sender, true, flags, isMarket ? type(uint256).max : maxPricePerTick, ticks, escrow, address(0), deadline);
        _processHeadCore();
    }

    function cancelOrder(uint128 orderId) public {
        ensureBalance();
        tvm.accept();
        _enqueueCancel(msg.sender, orderId);
        _processHeadCore();
    }

    function cancelAllOrders() public {
        ensureBalance();
        tvm.accept();
        _enqueueCancelAll(msg.sender);
        _processHeadCore();
    }

    // ========================================================
    // §8 subscription (semantic order)
    // ========================================================

    /// @notice §8 subscription: a resting limit buy whose budget is throttled into
    ///         SUB_CYCLES weekly cycles; unspent per-cycle budget is forfeited (not
    ///         rolled) to the sellers it funded that cycle, pro-rata by funded ticks.
    /// @dev Rests as a standing bid (filled by incoming sells); does not take on
    ///      placement. Renewal = client re-places (§8.2); `autoRenew` is a hint.
    function placeSubscription(uint128 maxPricePerTick, uint128 ticks, bool autoRenew) public {
        ensureBalance();
        require(ticks > 0 && maxPricePerTick > 0, ERR_BAD_PARAM);
        mapping(uint32 => varuint32) currencies = msg.currencies;
        require(currencies.exists(SHELL_ECC_ID), ERR_NO_SHELL);
        uint128 escrow = uint128(currencies[SHELL_ECC_ID]);
        require(escrow >= uint128(uint256(ticks) * _unit(maxPricePerTick)), ERR_INSUFFICIENT_DEPOSIT);
        tvm.accept();

        address buyerNote = msg.sender;
        uint128 orderId = _nextOrderId++;
        uint64  dl = uint64(block.timestamp) + SUB_PERIOD;
        uint128 cycleBudget = escrow / uint128(SUB_CYCLES);

        _insertIntoBook(orderId, Order({
            note: buyerNote, tokenContract: address(0), price: maxPricePerTick,
            amount: ticks, initialAmount: ticks, filledAccum: 0,
            escrow: escrow, deadline: dl, ts: uint64(block.timestamp),
            flags: 0, isBuy: true,
            nextAtPrice: 0, prevAtPrice: 0, nextInOwner: 0, prevInOwner: 0
        }));
        _subs[orderId] = Sub({
            periodStart: uint64(block.timestamp), curCycle: 0,
            cycleBudget: cycleBudget, cycleSpent: 0, autoRenew: autoRenew, exists: true
        });
        emit SubscriptionPlaced{dest: address.makeAddrExtern(SubscriptionPlacedEmit, bitCntAddress)}(orderId, buyerNote, maxPricePerTick, ticks, cycleBudget, autoRenew);
    }

    function _subTouch(uint128 orderId) private {
        Sub s = _subs[orderId];
        if (!s.exists) { return; }
        while (block.timestamp >= s.periodStart + (uint64(s.curCycle) + 1) * SUB_CYCLE_LEN) {
            uint128 unspent = s.cycleBudget > s.cycleSpent ? s.cycleBudget - s.cycleSpent : 0;
            if (unspent > 0) {
                Order o = _orders[orderId];
                if (o.escrow < unspent) { unspent = o.escrow; }
                _orders[orderId].escrow = o.escrow - unspent;
                _forfeitPool[orderId][s.curCycle] += unspent;
                emit CycleForfeited{dest: address.makeAddrExtern(CycleForfeitedEmit, bitCntAddress)}(orderId, s.curCycle, unspent, _cycleFundedTicks[orderId][s.curCycle]);
            }
            s.cycleSpent = 0;
            s.curCycle += 1;
            if (s.curCycle >= SUB_CYCLES) { _subs[orderId] = s; _expireSub(orderId); return; }
        }
        _subs[orderId] = s;
    }

    function _expireSub(uint128 orderId) private {
        Order o = _orders[orderId];
        uint128 refund = o.escrow;
        _removeFromBook(orderId);
        delete _subs[orderId];
        if (refund > 0) { _payShell(o.note, refund); emit Refunded{dest: address.makeAddrExtern(BuyUnmatchedEmit, bitCntAddress)}(o.note, refund); }
    }

    function pokeSubscription(uint128 orderId) public {
        ensureBalance();
        require(_subs[orderId].exists, ERR_NOT_SUB);
        tvm.accept();
        _subTouch(orderId);
    }

    function claimForfeit(uint128 orderId, uint8 cycle) public {
        ensureBalance();
        address seller = msg.sender;
        uint128 pool  = _forfeitPool[orderId][cycle];
        uint128 total = _cycleFundedTicks[orderId][cycle];
        uint128 mine  = _cycleSellerTicks[orderId][cycle][seller];
        require(pool > 0 && total > 0 && mine > 0, ERR_NOTHING_TO_CLAIM);
        tvm.accept();
        uint128 share = uint128(uint256(pool) * uint256(mine) / uint256(total));
        delete _cycleSellerTicks[orderId][cycle][seller];
        _forfeitPool[orderId][cycle]      = pool - share;
        _cycleFundedTicks[orderId][cycle] = total - mine;
        _payShell(seller, share);
        emit ForfeitClaimed{dest: address.makeAddrExtern(ForfeitClaimedEmit, bitCntAddress)}(orderId, cycle, seller, share);
    }

    // ========================================================
    // Reference-price stats (spec §6.2 / §7)
    // ========================================================

    function _recordTrade(uint128 price, uint128 ticks) private {
        uint64 day = uint64(block.timestamp) / SECS_PER_DAY;
        DayData cur = _daily[_curSlot];
        if (cur.day != day) {
            if (cur.day != 0 || cur.volSum != 0) { _curSlot = uint8((_curSlot + 1) % 8); }
            _daily[_curSlot] = DayData({day: day, priceVolSum: 0, volSum: 0});
        }
        _daily[_curSlot].priceVolSum += uint256(price) * uint256(ticks);
        _daily[_curSlot].volSum      += uint256(ticks);
    }

    function _weeklyMedian() private view returns (uint256) {
        uint64 nowDay = uint64(block.timestamp) / SECS_PER_DAY;
        uint64 minDay = nowDay >= 6 ? nowDay - 6 : 0;
        uint256[] vwaps;
        uint256 totalVol = 0;
        for (uint8 i = 0; i < 8; i++) {
            DayData d = _daily[i];
            if (d.volSum == 0) { continue; }
            if (d.day < minDay || d.day > nowDay) { continue; }
            vwaps.push(d.priceVolSum / d.volSum);
            totalVol += d.volSum;
        }
        require(totalVol >= MIN_LIQUIDITY, ERR_NO_LIQUIDITY);
        uint n = vwaps.length;
        for (uint a = 0; a < n; a++) {
            for (uint b = a + 1; b < n; b++) {
                if (vwaps[b] < vwaps[a]) { uint256 t = vwaps[a]; vwaps[a] = vwaps[b]; vwaps[b] = t; }
            }
        }
        if (n % 2 == 1) { return vwaps[n / 2]; }
        return (vwaps[n / 2 - 1] + vwaps[n / 2]) / 2;
    }

    function getWeeklyMedianPrice() external view returns (uint256) { return _weeklyMedian(); }

    function requestWeeklyMedian(uint256 eventId, uint256 oracleListHash, uint32 tokenType) public {
        ensureBalance();
        uint256 price = _weeklyMedian();
        IWeeklyMedianSink(msg.sender).onWeeklyMedian{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(
            eventId, oracleListHash, tokenType, price);
    }

    // ========================================================
    // Getters
    // ========================================================

    function getOrder(uint128 id) external view returns (
        address note, address tokenContract, uint256 price, uint128 amount,
        uint128 escrow, uint64 deadline, uint8 flags, bool isBuy, uint64 ts
    ) {
        Order o = _orders[id];
        return (o.note, o.tokenContract, o.price, o.amount, o.escrow, o.deadline, o.flags, o.isBuy, o.ts);
    }

    function getBestBidAsk() external view returns (bool hasBid, uint256 bid, bool hasAsk, uint256 ask) {
        optional(uint256, PriceLevel) b = _levels[true].max();
        optional(uint256, PriceLevel) a = _levels[false].min();
        if (b.hasValue()) { (bid, ) = b.get(); hasBid = true; }
        if (a.hasValue()) { (ask, ) = a.get(); hasAsk = true; }
    }

    function getStats() external view returns (uint128 nextOrderId, uint128 orderCount, uint128 executedNotional, uint128 executedTicks) {
        return (_nextOrderId, _orderCount, _executedNotional, _executedTicks);
    }

    function getQueueSize() external view returns (uint8) { return _queueSize; }

    function getSubscription(uint128 orderId) external view returns (
        bool exists, uint64 periodStart, uint8 curCycle, uint128 cycleBudget, uint128 cycleSpent, bool autoRenew
    ) {
        Sub s = _subs[orderId];
        return (s.exists, s.periodStart, s.curCycle, s.cycleBudget, s.cycleSpent, s.autoRenew);
    }

    function getForfeit(uint128 orderId, uint8 cycle) external view returns (uint128 pool, uint128 fundedTicks) {
        return (_forfeitPool[orderId][cycle], _cycleFundedTicks[orderId][cycle]);
    }

    function getParams() external view returns (uint256 modelHash, uint128 tickSize, uint16 platformFeeBps) {
        return (_modelHash, _tickSize, PLATFORM_FEE_BPS);
    }

    function getVersion() external pure returns (string, string) {
        return (version, "InferenceOrderBook");
    }
}
