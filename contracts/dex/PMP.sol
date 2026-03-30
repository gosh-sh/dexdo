pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PrivateNote.sol";
import "./OracleEventList.sol";
import "./OrderBook.sol";
import "./libraries/DexLib.sol";

/// @title PMP - Pari Mutuel Pool
/// @notice PMP contract with creator fee, requires oracle approval
contract PMP is Modifiers {

    /// @notice Contract semantic version.
    string constant version = "1.0.2";

    /// @notice PMP name (static, unique identifier)
    string _name;

    /// @notice Describe of event
    string _describe;

    /// @notice Token type (static)
    uint32 static _token_type;

    /// @notice Event identifier
    uint256 static _event_id;

    /// @notice Oracle list hash (static)
    uint256 static _oracle_list_hash;

    /// @notice Contract deployer (PrivateNote address)
    address _deployer;

    /// @notice PrivateNote code for address computation
    TvmCell _PrivateNoteCode;

    /// @notice Total pool of all stakes
    uint128 _totalPool;

    /// @notice Pools separated by bet type
    /// @dev Structure: outcome => bet_type => pool_amount
    mapping(uint32 => mapping(uint8 => uint128)) _typedOutcomePools;

    /// @notice Stake counts separated by bet type
    /// @dev Structure: outcome => bet_type => stake_count
    mapping(uint32 => mapping(uint8 => uint128)) _typedOutcomeCounts;

    /// @notice Total coupon pool across all outcomes
    /// @dev Used to enforce COUPON_POOL_LIMIT_PERCENT
    uint128 _totalCouponPool;

    /// @notice Total pool for clean bets
    uint128 _totalCleanPool;

    /// @notice Total pool for debt bets
    uint128 _totalDebtPool;

    /// @notice Coefficient for calculating coupon winnings, set at resolution
    uint128 _couponWinCoef;

    /// @notice Coefficient for calculating debt winnings, set at resolution
    uint128 _debtWinCoef;

    /// @notice Total winnings from the winning outcome, used for payout calculations
    uint128 _totalWinPool;

    /// @notice Profit allocated to clean bets after resolve
    uint128 _profitToClean;

    /// @notice Creator fee collected at resolve, sent to _deployer
    uint128 _creatorFee;

    /// @notice Finalized outcome
    optional(uint32) _resolvedOutcome;

    /// @notice Approval flag from oracle
    bool _approved;

    /// @notice Number of outcome intervals (set by oracle)
    uint32 _numOutcomes;

    /// @notice Outcome names
    mapping(uint32 => string) _outcomeNames;

    /// ===== Time windows (set by oracle) =====

    /// @notice Stake acceptance start time (set once, immutable after start)
    uint64 _stakeStart;

    /// @notice Result acceptance start time (resolve deadline = _resultStart + GRACE_PERIOD)
    uint64 _resultStart;

    /// @notice Cancellation flag
    bool _isCancelled;

    /// @notice Mapping of confirmed oracle events
    mapping(uint256 => bool) _oracleEventsConfirmed;

    /// @notice Mapping of oracle event list public keys
    mapping(uint256 => bool) _oracleEventsPubkeys;
    
    /// @notice Mapping of oracle event list addresses
    mapping(uint256 => uint256) _oracleEventsAddress;

    /// @notice Number of oracle events
    uint128 _numberOfOracleEvents;

    /// @notice Number of approved oracle events
    uint128 _approvedOracleEvents;

    // ===== Consensus state for setTimings =====

    /// @notice Per-oracle hash of submitted timing params (oracle pubkey => hash)
    mapping(uint256 => uint256) _timingsOracleHash;

    /// @notice Count of oracles per hash of timing params (hash => count)
    mapping(uint256 => uint128) _timingsHashCount;

    // ===== Consensus state for resolve =====

    /// @notice Per-oracle hash of submitted resolve params (oracle pubkey => hash)
    mapping(uint256 => uint256) _resolveOracleHash;

    /// @notice Count of oracles per hash of resolve params (hash => count)
    mapping(uint256 => uint128) _resolveHashCount;

    // ===== Consensus state for cancelEvent =====

    /// @notice Tracks which oracles have voted to cancel (oracle pubkey => voted)
    mapping(uint256 => bool) _cancelOracleVoted;

    /// @notice Total number of cancel votes
    uint128 _cancelVoteCount;

    /// @notice Initial per-outcome stakes submitted at deploy time (before numOutcomes known)
    uint128[] _initialStakes;

    // ===== Split/Merge state =====

    /// @notice Whether base pools have been frozen (snapshot taken at stakeEnd)
    bool _frozen;

    /// @notice Snapshot of _totalPool at freeze time (clean + debt only)
    uint128 _baseTotalPool;

    /// @notice Frozen clean pool per outcome at stakeEnd (M_k in spec)
    mapping(uint32 => uint128) _frozenCleanPools;

    /// @notice Frozen debt pool per outcome at stakeEnd (D_k in spec)
    mapping(uint32 => uint128) _frozenDebtPools;

    // ===== Pre-computed per-outcome payout coefficients (frozen) =====

    /// @notice Frozen coupon win coefficient per outcome (K_coupon_k)
    mapping(uint32 => uint128) _frozenCouponWinCoef;

    /// @notice Frozen debt win coefficient per outcome (K_debt_k)
    mapping(uint32 => uint128) _frozenDebtWinCoef;

    /// @notice Frozen profit allocated to clean bets per outcome
    mapping(uint32 => uint128) _frozenProfitToClean;

    /// @notice Frozen creator fee per outcome
    mapping(uint32 => uint128) _frozenCreatorFee;

    /// @notice Maximum fixed obligation across all outcomes (for merge solvency)
    uint128 _maxFixedObligation;

    /// @notice OrderBook contract code for deployment
    TvmCell _orderBookCode;

    /// Events

    /// @notice Emitted when a stake is accepted and accounted into the pool.
    /// @dev `note` is a PrivateNote wallet address that sent the stake.
    /// @param note PrivateNote address (wallet) that placed the stake.
    /// @param outcomeId Outcome identifier the stake is placed on.
    /// @param amount Stake amount added to the pool.
    /// @param bet_type 0 - clean bet, 1 - debt bet, 2 - coupon bet
    event StakeAccepted(address indexed note, uint32 outcomeId, uint128 amount, uint8 bet_type);

    /// @notice Emitted when the event is fully approved by oracle(s).
    /// @dev This event is emitted only when all required oracle confirmations are collected.
    /// @param oracleEventList Address of the oracle event list that triggered the last required confirmation.
    /// @param oraclePubkey Public key of the oracle that confirmed/approved the event.
    event ApprovedByOracle(address oracleEventList, uint256 oraclePubkey);

    /// @notice Emitted when the event outcome is resolved.
    /// @param outcomeId Final outcome identifier.
    event Resolved(uint32 outcomeId);

    /// @notice Emitted when a claim is processed (either payout or rejection).
    /// @dev In this implementation the event is emitted only after resolution logic is executed.
    /// @param note PrivateNote address (wallet) that claimed.
    /// @param payout Calculated payout amount (0 if no payout).
    /// @param win True if the claim is winning and payout > 0.
    event ClaimProcessed(address indexed note, uint128 payout, bool win);

    /// @notice Emitted when network fee was burned.
    /// @dev The contract currently does not emit this event in the shown code; reserved for future accounting.
    /// @param amount Burned fee amount in native units.
    event NetworkFeeBurned(uint64 amount);

    /// @notice Emitted when stake/result time windows are set and the event becomes approved for staking.
    /// @param stakeStart Stake acceptance start timestamp.
    /// @param stakeEnd Stake acceptance end timestamp.
    /// @param resultStart Result acceptance start timestamp.
    /// @param resultEnd Result acceptance end timestamp.
    event TimingsSet(uint64 stakeStart, uint64 stakeEnd, uint64 resultStart, uint64 resultEnd);

    /// @notice Emitted when number of outcomes is set.
    /// @dev The contract currently derives `_numOutcomes` from `outcomeNames` and does not emit this event.
    /// @param numOutcomes Number of available outcomes.
    event NumOutcomesSet(uint32 numOutcomes);

    /// @notice Emitted when the event is cancelled by oracle governance.
    event EventCancelled();

    /// @notice Emitted when a PMP is cancelled by oracle.
    event PMPCancelled();

    /// @notice Emitted when creator fee is collected at resolution.
    /// @param fee Fee amount credited to deployer.
    event CreatorFeeCollected(uint128 fee);

    /// @notice Emitted when base pools are frozen at stakeEnd
    event PoolsFrozen(uint128 baseTotalPool);

    /// @notice Emitted when a split is processed
    /// @param note PrivateNote address
    /// @param collateral Collateral amount used for split
    event SplitProcessed(address indexed note, uint128 collateral);

    /// @notice Emitted when a merge is processed
    /// @param note PrivateNote address
    /// @param collateral Collateral amount returned from merge
    event MergeProcessed(address indexed note, uint128 collateral);


    /// @notice PMP constructor
    /// @param deposit_identifier_hash Deposit identifier hash from PrivateNote
    /// @param token_type Token type of collateral used by this PMP.
    /// @param oracle_event_lists OracleEventList contracts that must confirm this event.
    /// @param oracle_fees Per-oracle shell fees transferred during confirmation.
    /// @param initialStakes Per-outcome initial clean stakes from deployer (validated in approveEvent)
    /// @param orderBookCode OrderBook contract code used for deterministic OrderBook address.
    constructor(uint256 deposit_identifier_hash, uint32 token_type, address[] oracle_event_lists, uint128[] oracle_fees, uint128[] initialStakes, TvmCell orderBookCode) {
        tvm.accept();
        _token_type = token_type;
        TvmCell salt = abi.codeSalt(tvm.code()).get();
        (TvmCell PrivateNoteCode) = abi.decode(salt, (TvmCell));
        _PrivateNoteCode = PrivateNoteCode;
        _orderBookCode = orderBookCode;
        _approved = false;
        _deployer = msg.sender;
        _numOutcomes = 0; // Initialize with 0 outcomes
        _initialStakes = initialStakes;

        address expectedNote = DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
        require(msg.sender == expectedNote, ERR_INVALID_SENDER);
        _numberOfOracleEvents = uint128(oracle_event_lists.length);
        for (uint32 i = 0; i < oracle_event_lists.length; i++) {
            mapping(uint32 => varuint32) data;
            data[CURRENCIES_ID_SHELL] = oracle_fees[i];
            _oracleEventsConfirmed[oracle_event_lists[i].value] = false;
            OracleEventList(oracle_event_lists[i]).confirmEvent{
                value: 0.1 vmshell,
                flag: 1,
                currencies: data
            }(_event_id, _oracle_list_hash, _token_type);
        }
    }

    /// @notice Rejects the event and self-destructs the contract
    function rejectEvent() public {
        require(_oracleEventsConfirmed.exists(msg.sender.value), ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();
        address addrExtern = address.makeAddrExtern(PMP_CANCELLED_BY_ORACLE, bitCntAddress);
        emit PMPCancelled{dest: addrExtern}();
        
        for ((uint256 key, bool value) : _oracleEventsConfirmed) {
            if (value == true) {
                OracleEventList(address.makeAddrStd(0, key)).cancelEvent{
                    value: 0.1 vmshell,
                    flag: 1
                }(_event_id, _oracle_list_hash, _token_type);
            }
        }

        // If no approveEvent was called yet, initial stakes are still locked in PN (_busy set).
        // Refund the deployer to unblock the note before self-destructing.
        if (_approvedOracleEvents == 0 && _initialStakes.length > 0) {
            uint128 refundTotal = 0;
            for (uint32 i = 0; i < _initialStakes.length; i++) {
                refundTotal += _initialStakes[i];
            }
            PrivateNote(_deployer).onInitialStakesFailed{value: 0.1 vmshell, flag: 1}(
                _event_id, _oracle_list_hash, _token_type, refundTotal
            );
        }

        selfdestruct(ROOT_PN_ADDRESS);
    }

    /// @notice Confirms and approves the PMP event by an oracle
    /// @dev
    /// - Callable only by registered OracleEventList contract addresses.
    /// - Each oracle can approve the event only once (duplicate approvals are ignored).
    /// - On every call, updates the pool name (`_name`).
    /// - On the first approval only, initializes event metadata:
    ///   - event description (`_describe`)
    ///   - outcome names mapping (`_outcomeNames`)
    ///   - number of outcomes (`_numOutcomes`)
    /// - Optionally binds an internal trusted address to the oracle public key
    ///   for internal governance message authorization.
    /// - When all required oracle approvals are collected, emits `ApprovedByOracle`.
    ///
    /// @param oracle_pubkey Oracle public key used as a unique oracle identifier
    /// @param outcomeNames Mapping of outcome identifiers to human-readable names;
    ///        used only on the first approval call
    /// @param describe Human-readable description of the event;
    ///        used only on the first approval call
    /// @param name Pool name;
    ///        updated on every approval call (last call wins)
    /// @param trustAddr Optional trusted internal address to bind with `oracle_pubkey`
    function approveEvent(uint256 oracle_pubkey, mapping(uint32 => string) outcomeNames, string describe, string name, optional(uint256) trustAddr) public {
        require(_oracleEventsConfirmed.exists(msg.sender.value), ERR_INVALID_SENDER);
        if (_oracleEventsConfirmed[msg.sender.value] == true) {
            return;
        } 
        if (_approvedOracleEvents >= _numberOfOracleEvents) {
            return;
        }
        tvm.accept();
        ensureBalance();
        _name = name;
        if (_approvedOracleEvents == 0){
            _describe = describe;
            _outcomeNames = outcomeNames;
            _numOutcomes = uint32(outcomeNames.keys().length);

            // Validate and process deployer's initial stakes (must cover all outcomes)
            if (_initialStakes.length != _numOutcomes) {
                // Length mismatch → refund deployer and cancel PMP
                uint128 refundTotal = 0;
                for (uint32 i = 0; i < _initialStakes.length; i++) {
                    refundTotal += _initialStakes[i];
                }
                PrivateNote(_deployer).onInitialStakesFailed{value: 0.1 vmshell, flag: 1}(
                    _event_id, _oracle_list_hash, _token_type, refundTotal
                );
                selfdestruct(ROOT_PN_ADDRESS);
                return;
            }
            // Add initial stakes to the clean pool; mark deployer as covering all outcomes
            uint128 initialTotal = 0;
            for (uint32 i = 0; i < _numOutcomes; i++) {
                require(_initialStakes[i] > 0, ERR_ZERO_TOKEN_AMOUNT);
                _typedOutcomePools[i][BET_TYPE_CLEAN] += _initialStakes[i];
                initialTotal += _initialStakes[i];
            }
            _totalPool += initialTotal;
            _totalCleanPool += initialTotal;
            PrivateNote(_deployer).onInitialStakesAccepted{value: 0.1 vmshell, flag: 1}(
                _event_id, _oracle_list_hash, _token_type, _initialStakes
            );
        }
        _oracleEventsConfirmed[msg.sender.value] = true;
        _oracleEventsPubkeys[oracle_pubkey] = true;
        if (trustAddr.hasValue()) {
            _oracleEventsAddress[trustAddr.get()] = oracle_pubkey;
        }

        _approvedOracleEvents += 1;
        bool allConfirmed = _approvedOracleEvents == _numberOfOracleEvents;

        if (allConfirmed) {
            address addrExtern = address.makeAddrExtern(PMP_APPROVED_BY_ORACLE, bitCntAddress);
            emit ApprovedByOracle{dest: addrExtern}(msg.sender, oracle_pubkey);
        }
    }

    /// @notice Ensures minimal native balance for operations
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    /// @notice Computes stakeEnd as 10% of (stakeStart..resultStart).
    function _computeStakeEnd() private view returns (uint64) {
        return _stakeStart + (_resultStart - _stakeStart) / 10;
    }

    /// @param resultStart Result acceptance start time (resultEnd = resultStart + GRACE_PERIOD)
    function setTimings(
        uint64 resultStart
    ) private {
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(_approvedOracleEvents == _numberOfOracleEvents, ERR_NOT_INITIALIZED);
        require(resultStart > block.timestamp, ERR_INVALID_PARAMS);

        // stakeStart = now on first call (PMP approval)
        if (_stakeStart == 0) {
            _stakeStart = uint64(block.timestamp);
        }

        _resultStart = resultStart;
        _approved = true;

        // If the new stakeEnd <= now, auto-freeze
        if (block.timestamp >= _computeStakeEnd()) {
            _ensureFrozen();
        }

        tvm.accept();
        ensureBalance();

        address addrExtern = address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress);
        emit TimingsSet{dest: addrExtern}(_stakeStart, _computeStakeEnd(), _resultStart, _resultStart + GRACE_PERIOD);
    }

    /// @notice Cancels the event
    function cancelEvent() private {
        require(_approved, ERR_NOT_APPROVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        tvm.accept();
        ensureBalance();
        _isCancelled = true;

        address addrExtern = address.makeAddrExtern(PMP_EVENT_CANCELLED, bitCntAddress);
        emit EventCancelled{dest: addrExtern}();
    }

    /// @notice Accepts stake from PrivateNote and confirms it
    /// @param outcomeId Stake outcome identifier (must be < _numOutcomes)
    /// @param stakeAmount Stake amount
    /// @param deposit_identifier_hash Deposit identifier hash
    /// @param bet_type 0 - clean bet, 1 - debt bet, 2 - coupon bet
    function acceptStake(
        uint32 outcomeId,
        uint128 stakeAmount,
        uint256 deposit_identifier_hash,
        uint8 bet_type
    ) public {
        require(_approved, ERR_NOT_APPROVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(_numOutcomes > 0, ERR_NOT_INITIALIZED);
        require(outcomeId < _numOutcomes, ERR_INVALID_OUTCOME_ID);
        require(!_frozen, ERR_ALREADY_FROZEN);
        require(block.timestamp >= _stakeStart, ERR_STAKE_NOT_STARTED);
        require(block.timestamp < _computeStakeEnd(), ERR_STAKE_PERIOD_ENDED);
        require(bet_type <= BET_TYPE_COUPON, ERR_INVALID_BET_TYPE);
        
        address wallet = DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        tvm.accept();
        ensureBalance();

        if (bet_type == BET_TYPE_COUPON) {
            uint128 current_outcome_coupon_pool = _typedOutcomePools[outcomeId][BET_TYPE_COUPON];
            uint128 new_outcome_coupon_pool = current_outcome_coupon_pool + stakeAmount;            
            uint128 current_outcome_total = _typedOutcomePools[outcomeId][BET_TYPE_COUPON]
                                        + _typedOutcomePools[outcomeId][BET_TYPE_DEBT]
                                        + _typedOutcomePools[outcomeId][BET_TYPE_CLEAN];            
            uint128 new_outcome_total = current_outcome_total + stakeAmount;            
            uint128 max_outcome_coupon_pool = uint128(
                (uint256(new_outcome_total) * uint256(COUPON_POOL_LIMIT_PERCENT)) / uint256(FULL_PERCENT)
            );
            require(new_outcome_coupon_pool <= max_outcome_coupon_pool, ERR_COUPON_POOL_LIMIT_EXCEEDED);            
            _totalCouponPool += stakeAmount;
        } else if (bet_type == BET_TYPE_CLEAN) {
            _totalCleanPool += stakeAmount;
            _totalPool += stakeAmount;    
        } else if (bet_type == BET_TYPE_DEBT) {
            _totalDebtPool += stakeAmount;
            _totalPool += stakeAmount;    
        }
    
        _typedOutcomePools[outcomeId][bet_type] += stakeAmount;

        PrivateNote(wallet).onStakeAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, _numOutcomes, bet_type);
        
        address addrExtern = address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress);
        emit StakeAccepted{dest: addrExtern}(wallet, outcomeId, stakeAmount, bet_type);
    }

    /// @notice Cancels user stakes after event cancellation and processes refund
    /// @dev
    /// - Callable only by the corresponding PrivateNote wallet.
    /// - The event must be cancelled.
    /// - If the result window has ended and the event is not resolved,
    ///   the function triggers automatic cancellation.
    /// - Decreases internal pool balances for clean, debt and coupon bets.
    /// - Clean and debt amounts reduce `_totalPool`.
    /// - Coupon amounts reduce `_totalCouponPool` only.
    /// - Sends aggregated refund data back to PrivateNote via `onStakeCancelled`.
    ///
    /// @param stakeAmount Array of clean stake amounts per outcome.
    ///        Length should correspond to `_numOutcomes`.
    /// @param debtAmount Array of debt stake amounts per outcome.
    ///        Length should correspond to `_numOutcomes`.
    /// @param couponsAmount Array of coupon stake amounts per outcome.
    ///        Length should correspond to `_numOutcomes`.
    /// @param deposit_identifier_hash Deposit identifier hash used to
    ///        deterministically compute the caller's PrivateNote address.
    function cancelStake(
        uint128[] stakeAmount,
        uint128[] debtAmount,
        uint128[] couponsAmount,
        uint256 deposit_identifier_hash
    ) public {
        if ((block.timestamp > (_resultStart + GRACE_PERIOD)) && (!_resolvedOutcome.hasValue()) && (!_isCancelled)) {
            cancelEvent();
        }
        require(_isCancelled, ERR_NOT_CANCELLED);

        address wallet = DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        tvm.accept();
        ensureBalance();
        uint128 totalStake = 0;
        uint128 totalCouponRefund = 0;

        for (uint32 outcomeId = 0; outcomeId < stakeAmount.length; outcomeId++) {
            _typedOutcomePools[outcomeId][BET_TYPE_CLEAN] -= stakeAmount[outcomeId];
            _totalPool -= stakeAmount[outcomeId];
            _totalCleanPool -= stakeAmount[outcomeId];
            totalStake += stakeAmount[outcomeId];
        }

        for (uint32 outcomeId = 0; outcomeId < debtAmount.length; outcomeId++) {
            _typedOutcomePools[outcomeId][BET_TYPE_DEBT] -= debtAmount[outcomeId];
            _totalPool -= debtAmount[outcomeId];
            _totalDebtPool -= debtAmount[outcomeId];
            totalStake += debtAmount[outcomeId];
        }

        for (uint32 outcomeId = 0; outcomeId < couponsAmount.length; outcomeId++) {
            _typedOutcomePools[outcomeId][BET_TYPE_COUPON] -= couponsAmount[outcomeId];
            _totalCouponPool -= couponsAmount[outcomeId];
            totalCouponRefund += couponsAmount[outcomeId];
        }

        PrivateNote(wallet).onStakeCancelled{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, totalStake, totalCouponRefund);
    }



    // ===== Split/Merge Functions =====

    /// @dev Ensures pools are frozen. Called automatically by split/merge/resolve
    ///      on first access after stakeEnd. Snapshots pools, pre-computes payout
    ///      coefficients for every possible outcome, and deploys OrderBook.
    function _ensureFrozen() private {
        if (_frozen) return;
        require(_approved, ERR_NOT_APPROVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(block.timestamp >= _computeStakeEnd(), ERR_NOT_STAKEEND);

        _frozen = true;
        _baseTotalPool = _totalPool;

        // Snapshot per-outcome clean and debt pools
        for (uint32 i = 0; i < _numOutcomes; i++) {
            _frozenCleanPools[i] = _typedOutcomePools[i][BET_TYPE_CLEAN];
            _frozenDebtPools[i] = _typedOutcomePools[i][BET_TYPE_DEBT];
        }

        // Pre-compute payout coefficients for each possible winning outcome
        uint128 maxFO = 0;
        for (uint32 k = 0; k < _numOutcomes; k++) {
            uint128 winClean = _frozenCleanPools[k];
            uint128 winDebt = _frozenDebtPools[k];
            uint128 winCoupon = _typedOutcomePools[k][BET_TYPE_COUPON];
            uint128 totalWinMass = winClean + winDebt + winCoupon;

            if (totalWinMass == 0) {
                continue;
            }

            uint128 profitBudget = _baseTotalPool - winClean - winDebt;

            // Creator fee (capped by profit budget)
            uint128 fee = uint128(
                (uint256(_baseTotalPool) * uint256(FEE_PERCENT)) / uint256(FULL_PERCENT)
            );
            if (fee > profitBudget) {
                fee = profitBudget;
            }
            _frozenCreatorFee[k] = fee;
            profitBudget -= fee;

            // Coupon coefficient (capped at COUPON_MAX_PAYOUT_MULTIPLIER)
            uint128 profitPerUnit = uint128(
                (uint256(profitBudget) * FULL_PERCENT) / totalWinMass
            );
            uint128 couponCoef = profitPerUnit;
            if (couponCoef > COUPON_MAX_PAYOUT_MULTIPLIER) {
                couponCoef = COUPON_MAX_PAYOUT_MULTIPLIER;
            }
            _frozenCouponWinCoef[k] = couponCoef;

            uint128 couponPaid = uint128(
                (uint256(winCoupon) * couponCoef) / FULL_PERCENT
            );
            if (couponPaid > profitBudget) {
                couponPaid = profitBudget;
            }
            profitBudget -= couponPaid;

            // Debt coefficient
            uint128 realWinMass = winClean + winDebt;
            uint128 debtCoef = 0;
            uint128 debtProfit = 0;
            if (realWinMass > 0) {
                uint128 baseRealPPU = uint128(
                    (uint256(profitBudget) * FULL_PERCENT) / realWinMass
                );
                debtCoef = uint128(
                    (uint256(baseRealPPU) *
                    uint256(FULL_PERCENT - DEBT_REDISTRIBUTION_PERCENT)) / uint256(FULL_PERCENT)
                );
                debtProfit = uint128(
                    (uint256(winDebt) * uint256(debtCoef)) / FULL_PERCENT
                );
                profitBudget -= debtProfit;
            }
            _frozenDebtWinCoef[k] = debtCoef;
            _frozenProfitToClean[k] = profitBudget;

            // Fixed obligation for solvency check:
            // FO_k = D_k + Fee_k + CouponPaid_k + DebtProfit_k
            uint128 fo = winDebt + fee + couponPaid + debtProfit;
            if (fo > maxFO) {
                maxFO = fo;
            }
        }
        _maxFixedObligation = maxFO;

        // Deploy OrderBook
        TvmCell stateInit = DexLib.buildOrderBookStateInit(
            _PrivateNoteCode,
            _orderBookCode,
            _event_id,
            _oracle_list_hash,
            _token_type
        );

        new OrderBook{
            stateInit: stateInit,
            value: 1 vmshell,
            flag: 1
        }(tvm.hash(tvm.code()), tvm.code().depth());

        address addrExtern = address.makeAddrExtern(PMP_POOLS_FROZEN, bitCntAddress);
        emit PoolsFrozen{dest: addrExtern}(_baseTotalPool);
    }

    /// @notice Splits collateral into proportional outcome tokens across all outcomes.
    /// @dev Per spec: δ_k = floor(F × M_k / T) where M_k = frozen clean pool,
    ///      T = baseTotalPool. Pool update: _totalPool += F (full collateral).
    ///      Tokens minted: _cleanPools[k] += δ_k.
    ///      Remainder F - Σδ_k stays in pool as surplus (benefits existing stakers).
    ///
    /// @param collateral Amount of collateral to split (F).
    /// @param deposit_identifier_hash Caller's PrivateNote deposit ID.
    function splitFullSet(
        uint128 collateral,
        uint256 deposit_identifier_hash
    ) public {
        _ensureFrozen();
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(collateral > 0, ERR_LOW_VALUE);
        require(_baseTotalPool > 0, ERR_INVALID_PARAMS);

        address wallet = DexLib.computePrivateNoteAddress(
            _PrivateNoteCode,
            deposit_identifier_hash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        tvm.accept();
        ensureBalance();

        // δ_k = floor(F × M_k / R_k), where R_k = M_k + profitToClean_k
        // R_k is the total clean residual on outcome k (principal + profit).
        // This preserves per-token payout coefficients and ensures neutrality.
        uint128[] amounts = new uint128[](_numOutcomes);
        uint128 mintedTotal = 0;
        for (uint32 k = 0; k < _numOutcomes; k++) {
            if (_frozenCleanPools[k] == 0) {
                amounts[k] = 0;
                continue;
            }
            uint128 R_k = _frozenCleanPools[k] + _frozenProfitToClean[k];
            amounts[k] = uint128(
                (uint256(collateral) * uint256(_frozenCleanPools[k])) / uint256(R_k)
            );
            _typedOutcomePools[k][BET_TYPE_CLEAN] += amounts[k];
            mintedTotal += amounts[k];
        }
        _totalPool += collateral;
        _totalCleanPool += mintedTotal;

        PrivateNote(wallet).onSplitAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, amounts);

        address addrExtern = address.makeAddrExtern(PMP_SPLIT_PROCESSED, bitCntAddress);
        emit SplitProcessed{dest: addrExtern}(wallet, collateral);
    }

    /// @notice Merges proportional outcome tokens back into collateral.
    /// @dev Per spec: δ_k = floor(F × M_k / T) using frozen clean pools.
    ///      Solvency: _totalPool - F >= _maxFixedObligation.
    ///      Pool update: _totalPool -= F (full collateral returned).
    ///
    /// @param amount Array of outcome token amounts to merge (upper bounds).
    /// @param deposit_identifier_hash Caller's PrivateNote deposit ID.
    function mergeFullSet(
        uint128[] amount,
        uint256 deposit_identifier_hash
    ) public {
        _ensureFrozen();
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(amount.length == _numOutcomes, ERR_INVALID_OUTCOME_ID);
        require(_baseTotalPool > 0, ERR_INVALID_PARAMS);

        address wallet = DexLib.computePrivateNoteAddress(
            _PrivateNoteCode,
            deposit_identifier_hash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        tvm.accept();
        ensureBalance();

        // Compute F = min(amount[k] * R_k / M_k), where R_k = M_k + profitToClean_k
        uint128 collateral = type(uint128).max;
        for (uint32 k = 0; k < _numOutcomes; k++) {
            if (_frozenCleanPools[k] == 0) {
                require(amount[k] == 0, ERR_INVALID_PARAMS);
                continue;
            }
            uint128 R_k = _frozenCleanPools[k] + _frozenProfitToClean[k];
            uint128 c = uint128(
                (uint256(amount[k]) * uint256(R_k)) / uint256(_frozenCleanPools[k])
            );
            if (c < collateral) {
                collateral = c;
            }
        }
        require(collateral > 0 && collateral != type(uint128).max, ERR_INVALID_PARAMS);

        // Solvency: pool after merge must cover max fixed obligation
        require(_totalPool >= collateral, ERR_INVALID_PARAMS);
        uint128 poolAfter = _totalPool - collateral;
        require(poolAfter >= _maxFixedObligation, ERR_MERGE_SOLVENCY);

        // Compute actual token amounts: δ_k = floor(F × M_k / R_k)
        uint128[] actual;
        uint128 burnedTotal = 0;
        for (uint32 k = 0; k < _numOutcomes; k++) {
            uint128 a = 0;
            if (_frozenCleanPools[k] > 0) {
                uint128 R_k = _frozenCleanPools[k] + _frozenProfitToClean[k];
                a = uint128(
                    (uint256(collateral) * uint256(_frozenCleanPools[k])) / uint256(R_k)
                );
            }
            require(_typedOutcomePools[k][BET_TYPE_CLEAN] >= a, ERR_INVALID_PARAMS);
            actual.push(a);
            burnedTotal += a;
        }

        // Apply merge
        for (uint32 k = 0; k < _numOutcomes; k++) {
            _typedOutcomePools[k][BET_TYPE_CLEAN] -= actual[k];
        }
        // Per spec: _totalPool -= F (full collateral)
        _totalPool -= collateral;
        _totalCleanPool -= burnedTotal;

        PrivateNote(wallet).onMergeAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, collateral, actual);

        address addrExtern = address.makeAddrExtern(PMP_MERGE_PROCESSED, bitCntAddress);
        emit MergeProcessed{dest: addrExtern}(wallet, collateral);
    }


    /// @notice Resolves the event outcome
    /// @param outcomeId Resolution outcome identifier (must be < _numOutcomes)
    function resolve(uint32 outcomeId) private {
        require(_approved, ERR_NOT_APPROVED);
        _ensureFrozen();
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(outcomeId < _numOutcomes, ERR_INVALID_OUTCOME_ID);
        require(block.timestamp >= _resultStart, ERR_RESULT_NOT_STARTED);
        require(block.timestamp <= (_resultStart + GRACE_PERIOD), ERR_RESULT_ENDED);

        tvm.accept();
        ensureBalance();
        _resolvedOutcome = outcomeId;

        // Read pre-computed coefficients for the winning outcome
        _creatorFee = _frozenCreatorFee[outcomeId];
        _couponWinCoef = _frozenCouponWinCoef[outcomeId];
        _debtWinCoef = _frozenDebtWinCoef[outcomeId];
        _profitToClean = _frozenProfitToClean[outcomeId];
        // Use live clean pool (includes split tokens) for selfdestruct tracking
        _totalWinPool = _typedOutcomePools[outcomeId][BET_TYPE_CLEAN]
                      + _frozenDebtPools[outcomeId]
                      + _typedOutcomePools[outcomeId][BET_TYPE_COUPON];

        // Send creator fee to deployer's PrivateNote
        if (_creatorFee > 0) {
            PrivateNote(_deployer).acceptFee{
                value: 0.1 vmshell,
                flag: 1
            }(_creatorFee, _token_type, _event_id, _oracle_list_hash);

            address addrExternFee = address.makeAddrExtern(PMP_CREATOR_FEE_COLLECTED, bitCntAddress);
            emit CreatorFeeCollected{dest: addrExternFee}(_creatorFee);
        }

        address addrExtern =
        address.makeAddrExtern(PMP_RESOLVED, bitCntAddress);
        emit Resolved{dest: addrExtern}(outcomeId);
    }



    /// @notice Claims winnings for the caller's PrivateNote wallet.
    /// @dev
    /// - Callable only by the corresponding PrivateNote wallet.
    /// - The event must be approved.
    /// - `stakeAmount.length` must equal `_numOutcomes`.
    /// - If the event is not yet resolved, the function processes
    ///   a zero-payout claim and returns immediately.
    /// - Determines whether the caller has a winning position
    ///   based on the resolved outcome.
    /// - Calculates payout for:
    ///     * clean bets — proportional share of `_totalPool`
    ///       relative to the winning clean pool,
    ///     * debt bets — original debt amount plus proportional
    ///       profit based on `_debtWinCoef`,
    ///     * coupon bets — profit based on `_couponWinCoef`.
    /// - Updates `_totalWinPool` to track remaining distributable
    ///   winning stake amounts.
    /// - Notifies the caller’s PrivateNote via `onClaimAccepted`.
    /// - Emits `ClaimProcessed` only for resolved events.
    /// - If `_totalWinPool` becomes zero after processing,
    ///   the contract self-destructs to `_deployer`.
    ///
    /// @param stakeAmount Array of clean stake amounts per outcome.
    /// @param debtAmount Array of debt stake amounts per outcome.
    /// @param couponsAmount Array of coupon stake amounts per outcome.
    /// @param deposit_identifier_hash Deposit identifier hash used to
    ///        deterministically compute the caller's PrivateNote address.
    function claim(
    uint128[] stakeAmount,
    uint128[] debtAmount,
    uint128[] couponsAmount,
    uint256 deposit_identifier_hash
    ) public {
        require(_approved, ERR_NOT_APPROVED);
        require(stakeAmount.length == _numOutcomes, ERR_INVALID_OUTCOME_ID);
        address wallet = DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
        require(msg.sender == wallet, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();
        if (!_resolvedOutcome.hasValue()) {
            PrivateNote(wallet).onClaimAccepted{
                value: 0.1 vmshell,
                flag: 1
            }(_event_id, _oracle_list_hash, _token_type, _resolvedOutcome, 0, 0, 0, 0);
            return;
        }
        uint32 W = _resolvedOutcome.get();
        uint128 payoutClean = 0;
        uint128 payoutDebt = 0;
        uint128 payoutCoupon = 0;
        uint128 debtPaid = 0;
        bool win = false;
        // Use frozen clean pool for payout ratio (consistent with resolve)
        uint128 winningClean = _frozenCleanPools[W];
        if (winningClean > 0 && stakeAmount[W] > 0) {
            // Formula 20: payout = principal + profit
            // K_clean^bps = P + (ProfitToClean * P) / M_W
            // payout_i = m_i * K_clean^bps / P = m_i + m_i * ProfitToClean / M_W
            uint128 profit = uint128((uint256(stakeAmount[W]) * uint256(_profitToClean)) / uint256(winningClean));
            payoutClean = stakeAmount[W] + profit;
            win = true;
        }

        if (debtAmount.length > W && debtAmount[W] > 0) {
            uint128 profit = uint128((uint256(debtAmount[W]) * uint256(_debtWinCoef)) / FULL_PERCENT);
            payoutDebt = debtAmount[W] + profit;

            // Formula 17: debtPaid_i = ⌊(profit · R) / (P - R)⌋
            debtPaid = uint128(
                (uint256(profit) * uint256(DEBT_REDISTRIBUTION_PERCENT)) /
                uint256(FULL_PERCENT - DEBT_REDISTRIBUTION_PERCENT)
            );
            win = true;
        }

        if (couponsAmount.length > W && couponsAmount[W] > 0) {
            payoutCoupon = uint128((uint256(couponsAmount[W]) * uint256(_couponWinCoef)) / FULL_PERCENT);
            win = true;
        }

        uint128 totalPayout = payoutClean + payoutDebt + payoutCoupon;

        PrivateNote(wallet).onClaimAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(
            _event_id,
            _oracle_list_hash,
            _token_type,
            _resolvedOutcome,
            payoutClean,
            payoutDebt,
            payoutCoupon,
            debtPaid
        );

        address addrExtern = address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress);
        emit ClaimProcessed{dest: addrExtern}(wallet, totalPayout, win);
        if (_totalWinPool > 0) {
            uint128 debtW = (debtAmount.length > W) ? debtAmount[W] : 0;
            uint128 couponsW = (couponsAmount.length > W) ? couponsAmount[W] : 0;
            uint128 claimedMass = stakeAmount[W] + debtW + couponsW;
            if (_totalWinPool >= claimedMass) {
                _totalWinPool -= claimedMass;
            } else {
                _totalWinPool = 0;
            }
        }
        if (_totalWinPool == 0) {
            selfdestruct(_deployer);
        }
    }


    /// @notice Returns the oracle pubkey from the current message sender.
    ///         Accepts both internal (address-based) and external (pubkey-based) messages.
    /// @return pubkey Resolved oracle public key bound to the caller.
    function _getOraclePubkey() private view returns (uint256 pubkey) {
        if (msg.isInternal) {
            require(_oracleEventsAddress.exists(msg.sender.value), ERR_INVALID_SENDER);
            pubkey = _oracleEventsAddress[msg.sender.value];
        } else {
            pubkey = msg.pubkey();
            require(_oracleEventsPubkeys.exists(pubkey), ERR_INVALID_SENDER);
        }
    }

    /// @notice Computes the quorum threshold: ceil(total * 66 / 100)
    /// @return quorumCount Minimum oracle votes required to execute governance actions.
    function _quorum() private view returns (uint128) {
        return uint128((_numberOfOracleEvents * uint128(THRESHOLD) + 9999) / 10000);
    }

    /// @notice Oracle submits timing parameters; executes when 66% quorum is reached.
    ///         An oracle can update its submission before quorum is reached.
    /// @param resultStart Result acceptance start time (resultEnd = resultStart + GRACE_PERIOD)
    function submitSetTimings(
        uint64 resultStart
    ) public {
        uint256 pubkey = _getOraclePubkey();
        tvm.accept();
        ensureBalance();

        uint256 newHash = tvm.hash(abi.encode(resultStart));

        if (_timingsOracleHash.exists(pubkey)) {
            uint256 oldHash = _timingsOracleHash[pubkey];
            if (oldHash == newHash) return;
            _timingsHashCount[oldHash] -= 1;
            if (_timingsHashCount[oldHash] == 0) {
                delete _timingsHashCount[oldHash];
            }
        }

        _timingsOracleHash[pubkey] = newHash;
        _timingsHashCount[newHash] += 1;

        uint128 count = _timingsHashCount[newHash];
        if (count >= _quorum()) {
            delete _timingsOracleHash;
            delete _timingsHashCount;
            setTimings(resultStart);
        }
    }

    /// @notice Oracle submits resolve outcome; executes when 66% quorum is reached.
    ///         An oracle can update its submission before quorum is reached.
    /// @param outcomeId Outcome identifier to resolve
    function submitResolve(uint32 outcomeId) public {
        uint256 pubkey = _getOraclePubkey();
        tvm.accept();
        ensureBalance();

        uint256 newHash = tvm.hash(abi.encode(outcomeId));

        if (_resolveOracleHash.exists(pubkey)) {
            uint256 oldHash = _resolveOracleHash[pubkey];
            if (oldHash == newHash) return;
            _resolveHashCount[oldHash] -= 1;
            if (_resolveHashCount[oldHash] == 0) {
                delete _resolveHashCount[oldHash];
            }
        }

        _resolveOracleHash[pubkey] = newHash;
        _resolveHashCount[newHash] += 1;

        uint128 count = _resolveHashCount[newHash];
        if (count >= _quorum()) {
            delete _resolveOracleHash;
            delete _resolveHashCount;
            resolve(outcomeId);
        }
    }

    /// @notice Oracle votes to cancel the event; executes when 66% quorum is reached.
    ///         Each oracle can only vote once.
    function submitCancelEvent() public {
        uint256 pubkey = _getOraclePubkey();
        tvm.accept();
        ensureBalance();

        if (_cancelOracleVoted.exists(pubkey)) return;

        _cancelOracleVoted[pubkey] = true;
        _cancelVoteCount += 1;

        if (_cancelVoteCount >= _quorum()) {
            delete _cancelOracleVoted;
            _cancelVoteCount = 0;
            cancelEvent();
        }
    }

    /// @notice Handles bounce from downstream oracle calls and rolls back oracle confirmations.
    /// @dev
    /// - If bounce sender is one of configured oracle event lists, the contract asks every
    ///   already-confirmed oracle list to cancel this event and then self-destructs.
    /// - Prevents half-initialized market state after failed oracle interactions.
    /// @param body Bounced message body (unused, accepted to satisfy ABI).
    onBounce(TvmSlice body) external {
        tvm.accept();
        ensureBalance();
        body;
        if (_oracleEventsConfirmed.exists(msg.sender.value)) {
            // Only cancel oracles that have already confirmed (not the one that bounced)
            for ((uint256 key, bool confirmed) : _oracleEventsConfirmed) {
                if (confirmed) {
                    OracleEventList(address.makeAddrStd(0, key)).cancelEvent{
                        value: 0.1 vmshell,
                        flag: 1
                    }(_event_id, _oracle_list_hash, _token_type);
                }
            }
            selfdestruct(ROOT_PN_ADDRESS);
        }
    }

    /// @notice Returns full current state of the PMP contract.
    /// @dev
    /// - Provides a complete snapshot of event configuration,
    ///   lifecycle state, oracle confirmations and pool balances.
    /// - Intended for frontend applications, indexers and analytics tools.
    /// - Does not modify contract state.
    ///
    /// @return name Human-readable pool name.
    /// @return token_type Static token type used by the pool.
    /// @return event_id Identifier of the associated event.
    /// @return oracle_list_hash Hash of the oracle list used during deployment.
    /// @return deployer Address of the PrivateNote wallet that deployed the contract.
    /// @return privateNoteCodeHash Hash of the PrivateNote contract code used for address derivation.
    /// @return totalPool Total amount currently stored in the pool.
    /// @return approved Whether the event has been approved for staking.
    /// @return numOutcomes Total number of available outcomes.
    /// @return resolvedOutcome Final resolved outcome identifier (if set).
    /// @return stakeStart Stake acceptance start timestamp.
    /// @return stakeEnd Stake acceptance end timestamp.
    /// @return resultStart Result acceptance start timestamp.
    /// @return resultEnd Result acceptance end timestamp.
    /// @return isCancelled Whether the event has been cancelled.
    /// @return numberOfOracleEvents Total number of required oracle confirmations.
    /// @return approvedOracleEvents Number of oracle confirmations received.
    /// @return typedOutcomePools Mapping of outcome → bet type → pool amount.
    /// @return outcomeNames Mapping of outcome identifiers to human-readable names.
    /// @return creatorFee Creator fee collected at resolve.
    function getDetails() external view returns (
        string name,
        uint32 token_type,
        uint256 event_id,
        uint256 oracle_list_hash,
        address deployer,
        uint256 privateNoteCodeHash,
        uint128 totalPool,
        bool approved,
        uint32 numOutcomes,
        optional(uint32) resolvedOutcome,
        uint64 stakeStart,
        uint64 stakeEnd,
        uint64 resultStart,
        uint64 resultEnd,
        bool isCancelled,
        uint128 numberOfOracleEvents,
        uint128 approvedOracleEvents,
        mapping(uint32 => mapping(uint8 => uint128)) typedOutcomePools,
        mapping(uint32 => string) outcomeNames,
        uint128 creatorFee,
        bool frozen,
        uint128 baseTotalPool,
        uint128 profitToClean
    ) {
        return (
            _name,
            _token_type,
            _event_id,
            _oracle_list_hash,
            _deployer,
            tvm.hash(_PrivateNoteCode),
            _totalPool,
            _approved,
            _numOutcomes,
            _resolvedOutcome,
            _stakeStart,
            _computeStakeEnd(),
            _resultStart,
            (_resultStart + GRACE_PERIOD),
            _isCancelled,
            _numberOfOracleEvents,
            _approvedOracleEvents,
            _typedOutcomePools,
            _outcomeNames,
            _creatorFee,
            _frozen,
            _baseTotalPool,
            _profitToClean
        );
    }

    /// @notice Returns the OrderBook address for this PMP
    /// @return orderBookAddress Deterministic OrderBook address for this market.
    function getOrderBookAddress() external view returns (address orderBookAddress) {
        return DexLib.computeOrderBookAddress(
            _PrivateNoteCode,
            _orderBookCode,
            _event_id,
            _oracle_list_hash,
            _token_type
        );
    }

    /// @notice Returns contract name
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "PMP");
    }
}
