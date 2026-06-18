pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";

/// @notice Prediction-market oracle sink (spec §6.2). The reference price is
///         published to the PMP through this interface; the PMP settles its
///         instruments on it.
interface IPmpOracle {
    function submitReferencePrice(uint256 modelHash, uint128 price) external;
}

/// @title InferenceOracle (spec §6-7)
/// @notice Manipulation-resistant reference price for one model's order book.
///         Fed ONLY by executed trades (§6.1) pushed from the bound
///         InferenceOrderBook. Per `SAMPLE_INTERVAL` it computes a
///         volume-weighted trimmed mean (trim `TRIM_PCT` each side by volume,
///         §7.2) of that interval's executions; `reference_price` is the TWAP of
///         those interval values over `ORACLE_WINDOW`. Guards: `MIN_LIQUIDITY`
///         (don't settle thin windows) and `MOVE_CAP` (per-publish circuit
///         breaker). The address derives from the full parameter set (§8).
contract InferenceOracle is AiRegistryModifiers {
    string constant version = "1.0.0";

    // Bounds the per-interval sample buffer (gas cap on the in-place sort).
    uint32 constant MAX_SAMPLES = 64;

    // Static — address derivation (spec §8).
    uint256 static _modelHash;
    uint64  static _sampleInterval;   // seconds per interval
    uint64  static _windowIntervals;  // intervals in the TWAP window
    uint32  static _trimPctBps;       // trim each side (2500 = 25%)
    uint128 static _minLiquidity;     // min total volume in window to settle
    uint32  static _moveCapBps;       // max reference move per publish (0 = off)

    address _orderBook;   // bound execution source (set once)
    address _pmp;         // optional PMP push target (set once)

    // Current (open) interval accumulation.
    uint64    _curIndex;
    uint128[] _curPrices;
    uint128[] _curVolumes;
    uint128   _curVolume;

    // Finalized interval values.
    mapping(uint64 => uint128) _intervalPrice;   // index => vw trimmed mean
    mapping(uint64 => uint128) _intervalVolume;  // index => total volume
    uint64  _lastFinalizedIndex;

    uint128 _lastReference;
    bool    _hasReference;

    event ExecutionRecorded(uint128 price, uint128 ticks, uint64 intervalIndex);
    event IntervalFinalized(uint64 index, uint128 vwTrimmedMean, uint128 volume);
    event ReferencePublished(uint128 price, uint128 windowVolume);

    constructor() {
        tvm.accept();
        // addr_std(0,0) so the `== address(0)` unset-checks in setOrderBook/
        // setPmp and the PMP push work (an uninitialized address is addr_none,
        // which does not compare equal to address(0)).
        _orderBook = address(0);
        _pmp = address(0);
        ensureBalance();
        _curIndex = uint64(block.timestamp) / _sampleInterval;
    }

    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) { return; }
        gosh.mintshellq(MIN_BALANCE);
    }

    // ========================================================
    // Wiring (one-time)
    // ========================================================

    function setOrderBook(address ob) public {
        require(_orderBook == address(0), ERR_ALREADY_SET);
        tvm.accept();
        ensureBalance();
        _orderBook = ob;
    }

    function setPmp(address pmp) public {
        require(_pmp == address(0), ERR_ALREADY_SET);
        tvm.accept();
        ensureBalance();
        _pmp = pmp;
    }

    // ========================================================
    // Ingest (spec §6.1) — only executed trades, only from the book
    // ========================================================

    /// @notice Record one executed trade. Auth: the bound order book only.
    function recordExecution(uint128 pricePerTick, uint128 ticks) public {
        require(msg.sender == _orderBook, ERR_INVALID_SENDER);
        ensureBalance();
        if (pricePerTick == 0 || ticks == 0) { return; }
        _rollIfNeeded();
        if (uint32(_curPrices.length) < MAX_SAMPLES) {
            _curPrices.push(pricePerTick);
            _curVolumes.push(ticks);
            _curVolume += ticks;
        }
        emit ExecutionRecorded{dest: address.makeAddrExtern(ExecutionRecordedEmit, bitCntAddress)}(pricePerTick, ticks, _curIndex);
    }

    /// @notice Force a roll of the current interval if its window elapsed (lets
    ///         the last interval finalize without waiting for the next trade).
    function poke() public {
        ensureBalance();
        tvm.accept();
        _rollIfNeeded();
    }

    function _rollIfNeeded() private {
        uint64 idx = uint64(block.timestamp) / _sampleInterval;
        if (idx == _curIndex) { return; }
        if (_curPrices.length > 0) { _finalizeCurrent(); }
        _curIndex = idx;
    }

    function _finalizeCurrent() private {
        uint128 mean = _vwTrimmedMeanInPlace();
        _intervalPrice[_curIndex]  = mean;
        _intervalVolume[_curIndex] = _curVolume;
        _lastFinalizedIndex = _curIndex;
        emit IntervalFinalized{dest: address.makeAddrExtern(IntervalFinalizedEmit, bitCntAddress)}(_curIndex, mean, _curVolume);
        delete _curPrices;
        delete _curVolumes;
        _curVolume = 0;
    }

    // ========================================================
    // §7.2 volume-weighted trimmed mean (sorts the buffer in place)
    // ========================================================

    function _vwTrimmedMeanInPlace() private returns (uint128) {
        uint n = _curPrices.length;
        if (n == 0) { return 0; }

        // Insertion sort by price ascending, carrying volumes in tandem.
        for (uint i = 1; i < n; i++) {
            uint128 p = _curPrices[i];
            uint128 v = _curVolumes[i];
            uint j = i;
            while (j > 0 && _curPrices[j - 1] > p) {
                _curPrices[j]  = _curPrices[j - 1];
                _curVolumes[j] = _curVolumes[j - 1];
                j--;
            }
            _curPrices[j]  = p;
            _curVolumes[j] = v;
        }

        // Trim TRIM_PCT of total volume off each tail; average the middle band,
        // splitting volume that straddles a cut boundary.
        uint256 total   = uint256(_curVolume);
        uint256 trim    = total * uint256(_trimPctBps) / uint256(BPS_DENOMINATOR);
        uint256 lowCut  = trim;
        uint256 highCut = total - trim;

        uint256 cum  = 0;
        uint256 wsum = 0;
        uint256 used = 0;
        for (uint i = 0; i < n; i++) {
            uint256 vol = uint256(_curVolumes[i]);
            uint256 lo  = cum;
            uint256 hi  = cum + vol;
            uint256 a   = lo > lowCut ? lo : lowCut;
            uint256 b   = hi < highCut ? hi : highCut;
            if (b > a) {
                uint256 ov = b - a;
                wsum += uint256(_curPrices[i]) * ov;
                used += ov;
            }
            cum = hi;
        }

        // Degenerate band (e.g. a single trade fully trimmed) -> raw VWAP.
        if (used == 0) {
            for (uint i = 0; i < n; i++) {
                wsum += uint256(_curPrices[i]) * uint256(_curVolumes[i]);
                used += uint256(_curVolumes[i]);
            }
        }
        return uint128(wsum / used);
    }

    // ========================================================
    // Reference price (spec §7.2 TWAP) + publish to PMP (spec §6.2)
    // ========================================================

    function _computeReference() private view returns (uint128 price, uint128 windowVolume, bool settleable) {
        uint64 cur = uint64(block.timestamp) / _sampleInterval;
        uint64 startIdx = cur > _windowIntervals ? cur - _windowIntervals : 0;
        uint256 sum = 0;
        uint64  count = 0;
        uint128 vol = 0;
        for (uint64 idx = startIdx; idx < cur; idx++) {
            uint128 p = _intervalPrice[idx];
            if (p > 0) {
                sum += uint256(p);
                count += 1;
                vol += _intervalVolume[idx];
            }
        }
        if (count == 0) { return (0, 0, false); }
        return (uint128(sum / uint256(count)), vol, vol >= _minLiquidity);
    }

    function _applyMoveCap(uint128 p) private view returns (uint128) {
        if (!_hasReference || _moveCapBps == 0) { return p; }
        uint128 delta = uint128(uint256(_lastReference) * uint256(_moveCapBps) / uint256(BPS_DENOMINATOR));
        uint128 maxUp = _lastReference + delta;
        uint128 maxDn = _lastReference > delta ? _lastReference - delta : 0;
        if (p > maxUp) { return maxUp; }
        if (p < maxDn) { return maxDn; }
        return p;
    }

    /// @notice Compute (and circuit-break) the reference, publish to the PMP if
    ///         wired, and remember it for the next move-cap. Reverts if the
    ///         window is below `MIN_LIQUIDITY`.
    function publish() public returns (uint128) {
        ensureBalance();
        tvm.accept();
        _rollIfNeeded();
        (uint128 twap, uint128 vol, bool settleable) = _computeReference();
        require(settleable, ERR_LOW_LIQUIDITY);
        uint128 finalP = _applyMoveCap(twap);
        _lastReference = finalP;
        _hasReference  = true;
        if (_pmp != address(0)) {
            IPmpOracle(_pmp).submitReferencePrice{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(_modelHash, finalP);
        }
        emit ReferencePublished{dest: address.makeAddrExtern(ReferencePublishedEmit, bitCntAddress)}(finalP, vol);
        return finalP;
    }

    // ========================================================
    // Getters
    // ========================================================

    function getReferencePrice() external view returns (uint128 price, uint128 windowVolume, bool settleable) {
        return _computeReference();
    }

    function getCurrent() external view returns (uint64 intervalIndex, uint32 sampleCount, uint128 volume) {
        return (_curIndex, uint32(_curPrices.length), _curVolume);
    }

    function getInterval(uint64 index) external view returns (uint128 price, uint128 volume) {
        return (_intervalPrice[index], _intervalVolume[index]);
    }

    function getLastReference() external view returns (uint128 price, bool hasReference) {
        return (_lastReference, _hasReference);
    }

    function getWiring() external view returns (address orderBook, address pmp) {
        return (_orderBook, _pmp);
    }

    function getConfig() external view returns (
        uint256 modelHash, uint64 sampleInterval, uint64 windowIntervals,
        uint32 trimPctBps, uint128 minLiquidity, uint32 moveCapBps
    ) {
        return (_modelHash, _sampleInterval, _windowIntervals, _trimPctBps, _minLiquidity, _moveCapBps);
    }

    function getVersion() external pure returns (string, string) {
        return (version, "InferenceOracle");
    }
}
