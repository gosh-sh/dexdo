pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./interfaces.sol";

/// @title TokenContract (streaming deal / `token_contract` per spec §3-4)
/// @notice One streaming inference deal between one seller (owner) and one
///         buyer. Payment is in ECC[2] SHELL, escrowed in this contract;
///         identities/locks ride on PrivateNotes (model B).
///
///         PROBE TICK (spec §3.1.2). The first tick of every stream is a
///         *probe*: it is FROZEN from the buyer's escrow and NOT prepaid to the
///         seller, and the seller posts `SELLER_PROBE_COMMISSION` (≈ the
///         platform fee on one tick, `_probeCommission()`). While in `Probe`:
///           - silence through `SETTLE_WINDOW` (advance) → probe accepted: the
///             probe tick is finalized to the seller, the commission is returned
///             to the seller, the platform fee is taken by-fact, and the deal
///             enters `Streaming` with the standard two-tick invariant (§3.2);
///           - buyer `stop()` → BURN BOTH: the buyer's probe tick AND the
///             seller's commission go to `gosh.burnecc`; nothing to either side
///             (scam revenue = 0, §3.1.2/§5.4). Remaining deposit refunds the buyer;
///           - seller no-show (`reclaimOnTimeout`, `STREAM_TIMEOUT`) → the buyer
///             reclaims the probe tick in full (pays nothing) and the commission
///             is returned to the seller; NO burn (no-show is not slashed, §9.1);
///           - dispute → both notes lock; on `DISPUTE_WINDOW` timeout it reduces
///             to the probe rule (burn both, §4.2), not the standard split.
///         Burn happens ONLY on an active buyer stop, never on a seller no-show.
///
///         After the probe is accepted (`_probeAccepted`), at any open moment
///         exactly one tick is prepaid to the seller (delivered, awaiting
///         finalization) and exactly one tick is frozen as buffer (spec §3.2);
///         lifecycle is the standard split (§4.1) untouched.
///
///         Timing windows and the platform fee are PROTOCOL CONSTANTS
///         (`SETTLE_WINDOW`/`STREAM_TIMEOUT`/`DISPUTE_WINDOW`/`PLATFORM_FEE_BPS`
///         in modifiers.sol), not per-deal parameters.
///
///         Lifecycle:
///         1. `fund()`/`fundFromOrderBook()` — buyer escrows SHELL; the buyer
///                             note pubkey is recorded (spec §2.3/§3.1.1).
///         1b.`fundProbeCommission()` — seller posts SELLER_PROBE_COMMISSION.
///         2. `open(cipher)` — seller posts the endpoint encrypted to the
///                             buyer's pubkey, freezes the probe tick, locks the
///                             buyer note; state = `Probe`.
///         3. `advance()`    — seller-driven: after `SETTLE_WINDOW` of buyer
///                             silence, accept the probe (Probe→Streaming) or
///                             finalize the prepaid tick (Streaming).
///         4a.`stop()`       — buyer exit: Probe → burn both; Streaming → §4.1.
///         4b.`dispute()`    — buyer contests; both notes lock;
///                             `resolveDisputeTimeout()`/`releaseDispute()`.
///         4c.`reclaimOnTimeout()` — seller no-show after `STREAM_TIMEOUT`.
///         5. `withdrawShell`/`destroy` — seller pulls finalized SHELL (§3.5).
contract TokenContract is AiRegistryModifiers {
    string constant version = "4.0.15";

    // Canonical AI SuperRoot account id (workchain 0) — same anchor IOB/PN pin. Used ONLY as the
    // fixed sink for `cleanupUnopened`'s residual-native sweep (so a permissionless caller cannot
    // route the leftover gas to an arbitrary address). On shellnet the SuperRoot is now ADDRESS-STABLE
    // across versions (`SuperRoot.updateCode` swaps code in place, no rotation), so this is a fixed
    // literal — no genaddr recompute, no pin cycle. LOCAL/MAINNET build: the vanity 0:0c0c… SuperRoot.
    uint256 constant SUPER_ROOT_ADDR = 0x0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c;

    // Native value attached to THIS contract's cross-dapp messages (register / stream-lock /
    // payout). Tunable; recipients self-fund via `accept`/`ensureBalance`, so this
    // only needs to cover what a non-accepting hop requires. (TC/RM-local — NOT the shared
    // REGISTER_FORWARD_VALUE the IOB also uses for its SHELL handover.)
    varuint16 constant DAPP_MSG_VALUE = 0.01 vmshell;

    event ContractDeployed(address self);
    event StreamFunded(address buyer, uint128 deposit);
    event ProbeCommissionFunded(uint128 amount);
    event StreamOpened(address buyer, uint128 pricePerTick);
    event ProbeAccepted(address buyer, uint128 toSeller, uint128 commissionReturned);
    event ProbeBurned(address buyer, uint128 burnedProbe, uint128 burnedCommission, uint128 refundToBuyer);
    event TickFinalized(uint128 finalizedOwed, uint128 deposit);
    event StreamStopped(address buyer, uint128 toSeller, uint128 refundToBuyer);
    event StreamDisputed(address buyer, uint64 at);
    event DisputeResolved(uint128 toSeller, uint128 refundToBuyer, bool released);
    event StreamReclaimed(address buyer, uint128 refundToBuyer);
    event ShellWithdrawn(address recipient, uint128 amount);
    event ContractDestroyed(address self);

    // Static (part of stateInit, contribute to address derivation).
    uint256 static _sellerPubkey;
    address static _rootModelAddress;
    uint64 static _nonce;

    // Immutable deal config (constructor).
    string  _modelName;
    uint256 _modelHash;       // sha256(_modelName), verified in the ctor — on-chain authoritative id
    uint128 _tickSize;        // tokens per tick (informational for the buyer)
    uint128 _pricePerTick;    // SHELL per tick (P)
    uint128 _maxTicks;        // upper bound on ticks this deal serves

    // Deal state.
    address _buyer;           // buyer note address (funder; payouts/locks)
    uint256 _buyerPubkey;     // buyer note pubkey (gateway auth, spec §3.1.1)
    address _sellerNote;      // seller note address (dispute lock)
    bytes   _endpointCipher;  // endpoint encrypted to the buyer's pubkey

    bool    _funded;
    bool    _opened;
    bool    _probeAccepted;   // false = Probe (first tick), true = Streaming (§3.1.2)
    bool    _disputed;

    bool    _sellerProbeFunded; // seller posted SELLER_PROBE_COMMISSION
    uint128 _sellerProbeLocked; // SHELL held as the seller's probe commission

    uint128 _deposit;         // SHELL available for future ticks (value + reserved fee)
    uint128 _prepaid;         // SHELL: the delivered, not-yet-finalized tick (value P); 0 in Probe
    uint128 _frozen;          // SHELL: buffer tick in Streaming, or the probe tick in Probe (value P)
    uint128 _finalizedOwed;   // SHELL finalized to the seller (withdrawable; incl. rebate / returned commission)
    uint128 _feeAccrued;      // SHELL fee charged by-fact on finalized ticks (§5.1)
    uint128 _ticksFinalized;  // count of finalized ticks (n for rebate §5.3)
    bool    _everDisputed;    // a dispute ever opened → no rebate (§5.3)
    uint64  _fundedTime;      // when funded (MATCH_OPEN_TIMEOUT cleanup, §2.1)
    uint64  _prepaidTime;     // when `_prepaid`/probe was set (settle window)
    uint64  _lastAdvance;     // last seller activity (stream timeout)
    uint64  _disputeTime;     // when the dispute opened
    uint64  _settleWindow;    // per-deal advance window W = f(pricePerTick), §9.1
    uint64  _streamTimeout;   // per-deal reclaim window = W + grace, §9.1

    constructor(
        string  modelName,
        uint256 modelHash,
        uint128 tickSize,
        uint128 pricePerTick,
        uint128 maxTicks,
        address sellerNote
    ) {
        tvm.accept();
        require(tickSize > 0, ERR_BAD_PARAM);
        require(pricePerTick > 0, ERR_BAD_PARAM);
        require(maxTicks >= 2, ERR_BAD_PARAM);
        // On-chain authoritative model id: same single-cell sha256 invariant as the order book.
        // Binds this deal contract's modelHash to the verified `producer--model--version` preimage
        // (so an indexer reading the TC alone gets the genuine model name, not a free-text label).
        require(modelName.byteLength() <= 127, ERR_BAD_PARAM);
        require(sha256(modelName) == modelHash, ERR_BAD_PARAM);

        _modelName    = modelName;
        _modelHash    = modelHash;
        _tickSize     = tickSize;
        _pricePerTick = pricePerTick;
        _maxTicks     = maxTicks;
        _sellerNote   = sellerNote;

        // Per-deal advance window scaled by tick price (caps idle drain to the
        // slope), clamped to [SETTLE_WINDOW, STREAM_WINDOW_MAX]; the reclaim
        // window is W + grace so reclaim is always strictly after advance (§9.1).
        uint64 w = uint64(uint256(pricePerTick) * uint256(STREAM_WINDOW_SECS_PER_SHELL) / uint256(SHELL_UNIT));
        if (w < SETTLE_WINDOW) { w = SETTLE_WINDOW; }
        if (w > STREAM_WINDOW_MAX) { w = STREAM_WINDOW_MAX; }
        _settleWindow  = w;
        _streamTimeout = w + STREAM_TIMEOUT_GRACE;

        ensureBalance();

        address selfExtern = address.makeAddrExtern(ContractDeployedEmit, bitCntAddress);
        emit ContractDeployed{dest: selfExtern}(address(this));

        IRootModelRegistry(_rootModelAddress).registerTokenContract{value: DAPP_MSG_VALUE, flag: 1}(_sellerPubkey, _nonce);
    }

    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) { return; }
        gosh.mintshellq(MIN_BALANCE);
    }

    function _payShell(address to, uint128 amount) private pure {
        if (amount == 0) { return; }
        mapping(uint32 => varuint32) ecc;
        ecc[SHELL_ECC_ID] = varuint32(amount);
        to.transfer({value: DAPP_MSG_VALUE, bounce: false, flag: 1, currencies: ecc});
    }

    /// @notice Burn SHELL via gosh.burnecc (spec §5.4). uint64-bounded like _settleFees.
    function _burnShell(uint128 amount) private pure {
        if (amount > 0 && amount <= uint128(type(uint64).max)) {
            gosh.burnecc(uint64(amount), SHELL_ECC_ID);
        }
    }

    /// @notice Platform fee (2.5%, PLATFORM_FEE_BPS) of `amount` (spec §5.1).
    function _fee(uint128 amount) private pure returns (uint128) {
        return uint128(uint256(amount) * uint256(PLATFORM_FEE_BPS) / uint256(BPS_DENOMINATOR));
    }

    /// @notice Seller probe commission (spec §3.1.2/§9.2): a percent of the tick
    ///         price P (`SELLER_PROBE_COMMISSION_BPS`), on the order of the
    ///         platform fee on a single tick. Returned to the seller on probe
    ///         acceptance / no-show; burned with the probe tick on a probe stop.
    function _probeCommission() private view returns (uint128) {
        return uint128(uint256(_pricePerTick) * uint256(SELLER_PROBE_COMMISSION_BPS) / uint256(BPS_DENOMINATOR));
    }

    /// @notice Seller rebate (§5.3) for `n` cleanly-finalized ticks at price P:
    ///         rate = min(REBATE_MAX_BPS, REBATE_SLOPE_BPS·n) bps; rebate =
    ///         rate/10000 · n · P. Always < the fee charged on n ticks (rate <
    ///         PLATFORM_FEE_BPS by construction), so net burn stays positive.
    function _rebate(uint128 n) private view returns (uint128) {
        uint256 rateBps = uint256(REBATE_SLOPE_BPS) * uint256(n);
        if (rateBps > uint256(REBATE_MAX_BPS)) { rateBps = uint256(REBATE_MAX_BPS); }
        return uint128(rateBps * uint256(n) * uint256(_pricePerTick) / uint256(BPS_DENOMINATOR));
    }

    /// @notice Settle accrued fees at close: pay the seller a rebate (only on a
    ///         clean, never-disputed close, §5.3) and burn the net (§5.4).
    function _settleFees(bool clean) private {
        uint128 rebate = 0;
        if (clean && !_everDisputed) {
            rebate = _rebate(_ticksFinalized);
            if (rebate > _feeAccrued) { rebate = _feeAccrued; }   // safety (never triggers by construction)
        }
        uint128 netBurn = _feeAccrued - rebate;
        if (rebate > 0) { _finalizedOwed += rebate; }             // seller withdraws it
        _burnShell(netBurn);
        _feeAccrued = 0;
    }

    // ========================================================
    // 1. Fund — buyer escrows SHELL, locks the deal
    // ========================================================

    function _recordFunding(address buyer, uint256 buyerPubkey, uint128 paid) private {
        // Buyer-side, by-fact fee (§5.1): the escrow covers per-tick (P + fee).
        // Must cover >= 2 full ticks (probe + at least one streaming tick) and <= maxTicks.
        uint128 unit = _pricePerTick + _fee(_pricePerTick);
        require(paid >= 2 * unit, ERR_INSUFFICIENT_DEPOSIT);
        require(paid <= _maxTicks * unit, ERR_OVERFLOW);
        _buyer       = buyer;
        _buyerPubkey = buyerPubkey;
        _deposit     = paid;
        _funded      = true;
        _fundedTime  = uint64(block.timestamp);
        emit StreamFunded{dest: address.makeAddrExtern(StreamFundedEmit, bitCntAddress)}(buyer, paid);
    }

    /// @notice Buyer sends ECC[2] SHELL to escrow the deal (direct path) and
    ///         records the buyer note pubkey for gateway auth (spec §3.1.1).
    function fund(uint256 buyerPubkey) public {
        ensureBalance();
        require(!_funded, ERR_ALREADY_FUNDED);
        mapping(uint32 => varuint32) currencies = msg.currencies;
        require(currencies.exists(SHELL_ECC_ID), ERR_NO_SHELL);
        tvm.accept();
        _recordFunding(msg.sender, buyerPubkey, uint128(currencies[SHELL_ECC_ID]));
    }

    /// @notice Order-book handover (spec §2.3): the InferenceOrderBook forwards
    ///         the matched SHELL, binds the buyer note (not msg.sender), and
    ///         records the buyer note pubkey it held in the book — the buyer's
    ///         PrivateNote forwards its `_ephemeralPubkey` when ordering, the OB
    ///         threads it through the match (§3.1.1, gateway auth).
    function fundFromOrderBook(address buyerNote, uint256 buyerPubkey) public {
        ensureBalance();
        mapping(uint32 => varuint32) currencies = msg.currencies;
        if (!currencies.exists(SHELL_ECC_ID)) { return; }
        tvm.accept();
        uint128 paid = uint128(currencies[SHELL_ECC_ID]);
        // The book forwards bounce:false, so a revert here would STRAND the buyer's SHELL on this
        // contract (then be sweepable by the seller). Instead, on any non-fundable fill — already
        // funded (nonce reuse), under 2 ticks, or over maxTicks — refund the buyer note IN FULL and
        // do not fund. Same accept-then-refund pattern as the placement-reject fix (#91).
        uint128 unit = _pricePerTick + _fee(_pricePerTick);
        if (_funded || paid < 2 * unit || paid > _maxTicks * unit) {
            _payShell(buyerNote, paid);
            return;
        }
        _recordFunding(buyerNote, buyerPubkey, paid);
    }

    // ========================================================
    // 1b. Probe commission — seller posts SELLER_PROBE_COMMISSION (spec §3.1.2)
    // ========================================================

    /// @notice Seller posts the probe commission in SHELL before `open()`.
    ///         Held in the contract: returned to the seller on probe acceptance
    ///         or seller no-show, burned with the probe tick on a probe stop.
    ///         Sent as an internal ECC[2] message (open() is external and cannot
    ///         carry value); the buyer's note is never touched.
    function fundProbeCommission() public {
        ensureBalance();
        require(!_opened, ERR_ALREADY_OPEN);
        require(!_sellerProbeFunded, ERR_PROBE_ALREADY_FUNDED);
        mapping(uint32 => varuint32) currencies = msg.currencies;
        require(currencies.exists(SHELL_ECC_ID), ERR_NO_SHELL);
        uint128 amount = uint128(currencies[SHELL_ECC_ID]);
        uint128 need = _probeCommission();
        require(amount >= need, ERR_INSUFFICIENT_DEPOSIT);
        tvm.accept();

        _sellerProbeLocked = need;
        _sellerProbeFunded = true;

        // Refund any excess SHELL above the required commission to the sender.
        uint128 excess = amount - need;
        if (excess > 0) { _payShell(msg.sender, excess); }

        emit ProbeCommissionFunded{dest: address.makeAddrExtern(ProbeCommissionFundedEmit, bitCntAddress)}(need);
    }

    // ========================================================
    // 2. Open — seller posts encrypted endpoint, freezes the probe tick
    // ========================================================

    /// @notice Seller-only. Posts the endpoint ciphertext (encrypted to the
    ///         buyer's pubkey), freezes ONE tick as the probe (spec §3.1.2: not
    ///         prepaid to the seller), and locks the buyer note. No platform fee
    ///         yet — it is taken by-fact when the probe is accepted (§5.1).
    function open(bytes endpointCipher) public onlyOwnerPubkey(_sellerPubkey) accept {
        ensureBalance();
        require(_funded, ERR_NOT_FUNDED);
        require(!_opened, ERR_ALREADY_OPEN);
        require(_sellerProbeFunded, ERR_PROBE_NOT_FUNDED);
        require(_deposit >= _pricePerTick, ERR_INSUFFICIENT_DEPOSIT);

        _endpointCipher = endpointCipher;

        // Probe tick: frozen from the buyer's escrow, NOT prepaid to the seller.
        _prepaid       = 0;
        _frozen        = _pricePerTick;
        _deposit      -= _pricePerTick;
        _probeAccepted = false;
        _prepaidTime   = uint64(block.timestamp);
        _lastAdvance   = uint64(block.timestamp);
        _opened        = true;

        IStreamNote(_buyer).streamLock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));

        emit StreamOpened{dest: address.makeAddrExtern(StreamOpenedEmit, bitCntAddress)}(_buyer, _pricePerTick);
    }

    // ========================================================
    // 3. Advance — accept the probe or finalize an accepted tick
    // ========================================================

    /// @notice Seller-only. After `SETTLE_WINDOW` of buyer silence:
    ///         - in `Probe`: accept the probe (§3.1.2) — finalize the probe tick
    ///           to the seller, return the commission, charge the fee by-fact,
    ///           and enter `Streaming` with the two-tick invariant (§3.2);
    ///         - in `Streaming`: finalize the prepaid tick and roll the invariant
    ///           (silence = consent, §3.3).
    function advance() public onlyOwnerPubkey(_sellerPubkey) accept {
        ensureBalance();
        require(_opened, ERR_NOT_OPEN);
        require(!_disputed, ERR_DISPUTED);
        // Probe phase: fixed short PROBE_WINDOW; streaming: per-deal _settleWindow.
        uint64 advanceWindow = _probeAccepted ? _settleWindow : PROBE_WINDOW;
        require(uint64(block.timestamp) >= _prepaidTime + advanceWindow, ERR_SETTLE_WINDOW_OPEN);

        uint128 fee = _fee(_pricePerTick);

        if (!_probeAccepted) {
            // Probe accepted: the probe tick (held in _frozen) → seller, fee by-fact.
            _finalizedOwed  += _frozen;
            _feeAccrued     += fee;
            _ticksFinalized += 1;
            _deposit        -= fee;
            _frozen          = 0;

            // Return the seller's probe commission (no burn — probe accepted).
            uint128 commission = _sellerProbeLocked;
            _sellerProbeLocked = 0;
            _finalizedOwed    += commission;

            _probeAccepted = true;

            // Establish the two-tick invariant from the remaining deposit:
            // prepay the next tick forward + freeze a buffer, as the deposit allows.
            if (_deposit >= _pricePerTick) {
                _prepaid  = _pricePerTick;
                _deposit -= _pricePerTick;
            } else {
                _prepaid = 0;
            }
            if (_deposit >= _pricePerTick + fee) {
                _frozen   = _pricePerTick;
                _deposit -= _pricePerTick;
            } else {
                _frozen = 0;
            }
            _prepaidTime = uint64(block.timestamp);
            _lastAdvance = uint64(block.timestamp);

            emit ProbeAccepted{dest: address.makeAddrExtern(ProbeAcceptedEmit, bitCntAddress)}(_buyer, _finalizedOwed, commission);
            return;
        }

        // Streaming: finalize the delivered tick (P → seller, fee by-fact, §5.1).
        _finalizedOwed  += _prepaid;
        _feeAccrued     += fee;
        _ticksFinalized += 1;
        _deposit        -= fee;

        _prepaid     = _frozen;
        _prepaidTime = uint64(block.timestamp);

        // Refill the buffer only if the deposit still covers a full tick (value +
        // its later by-fact fee).
        if (_deposit >= _pricePerTick + fee) {
            _frozen   = _pricePerTick;
            _deposit -= _pricePerTick;
        } else {
            _frozen = 0;
        }
        _lastAdvance = uint64(block.timestamp);

        emit TickFinalized{dest: address.makeAddrExtern(TickFinalizedEmit, bitCntAddress)}(_finalizedOwed, _deposit);
    }

    // ========================================================
    // 4. Shared streaming close (spec §4.1 under the optimistic §3.3 rule)
    // ========================================================

    /// @notice Window-gated close of the streaming phase, shared by `stop()` and
    ///         `resolveDisputeTimeout()` so the split is IDENTICAL on every streaming close
    ///         (directive 92). The current prepaid tick is finalized to the seller ONLY if its
    ///         acceptance window (`_settleWindow`) has elapsed (silence = consent, §3.3); otherwise it
    ///         is not accepted and refunds to the buyer (no fee charged for it). On the dispute-timeout
    ///         path this prevents an overpay when `_settleWindow > DISPUTE_WINDOW` (the timeout can fire
    ///         before the window elapses). Updates the finalized/fee/tick counters and zeroes the escrow
    ///         buckets; the caller does its own `_settleFees(clean)` + unlock + payout.
    function _settleStreamingClose() private returns (uint128 toSeller, uint128 refundB) {
        bool tickAccepted = _prepaid > 0 && uint64(block.timestamp) >= _prepaidTime + _settleWindow;
        if (tickAccepted) {
            uint128 fee = _fee(_pricePerTick);
            _finalizedOwed  += _prepaid;
            _feeAccrued     += fee;
            _ticksFinalized += 1;
            _deposit        -= fee;            // fee by-fact, only for the kept tick (§5.1)
            toSeller = _prepaid;
            refundB  = _frozen + _deposit;
        } else {
            // Window still open (or nothing prepaid) → buyer keeps the unaccepted tick.
            toSeller = 0;
            refundB  = _prepaid + _frozen + _deposit;
        }
        _prepaid = 0; _frozen = 0; _deposit = 0;
    }

    // ========================================================
    // 4a. Stop — buyer exit (probe burn §3.1.2 / standard split §4.1)
    // ========================================================

    function stop() public {
        ensureBalance();
        require(_opened, ERR_NOT_OPEN);
        require(msg.sender == _buyer, ERR_NOT_BUYER);
        require(!_disputed, ERR_DISPUTED);
        tvm.accept();

        if (!_probeAccepted) {
            // Stop on the probe (§3.1.2): BURN BOTH the buyer's probe tick and the
            // seller's commission — nothing to either side. Remaining deposit (no
            // fee was charged on the probe) refunds the buyer.
            uint128 burnedProbe      = _frozen;
            uint128 burnedCommission = _sellerProbeLocked;
            uint128 refund           = _deposit;
            _frozen = 0; _sellerProbeLocked = 0; _deposit = 0;
            _opened = false;

            _burnShell(burnedProbe);
            _burnShell(burnedCommission);

            IStreamNote(_buyer).streamUnlock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
            _payShell(_buyer, refund);

            emit ProbeBurned{dest: address.makeAddrExtern(ProbeBurnedEmit, bitCntAddress)}(_buyer, burnedProbe, burnedCommission, refund);
            return;
        }

        // Standard split (§4.1) under the optimistic rule (§3.3) — the shared close window-gates the
        // current tick (same gate as advance()) and is reused by resolveDisputeTimeout (directive 92).
        (uint128 toSeller, uint128 refundB) = _settleStreamingClose();
        _opened = false;

        _settleFees(true);   // clean amicable close → rebate to seller, burn net (§5.3/§5.4)

        IStreamNote(_buyer).streamUnlock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
        _payShell(_buyer, refundB);

        emit StreamStopped{dest: address.makeAddrExtern(StreamStoppedEmit, bitCntAddress)}(_buyer, toSeller, refundB);
    }

    // ========================================================
    // 4b. Dispute — symmetric, both notes lock (spec §4.2)
    // ========================================================

    function dispute() public {
        ensureBalance();
        require(_opened, ERR_NOT_OPEN);
        require(msg.sender == _buyer, ERR_NOT_BUYER);
        require(!_disputed, ERR_DISPUTED);
        tvm.accept();

        _disputed     = true;
        _everDisputed = true;   // a dispute ever opened → seller forfeits rebate (§5.3)
        _disputeTime  = uint64(block.timestamp);

        IStreamNote(_buyer).streamDisputeLock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
        IStreamNote(_sellerNote).streamDisputeLock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));

        emit StreamDisputed{dest: address.makeAddrExtern(StreamDisputedEmit, bitCntAddress)}(_buyer, _disputeTime);
    }

    /// @notice Seller concedes. In `Probe`: the probe tick goes back to the buyer
    ///         and the commission is returned to the seller (a seller concession
    ///         is not a buyer stop → no burn, §3.1.2). In `Streaming`: contested
    ///         ticks + deposit refund to the buyer.
    function releaseDispute() public onlyOwnerPubkey(_sellerPubkey) accept {
        ensureBalance();
        require(_disputed, ERR_NOT_DISPUTED);

        if (!_probeAccepted) {
            uint128 commission = _sellerProbeLocked;
            _sellerProbeLocked = 0;
            _finalizedOwed    += commission;          // commission returned to seller

            uint128 refund = _frozen + _deposit;      // probe tick back to the buyer
            _frozen = 0; _deposit = 0;
            _disputed = false; _opened = false;

            _settleFees(false);   // no fee accrued on the probe; clears state safely

            _unlockBoth();
            _payShell(_buyer, refund);

            emit DisputeResolved{dest: address.makeAddrExtern(DisputeResolvedEmit, bitCntAddress)}(0, refund, true);
            return;
        }

        uint128 refundB = _prepaid + _frozen + _deposit;
        _prepaid = 0; _frozen = 0; _deposit = 0;
        _disputed = false; _opened = false;

        _settleFees(false);   // disputed → no rebate, burn accrued fees (§5.3)

        _unlockBoth();
        _payShell(_buyer, refundB);

        emit DisputeResolved{dest: address.makeAddrExtern(DisputeResolvedEmit, bitCntAddress)}(0, refundB, true);
    }

    /// @notice Anyone, after `DISPUTE_WINDOW`. In `Probe`: reduces to the probe
    ///         rule — BURN BOTH (§3.1.2/§4.2), an unaccepted probe has no value
    ///         to either side. In `Streaming`: fall back to the standard split.
    function resolveDisputeTimeout() public {
        ensureBalance();
        require(_disputed, ERR_NOT_DISPUTED);
        require(uint64(block.timestamp) >= _disputeTime + DISPUTE_WINDOW, ERR_DISPUTE_WINDOW_OPEN);
        tvm.accept();

        if (!_probeAccepted) {
            // Probe rule: burn the probe tick and the commission; remaining
            // deposit refunds the buyer. No tick finalized, no fee.
            uint128 burnedProbe      = _frozen;
            uint128 burnedCommission = _sellerProbeLocked;
            uint128 refund           = _deposit;
            _frozen = 0; _sellerProbeLocked = 0; _deposit = 0;
            _disputed = false; _opened = false;

            _burnShell(burnedProbe);
            _burnShell(burnedCommission);

            _unlockBoth();
            _payShell(_buyer, refund);

            emit ProbeBurned{dest: address.makeAddrExtern(ProbeBurnedEmit, bitCntAddress)}(_buyer, burnedProbe, burnedCommission, refund);
            return;
        }

        // Standard split — the SAME window-gated streaming close as stop() (directive 92): the disputed
        // tick goes to the seller ONLY if its acceptance window has elapsed by the timeout, else it
        // refunds to the buyer (no overpay when _settleWindow > DISPUTE_WINDOW). Disputed → no rebate.
        (uint128 toSeller, uint128 refundB) = _settleStreamingClose();
        _disputed = false; _opened = false;

        _settleFees(false);   // disputed → no rebate, burn net (§5.3)

        _unlockBoth();
        _payShell(_buyer, refundB);

        emit DisputeResolved{dest: address.makeAddrExtern(DisputeResolvedEmit, bitCntAddress)}(toSeller, refundB, false);
    }

    function _unlockBoth() private view {
        IStreamNote(_buyer).streamDisputeUnlock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
        IStreamNote(_buyer).streamUnlock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
        IStreamNote(_sellerNote).streamDisputeUnlock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
    }

    // ========================================================
    // 4c. Reclaim on seller no-show (spec §3.4 / §3.1.2)
    // ========================================================

    function reclaimOnTimeout() public {
        ensureBalance();
        require(_opened, ERR_NOT_OPEN);
        require(msg.sender == _buyer, ERR_NOT_BUYER);
        require(!_disputed, ERR_DISPUTED);
        require(uint64(block.timestamp) >= _lastAdvance + _streamTimeout, ERR_STREAM_TIMEOUT_OPEN);
        tvm.accept();

        if (!_probeAccepted) {
            // Seller no-show on the probe (§3.1.2/§3.4): the buyer reclaims the
            // probe tick in full (pays nothing), the commission is returned to
            // the seller. NO burn — a no-show is not slashed (§9.1).
            uint128 commission = _sellerProbeLocked;
            _sellerProbeLocked = 0;
            _finalizedOwed    += commission;

            uint128 refund = _frozen + _deposit;
            _frozen = 0; _deposit = 0;
            _opened = false;

            IStreamNote(_buyer).streamUnlock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
            _payShell(_buyer, refund);

            emit StreamReclaimed{dest: address.makeAddrExtern(StreamReclaimedEmit, bitCntAddress)}(_buyer, refund);
            return;
        }

        // Seller delivered the prepaid tick before vanishing → P + fee finalized;
        // buyer reclaims the buffer + remaining deposit. No rebate (abandoned).
        uint128 fee = _fee(_pricePerTick);
        _finalizedOwed  += _prepaid;
        _feeAccrued     += fee;
        _ticksFinalized += 1;
        _deposit        -= fee;

        uint128 refundB = _frozen + _deposit;
        _prepaid = 0; _frozen = 0; _deposit = 0;
        _opened  = false;

        _settleFees(false);   // seller abandoned → no rebate, burn net

        IStreamNote(_buyer).streamUnlock{value: DAPP_MSG_VALUE, flag: 1, bounce: false}(address(this));
        _payShell(_buyer, refundB);

        emit StreamReclaimed{dest: address.makeAddrExtern(StreamReclaimedEmit, bitCntAddress)}(_buyer, refundB);
    }

    // ========================================================
    // 4d. Cleanup — seller funded-but-never-opened (no-show, spec §2.1)
    // ========================================================

    /// @notice Anyone, after `MATCH_OPEN_TIMEOUT` with no open(): refund the
    ///         buyer's full deposit and return any posted probe commission to
    ///         the seller (nothing delivered → no fee, no penalty, §2.1), then
    ///         self-destruct the dead deal.
    /// @dev    Permissionless (no-show recovery), so the payout is NOT caller-chosen: the buyer's
    ///         deposit + seller commission (ECC SHELL) are refunded to their fixed notes FIRST, then
    ///         the residual native gas is swept to the canonical SuperRoot (a fixed protocol sink),
    ///         never to an arbitrary caller-supplied address.
    function cleanupUnopened() public {
        ensureBalance();
        require(_funded, ERR_NOT_FUNDED);
        require(!_opened, ERR_ALREADY_OPEN);
        require(uint64(block.timestamp) >= _fundedTime + MATCH_OPEN_TIMEOUT, ERR_STREAM_TIMEOUT_OPEN);
        tvm.accept();

        uint128 refund     = _deposit;
        uint128 commission = _sellerProbeLocked;
        _deposit = 0; _sellerProbeLocked = 0; _funded = false; _sellerProbeFunded = false;

        _payShell(_buyer, refund);
        _payShell(_sellerNote, commission);   // return the seller's probe commission

        emit ContractDestroyed{dest: address.makeAddrExtern(ContractDestroyedEmit, bitCntAddress)}(address(this));
        selfdestruct(address.makeAddrStd(0, SUPER_ROOT_ADDR));   // residual native → fixed SuperRoot, not caller
    }

    // ========================================================
    // 5. Seller withdraw + destroy
    // ========================================================

    function withdrawShell(uint128 amount, address recipient) public onlyOwnerPubkey(_sellerPubkey) accept {
        ensureBalance();
        require(amount > 0, ERR_ZERO_AMOUNT);
        require(amount <= _finalizedOwed, ERR_INSUFFICIENT_TOKENS);
        uint128 balance = uint128(address(this).currencies[SHELL_ECC_ID]);
        require(amount <= balance, ERR_INSUFFICIENT_TOKENS);

        _finalizedOwed -= amount;
        _payShell(recipient, amount);

        emit ShellWithdrawn{dest: address.makeAddrExtern(ShellWithdrawnEmit, bitCntAddress)}(recipient, amount);
    }

    function destroy(address payoutAddress) public onlyOwnerPubkey(_sellerPubkey) accept {
        require(!_opened, ERR_STILL_OPEN);
        require(!_disputed, ERR_DISPUTED);
        // Never selfdestruct over a live buyer deposit: a matched-but-unopened deal
        // (_funded && !_opened) still holds the buyer's escrowed SHELL, which selfdestruct would
        // sweep to the seller-chosen payoutAddress. Refund the buyer (and return the seller's probe
        // commission) first, mirroring cleanupUnopened, so the sweep only takes residual native gas.
        if (_funded) {
            uint128 refund     = _deposit;
            uint128 commission = _sellerProbeLocked;
            _deposit = 0; _sellerProbeLocked = 0; _funded = false; _sellerProbeFunded = false;
            _payShell(_buyer, refund);
            _payShell(_sellerNote, commission);
        }
        emit ContractDestroyed{dest: address.makeAddrExtern(ContractDestroyedEmit, bitCntAddress)}(address(this));
        selfdestruct(payoutAddress);
    }

    // ========================================================
    // Getters
    // ========================================================

    function getState() external view returns (
        bool funded, bool opened, bool probeAccepted, bool disputed,
        uint128 deposit, uint128 prepaid, uint128 frozen, uint128 finalizedOwed,
        uint64 prepaidTime, uint64 lastAdvance, uint64 disputeTime, uint64 fundedTime
    ) {
        return (_funded, _opened, _probeAccepted, _disputed, _deposit, _prepaid, _frozen,
                _finalizedOwed, _prepaidTime, _lastAdvance, _disputeTime, _fundedTime);
    }

    /// @notice Probe state (spec §3.1.2): whether the seller posted the
    ///         commission and the SHELL amount currently locked as it.
    function getProbe() external view returns (bool probeFunded, uint128 probeLocked, uint128 probeCommission) {
        return (_sellerProbeFunded, _sellerProbeLocked, _probeCommission());
    }

    function getConfig() external view returns (
        uint16 platformFeeBps, uint64 settleWindow, uint64 streamTimeout, uint64 disputeWindow
    ) {
        return (PLATFORM_FEE_BPS, _settleWindow, _streamTimeout, DISPUTE_WINDOW);
    }

    /// @notice Fee state (spec §5): accrued fee, finalized-tick count (rebate n),
    ///         whether a dispute ever opened, and the rebate config.
    function getFees() external view returns (
        uint128 feeAccrued, uint128 ticksFinalized, bool everDisputed,
        uint16 rebateMaxBps, uint16 rebateSlopeBps
    ) {
        return (_feeAccrued, _ticksFinalized, _everDisputed, REBATE_MAX_BPS, REBATE_SLOPE_BPS);
    }

    function getDeal() external view returns (uint128 tickSize, uint128 pricePerTick, uint128 maxTicks) {
        return (_tickSize, _pricePerTick, _maxTicks);
    }

    function getParties() external view returns (address buyer, address sellerNote) {
        return (_buyer, _sellerNote);
    }

    /// @notice Buyer note pubkey recorded at the match (spec §3.1.1): the gateway
    ///         verifies the buyer's challenge signature against this.
    function getBuyerPubkey() external view returns (uint256) { return _buyerPubkey; }

    function getEndpointCipher() external view returns (bytes) { return _endpointCipher; }

    function getModelName() external view returns (string) { return _modelName; }
    function getModelHash() external view returns (uint256) { return _modelHash; }

    function getShellBalance() external view returns (uint128) {
        return uint128(address(this).currencies[SHELL_ECC_ID]);
    }

    function getSeller() external view returns (uint256 sellerPubkey, address rootModelAddress, uint64 nonce) {
        return (_sellerPubkey, _rootModelAddress, _nonce);
    }

    function getVersion() external pure returns (string, string) {
        return (version, "TokenContract");
    }
}
