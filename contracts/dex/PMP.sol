pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PrivateNote.sol";
import "./OracleEventList.sol";
import "./libraries/DexLib.sol";

/// @title PMP - Pari Mutuel Pool
/// @notice PMP contract without fees, requires oracle approval
contract PMP is Modifiers {

    string constant version = "1.0.0";

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
    uint128 _deptWinCoef;

    /// @notice Total winnings from the winning outcome, used for payout calculations
    uint128 _totalWinPool;

    /// @notice Finalized outcome
    optional(uint32) _resolvedOutcome;

    /// @notice Approval flag from oracle
    bool _approved;

    /// @notice Number of outcome intervals (set by oracle)
    uint32 _numOutcomes;

    /// @notice Outcome names
    mapping(uint32 => string) _outcomeNames;

    /// ===== Time windows (set by oracle) =====

    /// @notice Stake acceptance start time
    uint64 _stakeStart;

    /// @notice Stake acceptance end time
    uint64 _stakeEnd;

    /// @notice Result acceptance start time
    uint64 _resultStart;

    /// @notice Result acceptance end time
    uint64 _resultEnd;

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

    /// @notice Mapping of oracle proposals
    mapping(uint256 => Proposal) _oracleProposals;

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
    /// @dev If event isn't resolved or user didn't win, `payout` will be 0 and `win` will be false.
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

    /// @notice Emitted when an oracle governance proposal is created.
    /// @param proposalId Deterministic proposal identifier (hash of functionType + data).
    /// @param functionType Proposed action type (see FUNCTION_TYPE_* constants).
    /// @param data ABI-encoded payload for the proposed action.
    event ProposalCreated(uint256 proposalId, uint32 functionType, TvmCell data);

    /// @notice Emitted when a governance proposal is executed.
    /// @param proposalId Identifier of the executed proposal.
    /// @param functionType Executed action type.
    /// @param data ABI-encoded payload used for execution.
    event ProposalExecuted(uint256 proposalId, uint32 functionType, TvmCell data);

    /// @notice PMP constructor
    /// @param deposit_identifier_hash Deposit identifier hash from PrivateNote
    constructor(uint256 deposit_identifier_hash, uint32 token_type, address[] oracle_event_lists, uint128[] oracle_fees) {
        tvm.accept();
        _token_type = token_type;
        TvmCell salt = abi.codeSalt(tvm.code()).get();
        (TvmCell PrivateNoteCode) = abi.decode(salt, (TvmCell));
        _PrivateNoteCode = PrivateNoteCode;
        _approved = false;
        _deployer = msg.sender;
        _numOutcomes = 0; // Initialize with 0 outcomes

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
        if (_oracleEventsPubkeys[msg.sender.value] == true) {
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

    /// @notice Sets stake and result time windows
    /// @param stakeStart Stake acceptance start time
    /// @param stakeEnd Stake acceptance end time
    /// @param resultStart Result acceptance start time
    /// @param resultEnd Result acceptance end time
    function setTimings(
        uint64 stakeStart,
        uint64 stakeEnd,
        uint64 resultStart,
        uint64 resultEnd
    ) private {
        require(!_approved, ERR_ALREADY_APPROVED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(_approvedOracleEvents == _numberOfOracleEvents, ERR_NOT_INITIALIZED); // Must set outcomes first

        require(stakeStart < stakeEnd, ERR_INVALID_PARAMS);
        require(stakeEnd <= resultStart, ERR_INVALID_PARAMS);
        require(resultStart < resultEnd, ERR_INVALID_PARAMS);

        tvm.accept();
        ensureBalance();

        _stakeStart = stakeStart;
        _stakeEnd = stakeEnd;
        _resultStart = resultStart;
        _resultEnd = resultEnd;
        _approved = true;
        
        address addrExtern = address.makeAddrExtern(PMP_SET_TIMINGS, bitCntAddress);
        emit TimingsSet{dest: addrExtern}(stakeStart, stakeEnd, resultStart, resultEnd);
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
        require(block.timestamp >= _stakeStart, ERR_STAKE_NOT_STARTED);
        require(block.timestamp < _stakeStart + (_stakeEnd - _stakeStart) * FULL_SET_PERCENT / FULL_PERCENT, ERR_STAKE_PERIOD_ENDED);
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
        if ((block.timestamp > _resultEnd) && (!_resolvedOutcome.hasValue()) && (!_isCancelled)) {
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

    function _checkFullSetProportion(uint128[] amount) private view {
        uint32 baseIndex = type(uint32).max;
        uint128 basePool;
        uint128 baseAmount;

        for (uint32 i = 0; i < _numOutcomes; i++) {
            uint128 poolSum = 0;
            for (uint8 t = 0; t < BET_TYPE_COUPON; t++) {
                poolSum += _typedOutcomePools[i][t];
            }
            if (poolSum > 0) {
                baseIndex = i;
                basePool = poolSum;
                baseAmount = amount[i];
                break;
            }
        }

        require(baseIndex != type(uint32).max, ERR_INVALID_PARAMS);

        for (uint32 i = 0; i < _numOutcomes; i++) {
            uint128 poolSum = 0;
            for (uint8 t = 0; t < BET_TYPE_COUPON; t++) {
                poolSum += _typedOutcomePools[i][t];
            }

            if (poolSum == 0) {
                require(amount[i] == 0, ERR_INVALID_PARAMS);
            } else {
                require(
                    uint256(amount[i]) * uint256(basePool) ==
                    uint256(baseAmount) * uint256(poolSum),
                    ERR_INVALID_PARAMS
                );
            }
        }
    }


    /// @notice Accepts a proportional full-set stake from PrivateNote across all outcomes.
    /// @dev
    /// - Callable only by the corresponding PrivateNote wallet.
    /// - The event must be approved and not cancelled.
    /// - The event must not be resolved.
    /// - Can be executed only during the full-set staking window
    ///   (final portion of the stake period).
    /// - `amount.length` must equal `_numOutcomes`.
    /// - `_totalPool` must be greater than zero.
    /// - The provided amounts must preserve proportionality with
    ///   existing outcome pools (validated via `_checkFullSetProportion`).
    /// - Internally treated as clean bets:
    ///   increases `_typedOutcomePools[i][BET_TYPE_CLEAN]`,
    ///   `_totalPool`, and `_totalCleanPool`.
    /// - Notifies the caller’s PrivateNote via `onFullSetStakeAccepted`.
    ///
    /// @param amount Array of stake amounts per outcome.
    ///        Must match `_numOutcomes` and preserve pool proportions.
    /// @param deposit_identifier_hash Deposit identifier hash used to
    ///        deterministically compute the caller's PrivateNote address.
    function acceptFullSetStake(
        uint128[] amount,
        uint256 deposit_identifier_hash
    ) public {
        require(_approved, ERR_NOT_APPROVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(amount.length == _numOutcomes, ERR_INVALID_OUTCOME_ID);

        uint128 fullSetStart = _stakeStart +
            (_stakeEnd - _stakeStart) * FULL_SET_PERCENT / FULL_PERCENT;

        require(block.timestamp >= fullSetStart, ERR_STAKE_NOT_STARTED);
        require(block.timestamp < _stakeEnd, ERR_STAKE_PERIOD_ENDED);

        address wallet = DexLib.computePrivateNoteAddress(
            _PrivateNoteCode,
            deposit_identifier_hash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        require(_totalPool > 0, ERR_INVALID_PARAMS);

        _checkFullSetProportion(amount);

        tvm.accept();
        ensureBalance();

        uint128 addedTotal = 0;
        for (uint32 i = 0; i < _numOutcomes; i++) {
            _typedOutcomePools[i][BET_TYPE_CLEAN] += amount[i];
            addedTotal += amount[i];
        }
        _totalPool += addedTotal;
        _totalCleanPool += addedTotal;

        PrivateNote(wallet).onFullSetStakeAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, amount);
    }

    /// @notice Withdraws (cancels) a proportional full-set stake across all outcomes.
    /// @dev
    /// - Callable only by the corresponding PrivateNote wallet.
    /// - The event must be approved and not cancelled.
    /// - The event must not be resolved.
    /// - Can be executed only during the full-set staking window
    ///   (final portion of the stake period).
    /// - `amount.length` must equal `_numOutcomes`.
    /// - `_totalPool` must be greater than zero.
    /// - Each outcome clean pool must be sufficient:
    ///   `_typedOutcomePools[i][BET_TYPE_CLEAN] >= amount[i]`.
    /// - The provided amounts must preserve proportionality with
    ///   existing outcome pools (validated via `_checkFullSetProportion`).
    /// - Internally decreases `_typedOutcomePools[i][BET_TYPE_CLEAN]`,
    ///   `_totalPool`, and `_totalCleanPool`.
    /// - Notifies the caller’s PrivateNote via `onFullSetStakeCancelled`.
    ///
    /// @param amount Array of withdrawal amounts per outcome.
    ///        Must match `_numOutcomes` and preserve pool proportions.
    /// @param deposit_identifier_hash Deposit identifier hash used to
    ///        deterministically compute the caller's PrivateNote address.
    function withdrawFullSet(
        uint128[] amount,
        uint256 deposit_identifier_hash
    ) public {
        require(_approved, ERR_NOT_APPROVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(amount.length == _numOutcomes, ERR_INVALID_OUTCOME_ID);

        uint128 fullSetStart = _stakeStart +
            (_stakeEnd - _stakeStart) * FULL_SET_PERCENT / FULL_PERCENT;

        require(block.timestamp >= fullSetStart, ERR_STAKE_NOT_STARTED);
        require(block.timestamp < _stakeEnd, ERR_STAKE_PERIOD_ENDED);

        address wallet = DexLib.computePrivateNoteAddress(
            _PrivateNoteCode,
            deposit_identifier_hash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        require(_totalPool > 0, ERR_INVALID_PARAMS);

        _checkFullSetProportion(amount);

        tvm.accept();
        ensureBalance();

        uint128 removedTotal = 0;
        for (uint32 i = 0; i < _numOutcomes; i++) {
            require(_typedOutcomePools[i][BET_TYPE_CLEAN] >= amount[i], ERR_INVALID_PARAMS);
            _typedOutcomePools[i][BET_TYPE_CLEAN] -= amount[i];
            removedTotal += amount[i];
        }
        _totalPool -= removedTotal;
        _totalCleanPool -= removedTotal;

        PrivateNote(wallet).onFullSetStakeCancelled{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, amount);
    }


    /// @notice Resolves the event outcome
    /// @param outcomeId Resolution outcome identifier (must be < _numOutcomes)
    function resolve(uint32 outcomeId) private {
        require(_approved, ERR_NOT_APPROVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(outcomeId < _numOutcomes, ERR_INVALID_OUTCOME_ID);
        require(block.timestamp >= _resultStart, ERR_RESULT_NOT_STARTED);
        require(block.timestamp <= _resultEnd, ERR_RESULT_ENDED);

        tvm.accept();
        ensureBalance();
        _resolvedOutcome = outcomeId;
        uint128 winningClean = _typedOutcomePools[outcomeId][BET_TYPE_CLEAN];
        uint128 winningDebt = _typedOutcomePools[outcomeId][BET_TYPE_DEBT];
        uint128 winningCoupon = _typedOutcomePools[outcomeId][BET_TYPE_COUPON];
        uint128 totalWinningMass = winningClean + winningDebt + winningCoupon;
        require(totalWinningMass > 0, ERR_INVALID_PARAMS);
        uint128 profitBudget = _totalPool - winningClean - winningDebt;
        uint128 originalProfitBudget = profitBudget;
        uint128 profitPerUnit = uint128(
            (uint256(profitBudget) * FULL_PERCENT) /
            totalWinningMass
        );
        _couponWinCoef = profitPerUnit;
        if (_couponWinCoef > COUPON_MAX_PAYOUT_MULTIPLIER) {
            _couponWinCoef = COUPON_MAX_PAYOUT_MULTIPLIER;
        }
        uint128 couponProfit = uint128(
            (uint256(winningCoupon) * _couponWinCoef) /
            FULL_PERCENT
        );
        if (couponProfit > profitBudget) {
            couponProfit = profitBudget;
        }
        profitBudget -= couponProfit;
        uint128 realWinningMass =
            winningClean + winningDebt;
        if (realWinningMass > 0) {
            uint128 baseRealProfitPerUnit = uint128(
                (uint256(profitBudget) * FULL_PERCENT) /
                realWinningMass
            );

            uint128 P = originalProfitBudget;
            uint128 R = winningDebt;

            if (P > 0 && P > R) {
                _deptWinCoef = uint128(
                    (uint256(baseRealProfitPerUnit) *
                    uint256(P - R)) / uint256(P)
                );
            } else {
                _deptWinCoef = 0;
            }
        } else {
            _deptWinCoef = 0;
        }
        _totalPool = profitBudget;
        _totalWinPool = totalWinningMass;
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
    ///       profit based on `_deptWinCoef`,
    ///     * coupon bets — profit based on `_couponWinCoef`.
    /// - Updates `_totalWinPool` to track remaining distributable
    ///   winning stake amounts.
    /// - Notifies the caller’s PrivateNote via `onClaimAccepted`.
    /// - Emits `ClaimProcessed`.
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
            }(_event_id, _oracle_list_hash, _token_type, _resolvedOutcome, 0, 0, 0);
            return;
        }
        uint32 W = _resolvedOutcome.get();
        uint128 payoutClean = 0;
        uint128 payoutDebt = 0;
        uint128 payoutCoupon = 0;
        bool win = false;
        uint128 winningClean = _typedOutcomePools[W][BET_TYPE_CLEAN];
        if (winningClean > 0 && stakeAmount[W] > 0) {
            payoutClean = uint128((uint256(stakeAmount[W]) * uint256(_totalPool)) / uint256(winningClean));
            win = true;
        }

        if (debtAmount[W] > 0) {
            uint128 profit = uint128((uint256(debtAmount[W]) * uint256(_deptWinCoef)) / FULL_PERCENT);
            payoutDebt = debtAmount[W] + profit;
            win = true;
        }

        if (couponsAmount[W] > 0) {
            payoutCoupon = uint128((uint256(couponsAmount[W]) * uint256(_couponWinCoef)) / FULL_PERCENT);
            win = true;
        }

        uint128 totalPayout = payoutClean + payoutCoupon;

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
            payoutCoupon
        );

        address addrExtern = address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress);
        emit ClaimProcessed{dest: addrExtern}(wallet, totalPayout, win);
        if (_totalWinPool > 0) {
            _totalWinPool -= (stakeAmount[W] + debtAmount[W] + couponsAmount[W]);
        }
        if (_totalWinPool == 0) {
            selfdestruct(_deployer);
        }
    }


    /// @notice Creates a new proposal for the PMP
    /// @param function_type Type of function the proposal is for
    /// @param data Encoded data for the proposal
    function createProposal(uint32 function_type, TvmCell data) public {
        uint256 pubkey = 0;
        if (msg.isInternal) {
            require(_oracleEventsAddress.exists(msg.sender.value), ERR_INVALID_SENDER);
            pubkey = _oracleEventsAddress[msg.sender.value];
        } else {
            pubkey = msg.pubkey();
            require(_oracleEventsPubkeys.exists(pubkey), ERR_INVALID_SENDER);
        }
        tvm.accept();
        ensureBalance();

        if (function_type == FUNCTION_TYPE_SET_STAKE_DEADLINE) {
            abi.decode(data, (uint64, uint64, uint64, uint64));
        } else if (function_type == FUNCTION_TYPE_SET_RESOLVE) {
            abi.decode(data, (uint32));
        } else if (function_type == FUNCTION_TYPE_CANCEL_EVENT) {
        }

        uint256 proposalId = tvm.hash(abi.encode(function_type, data));
        Proposal proposal;
        proposal.function_type = function_type;
        proposal.data = data;
        proposal.deadline = uint64(block.timestamp) + 7 days;
        proposal.voteCount = 1; // Creator's vote
        proposal.votes[pubkey] = true;
        
        _oracleProposals[proposalId] = proposal;

        if (_numberOfOracleEvents == 1) {
            executeProposal(proposalId);
            return;  
        }
        
        address addrExtern = address.makeAddrExtern(PMP_PROPOSAL_CREATED, bitCntAddress);
        emit ProposalCreated{dest: addrExtern}(proposalId, function_type, data);
    }

    /// @notice Casts a vote on a proposal
    /// @param proposalId Identifier of the proposal to vote on
    function vote(uint256 proposalId) public {
        uint256 pubkey = 0;
        if (msg.isInternal) {
            require(_oracleEventsAddress.exists(msg.sender.value), ERR_INVALID_SENDER);
            pubkey = _oracleEventsAddress[msg.sender.value];
        } else {
            pubkey = msg.pubkey();
            require(_oracleEventsPubkeys.exists(pubkey), ERR_INVALID_SENDER);
        }
        require(_oracleProposals.exists(proposalId), ERR_PROPOSAL_NOT_EXISTS);
        tvm.accept();
        ensureBalance();
        Proposal proposal = _oracleProposals[proposalId];
        if (proposal.deadline < uint64(block.timestamp)) {
            delete _oracleProposals[proposalId];
            return;
        }
        require(!proposal.votes.exists(pubkey), ERR_ALREADY_VOTED);
        
        proposal.votes[pubkey] = true;
        proposal.voteCount++;
        _oracleProposals[proposalId] = proposal;
        if (proposal.voteCount >= (_numberOfOracleEvents * THRESHOLD) / 10000) {
            executeProposal(proposalId);
        }
    }
    
    /// @notice Executes a proposal once it has enough votes
    /// @param proposalId Identifier of the proposal to execute 
    function executeProposal(uint256 proposalId) private {
        Proposal proposal = _oracleProposals[proposalId];
        
        tvm.accept();
        ensureBalance();

        if (proposal.function_type == FUNCTION_TYPE_SET_STAKE_DEADLINE) {
            (uint64 stakeStart, uint64 stakeEnd, uint64 resultStart, uint64 resultEnd) = abi.decode(proposal.data, (uint64, uint64, uint64, uint64));
            setTimings(stakeStart, stakeEnd, resultStart, resultEnd);
        } else if (proposal.function_type == FUNCTION_TYPE_SET_RESOLVE) {
            (uint32 outcomeId) = abi.decode(proposal.data, (uint32));
            resolve(outcomeId);
        } else if (proposal.function_type == FUNCTION_TYPE_CANCEL_EVENT) {
            cancelEvent();
        }
        
        address addrExtern = address.makeAddrExtern(PMP_PROPOSAL_EXECUTED, bitCntAddress);
        emit ProposalExecuted{dest: addrExtern}(proposalId, proposal.function_type, proposal.data);
        delete _oracleProposals[proposalId]; 
    }

    onBounce(TvmSlice body) external {
        tvm.accept();
        ensureBalance();
        body;
        if (_oracleEventsConfirmed.exists(msg.sender.value)) {
            for ((uint256 key, ) : _oracleEventsConfirmed) {
                OracleEventList(address.makeAddrStd(0, key)).cancelEvent{
                    value: 0.1 vmshell,
                    flag: 1
                }(_event_id, _oracle_list_hash, _token_type);
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
        mapping(uint32 => string) outcomeNames
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
            _stakeEnd,
            _resultStart,
            _resultEnd,
            _isCancelled,
            _numberOfOracleEvents,
            _approvedOracleEvents,
            _typedOutcomePools,
            _outcomeNames
        );
    }

    /// @notice Returns contract name
    function getVersion() external pure returns (string, string) {
        return (version, "PMP");
    }
}