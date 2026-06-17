pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./interfaces.sol";

/// @title TokenContract (streaming deal / `token_contract` per spec §3-4)
/// @notice One streaming inference deal between one seller (owner) and one
///         buyer. Payment is in ECC[2] SHELL, escrowed in this contract;
///         identities/locks ride on PrivateNotes (model B). Implements the
///         two-tick invariant with seller-driven optimistic acceptance and a
///         symmetric dispute.
///
///         Tick value (SHELL) = `_pricePerTick`. At any open moment exactly one
///         tick is prepaid to the seller (delivered, awaiting finalization) and
///         exactly one tick is frozen as buffer (spec §3.2).
///
///         Timing windows and the platform fee are PROTOCOL CONSTANTS
///         (`SETTLE_WINDOW`/`STREAM_TIMEOUT`/`DISPUTE_WINDOW`/`PLATFORM_FEE_BPS`
///         in modifiers.sol), not per-deal parameters.
///
///         Lifecycle:
///         1. `fund()`/`fundFromOrderBook()` — buyer escrows SHELL.
///         2. `open(cipher)` — seller posts the endpoint encrypted to the
///                             buyer's pubkey, charges the 1% fee, sets the
///                             two-tick invariant, locks the buyer note.
///         3. `advance()`    — seller-driven: after `SETTLE_WINDOW` of buyer
///                             silence, finalize the prepaid tick.
///         4a.`stop()`       — buyer amicable exit (spec §4.1).
///         4b.`dispute()`    — buyer contests <=2 live ticks; both notes lock;
///                             `resolveDisputeTimeout()`/`releaseDispute()`.
///         4c.`reclaimOnTimeout()` — seller no-show after `STREAM_TIMEOUT`.
///         5. `withdrawShell`/`destroy` — seller pulls finalized SHELL (§3.5).
contract TokenContract is AiRegistryModifiers {
    string constant version = "2.1.0";

    event ContractDeployed(address self);
    event StreamFunded(address buyer, uint128 deposit);
    event StreamOpened(address buyer, uint128 pricePerTick);
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
    uint128 _tickSize;        // tokens per tick (informational for the buyer)
    uint128 _pricePerTick;    // SHELL per tick (P)
    uint128 _maxTicks;        // upper bound on ticks this deal serves

    // Deal state.
    address _buyer;           // buyer note address (funder; payouts/locks)
    address _sellerNote;      // seller note address (dispute lock)
    bytes   _endpointCipher;  // endpoint encrypted to the buyer's pubkey

    bool    _funded;
    bool    _opened;
    bool    _disputed;

    uint128 _deposit;         // SHELL available for future ticks (value + reserved fee)
    uint128 _prepaid;         // SHELL: the delivered, not-yet-finalized tick (value P)
    uint128 _frozen;          // SHELL: the buffer tick (value P)
    uint128 _finalizedOwed;   // SHELL finalized to the seller (withdrawable; incl. rebate)
    uint128 _feeAccrued;      // SHELL fee charged by-fact on finalized ticks (§5.1)
    uint128 _ticksFinalized;  // count of finalized ticks (n for rebate §5.3)
    bool    _everDisputed;    // a dispute ever opened → no rebate (§5.3)
    uint64  _fundedTime;      // when funded (MATCH_OPEN_TIMEOUT cleanup, §2.1)
    uint64  _prepaidTime;     // when `_prepaid` was delivered (settle window)
    uint64  _lastAdvance;     // last seller activity (stream timeout)
    uint64  _disputeTime;     // when the dispute opened

    constructor(
        string  modelName,
        uint128 tickSize,
        uint128 pricePerTick,
        uint128 maxTicks,
        address sellerNote
    ) {
        tvm.accept();
        require(tickSize > 0, ERR_BAD_PARAM);
        require(pricePerTick > 0, ERR_BAD_PARAM);
        require(maxTicks >= 2, ERR_BAD_PARAM);

        _modelName    = modelName;
        _tickSize     = tickSize;
        _pricePerTick = pricePerTick;
        _maxTicks     = maxTicks;
        _sellerNote   = sellerNote;

        ensureBalance();

        address selfExtern = address.makeAddrExtern(ContractDeployedEmit, bitCntAddress);
        emit ContractDeployed{dest: selfExtern}(address(this));

        IRootModelRegistry(_rootModelAddress).registerTokenContract{value: REGISTER_FORWARD_VALUE, flag: 1}(_sellerPubkey, _nonce);
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

    /// @notice Platform fee (2.5%, PLATFORM_FEE_BPS) of `amount` (spec §5.1).
    function _fee(uint128 amount) private pure returns (uint128) {
        return uint128(uint256(amount) * uint256(PLATFORM_FEE_BPS) / uint256(BPS_DENOMINATOR));
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
        if (netBurn > 0 && netBurn <= uint128(type(uint64).max)) {
            gosh.burnecc(uint64(netBurn), SHELL_ECC_ID);
        }
        _feeAccrued = 0;
    }

    // ========================================================
    // 1. Fund — buyer escrows SHELL, locks the deal
    // ========================================================

    function _recordFunding(address buyer, uint128 paid) private {
        // Buyer-side, by-fact fee (§5.1): the escrow covers per-tick (P + fee).
        // Must cover >= 2 full ticks (the two-tick invariant) and <= maxTicks.
        uint128 unit = _pricePerTick + _fee(_pricePerTick);
        require(paid >= 2 * unit, ERR_INSUFFICIENT_DEPOSIT);
        require(paid <= _maxTicks * unit, ERR_OVERFLOW);
        _buyer      = buyer;
        _deposit    = paid;
        _funded     = true;
        _fundedTime = uint64(block.timestamp);
        emit StreamFunded{dest: address.makeAddrExtern(StreamFundedEmit, bitCntAddress)}(buyer, paid);
    }

    /// @notice Buyer sends ECC[2] SHELL to escrow the deal (direct path).
    function fund() public {
        ensureBalance();
        require(!_funded, ERR_ALREADY_FUNDED);
        mapping(uint32 => varuint32) currencies = msg.currencies;
        require(currencies.exists(SHELL_ECC_ID), ERR_NO_SHELL);
        tvm.accept();
        _recordFunding(msg.sender, uint128(currencies[SHELL_ECC_ID]));
    }

    /// @notice Order-book handover (spec §2.3): the InferenceOrderBook forwards
    ///         the matched SHELL and binds the buyer note (not msg.sender).
    function fundFromOrderBook(address buyerNote) public {
        ensureBalance();
        require(!_funded, ERR_ALREADY_FUNDED);
        mapping(uint32 => varuint32) currencies = msg.currencies;
        require(currencies.exists(SHELL_ECC_ID), ERR_NO_SHELL);
        tvm.accept();
        _recordFunding(buyerNote, uint128(currencies[SHELL_ECC_ID]));
    }

    // ========================================================
    // 2. Open — seller posts encrypted endpoint, sets two-tick invariant
    // ========================================================

    /// @notice Seller-only. Posts the endpoint ciphertext, charges the 1%
    ///         platform fee from the deposit (spec §3.1), prepays one tick +
    ///         freezes one tick (spec §3.2), and locks the buyer note.
    function open(bytes endpointCipher) public onlyOwnerPubkey(_sellerPubkey) accept {
        ensureBalance();
        require(_funded, ERR_NOT_FUNDED);
        require(!_opened, ERR_ALREADY_OPEN);

        _endpointCipher = endpointCipher;

        // No upfront fee (§5.1: by-fact, charged per finalized tick in advance/stop).
        // Two-tick invariant: reserve two tick VALUES; their fees stay in _deposit
        // and are charged on finalization.
        require(_deposit >= 2 * (_pricePerTick + _fee(_pricePerTick)), ERR_INSUFFICIENT_DEPOSIT);
        _prepaid     = _pricePerTick;
        _frozen      = _pricePerTick;
        _deposit    -= 2 * _pricePerTick;
        _prepaidTime = uint64(block.timestamp);
        _lastAdvance = uint64(block.timestamp);
        _opened      = true;

        IStreamNote(_buyer).streamLock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));

        emit StreamOpened{dest: address.makeAddrExtern(StreamOpenedEmit, bitCntAddress)}(_buyer, _pricePerTick);
    }

    // ========================================================
    // 3. Advance — seller finalizes accepted tick and rolls the invariant
    // ========================================================

    /// @notice Seller-only. After `SETTLE_WINDOW` of buyer silence the prepaid
    ///         tick is accepted (silence = consent) and finalized; the buffer
    ///         becomes the new prepaid and a fresh tick is frozen from deposit.
    function advance() public onlyOwnerPubkey(_sellerPubkey) accept {
        ensureBalance();
        require(_opened, ERR_NOT_OPEN);
        require(!_disputed, ERR_DISPUTED);
        require(uint64(block.timestamp) >= _prepaidTime + SETTLE_WINDOW, ERR_SETTLE_WINDOW_OPEN);

        // Finalize the delivered tick: P → seller, fee charged by-fact (§5.1).
        uint128 fee = _fee(_pricePerTick);
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
    // 4a. Stop — buyer amicable exit (spec §4.1)
    // ========================================================

    function stop() public {
        ensureBalance();
        require(_opened, ERR_NOT_OPEN);
        require(msg.sender == _buyer, ERR_NOT_BUYER);
        require(!_disputed, ERR_DISPUTED);
        tvm.accept();

        // Finalize the delivered tick (P → seller, fee by-fact), refund the
        // buffer + remaining deposit to the buyer (spec §4.1).
        uint128 fee = _fee(_pricePerTick);
        _finalizedOwed  += _prepaid;
        _feeAccrued     += fee;
        _ticksFinalized += 1;
        _deposit        -= fee;

        uint128 refund   = _frozen + _deposit;
        uint128 toSeller = _prepaid;
        _prepaid = 0; _frozen = 0; _deposit = 0;
        _opened  = false;

        _settleFees(true);   // clean amicable close → rebate to seller, burn net (§5.3/§5.4)

        IStreamNote(_buyer).streamUnlock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));
        _payShell(_buyer, refund);

        emit StreamStopped{dest: address.makeAddrExtern(StreamStoppedEmit, bitCntAddress)}(_buyer, toSeller, refund);
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

        IStreamNote(_buyer).streamDisputeLock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));
        IStreamNote(_sellerNote).streamDisputeLock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));

        emit StreamDisputed{dest: address.makeAddrExtern(StreamDisputedEmit, bitCntAddress)}(_buyer, _disputeTime);
    }

    /// @notice Seller concedes: contested ticks + deposit refund to the buyer.
    function releaseDispute() public onlyOwnerPubkey(_sellerPubkey) accept {
        ensureBalance();
        require(_disputed, ERR_NOT_DISPUTED);

        uint128 refund = _prepaid + _frozen + _deposit;
        _prepaid = 0; _frozen = 0; _deposit = 0;
        _disputed = false; _opened = false;

        _settleFees(false);   // disputed → no rebate, burn accrued fees (§5.3)

        _unlockBoth();
        _payShell(_buyer, refund);

        emit DisputeResolved{dest: address.makeAddrExtern(DisputeResolvedEmit, bitCntAddress)}(0, refund, true);
    }

    /// @notice Anyone, after `DISPUTE_WINDOW`: fall back to the standard split.
    function resolveDisputeTimeout() public {
        ensureBalance();
        require(_disputed, ERR_NOT_DISPUTED);
        require(uint64(block.timestamp) >= _disputeTime + DISPUTE_WINDOW, ERR_DISPUTE_WINDOW_OPEN);
        tvm.accept();

        // Standard split: delivered tick → seller (P + fee by-fact), buffer +
        // remaining deposit → buyer. Disputed, so no rebate.
        uint128 fee = _fee(_pricePerTick);
        _finalizedOwed  += _prepaid;
        _feeAccrued     += fee;
        _ticksFinalized += 1;
        _deposit        -= fee;

        uint128 refund   = _frozen + _deposit;
        uint128 toSeller = _prepaid;
        _prepaid = 0; _frozen = 0; _deposit = 0;
        _disputed = false; _opened = false;

        _settleFees(false);   // disputed → no rebate, burn net (§5.3)

        _unlockBoth();
        _payShell(_buyer, refund);

        emit DisputeResolved{dest: address.makeAddrExtern(DisputeResolvedEmit, bitCntAddress)}(toSeller, refund, false);
    }

    function _unlockBoth() private view {
        IStreamNote(_buyer).streamDisputeUnlock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));
        IStreamNote(_buyer).streamUnlock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));
        IStreamNote(_sellerNote).streamDisputeUnlock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));
    }

    // ========================================================
    // 4c. Reclaim on seller no-show (spec §3.4)
    // ========================================================

    function reclaimOnTimeout() public {
        ensureBalance();
        require(_opened, ERR_NOT_OPEN);
        require(msg.sender == _buyer, ERR_NOT_BUYER);
        require(!_disputed, ERR_DISPUTED);
        require(uint64(block.timestamp) >= _lastAdvance + STREAM_TIMEOUT, ERR_STREAM_TIMEOUT_OPEN);
        tvm.accept();

        // Seller delivered the prepaid tick before vanishing → P + fee finalized;
        // buyer reclaims the buffer + remaining deposit. No rebate (abandoned).
        uint128 fee = _fee(_pricePerTick);
        _finalizedOwed  += _prepaid;
        _feeAccrued     += fee;
        _ticksFinalized += 1;
        _deposit        -= fee;

        uint128 refund = _frozen + _deposit;
        _prepaid = 0; _frozen = 0; _deposit = 0;
        _opened  = false;

        _settleFees(false);   // seller abandoned → no rebate, burn net

        IStreamNote(_buyer).streamUnlock{value: REGISTER_FORWARD_VALUE, flag: 1, bounce: false}(address(this));
        _payShell(_buyer, refund);

        emit StreamReclaimed{dest: address.makeAddrExtern(StreamReclaimedEmit, bitCntAddress)}(_buyer, refund);
    }

    // ========================================================
    // 4d. Cleanup — seller funded-but-never-opened (no-show, spec §2.1)
    // ========================================================

    /// @notice Anyone, after `MATCH_OPEN_TIMEOUT` with no open(): refund the
    ///         buyer's full deposit (nothing delivered → no fee, no penalty,
    ///         §2.1) and self-destruct the dead deal.
    function cleanupUnopened(address payoutAddress) public {
        ensureBalance();
        require(_funded, ERR_NOT_FUNDED);
        require(!_opened, ERR_ALREADY_OPEN);
        require(uint64(block.timestamp) >= _fundedTime + MATCH_OPEN_TIMEOUT, ERR_STREAM_TIMEOUT_OPEN);
        tvm.accept();

        uint128 refund = _deposit;
        _deposit = 0; _funded = false;
        _payShell(_buyer, refund);

        emit ContractDestroyed{dest: address.makeAddrExtern(ContractDestroyedEmit, bitCntAddress)}(address(this));
        selfdestruct(payoutAddress);
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
        emit ContractDestroyed{dest: address.makeAddrExtern(ContractDestroyedEmit, bitCntAddress)}(address(this));
        selfdestruct(payoutAddress);
    }

    // ========================================================
    // Getters
    // ========================================================

    function getState() external view returns (
        bool funded, bool opened, bool disputed,
        uint128 deposit, uint128 prepaid, uint128 frozen, uint128 finalizedOwed,
        uint64 prepaidTime, uint64 lastAdvance, uint64 disputeTime
    ) {
        return (_funded, _opened, _disputed, _deposit, _prepaid, _frozen,
                _finalizedOwed, _prepaidTime, _lastAdvance, _disputeTime);
    }

    function getConfig() external pure returns (
        uint16 platformFeeBps, uint64 settleWindow, uint64 streamTimeout, uint64 disputeWindow
    ) {
        return (PLATFORM_FEE_BPS, SETTLE_WINDOW, STREAM_TIMEOUT, DISPUTE_WINDOW);
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

    function getEndpointCipher() external view returns (bytes) { return _endpointCipher; }

    function getModelName() external view returns (string) { return _modelName; }

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
