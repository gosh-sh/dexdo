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

    /// @notice Outcome pools
    mapping(uint32 => uint128) _outcomePools;

    /// @notice Outcome stake counts
    mapping(uint32 => uint128) _outcomeCounts;

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

    // Events

    /// @notice Emitted when a stake is accepted and accounted into the pool.
    /// @dev `note` is a PrivateNote wallet address that sent the stake.
    /// @param note PrivateNote address (wallet) that placed the stake.
    /// @param outcomeId Outcome identifier the stake is placed on.
    /// @param amount Stake amount added to the pool.
    event StakeAccepted(address indexed note, uint32 outcomeId, uint128 amount);

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
    /// @param isInc Is it new PrivateNote for Stake
    function acceptStake(
        uint32 outcomeId,
        uint128 stakeAmount,
        uint256 deposit_identifier_hash,
        bool isInc
    ) public {
        require(_approved, ERR_NOT_APPROVED);
        require(!_isCancelled, ERR_ALREADY_CANCELLED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(_numOutcomes > 0, ERR_NOT_INITIALIZED);
        require(outcomeId < _numOutcomes, ERR_INVALID_OUTCOME_ID);
        require(block.timestamp >= _stakeStart, ERR_STAKE_NOT_STARTED);
        require(block.timestamp < _stakeStart + (_stakeEnd - _stakeStart) * FULL_SET_PERCENT / FULL_PERCENT, ERR_STAKE_PERIOD_ENDED);
        
        address wallet = DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        tvm.accept();
        ensureBalance();

        _totalPool += stakeAmount;
        _outcomePools[outcomeId] += stakeAmount;
        if (isInc) {        
            _outcomeCounts[outcomeId] += 1;
        }

        address addrExtern = address.makeAddrExtern(PMP_STAKE_ACCEPTED, bitCntAddress);
        emit StakeAccepted{dest: addrExtern}(wallet, outcomeId, stakeAmount);

        PrivateNote(wallet).onStakeAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, _numOutcomes);
    }

    /// @notice Cancels a stake and refunds the user
    /// @param stakeAmount Stake amount
    /// @param deposit_identifier_hash Deposit identifier hash
    function cancelStake(
        uint128[] stakeAmount,
        uint256 deposit_identifier_hash
    ) public {
        require(_isCancelled, ERR_NOT_CANCELLED);

        address wallet = DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        tvm.accept();
        ensureBalance();
        uint128 totalStake = 0;
        for (uint32 outcomeId = 0; outcomeId < _numOutcomes; outcomeId++) {
            _outcomePools[outcomeId] -= stakeAmount[outcomeId];
            _outcomeCounts[outcomeId] -= 1;
            _totalPool -= stakeAmount[outcomeId];
            totalStake += stakeAmount[outcomeId];
        }

        PrivateNote(wallet).onStakeCancelled{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, totalStake);
    }

    function _checkFullSetProportion(uint128[] amount) private view {
        uint32 baseIndex = type(uint32).max;
        uint128 basePool;
        uint128 baseAmount;

        for (uint32 i = 0; i < _numOutcomes; i++) {
            if (_outcomePools[i] > 0) {
                baseIndex = i;
                basePool = _outcomePools[i];
                baseAmount = amount[i];
                break;
            }
        }
        require(baseIndex != type(uint32).max, ERR_INVALID_PARAMS);
        for (uint32 i = 0; i < _numOutcomes; i++) {
            if (_outcomePools[i] == 0) {
                require(amount[i] == 0, ERR_INVALID_PARAMS);
            } else {
                require(
                    uint256(amount[i]) * uint256(basePool) ==
                    uint256(baseAmount) * uint256(_outcomePools[i]),
                    ERR_INVALID_PARAMS
                );
            }
        }
    }


    /// @notice Accepts full set stake from PrivateNote and confirms it
    /// @param amount Stake amounts per outcome
    /// @param isInc Is it new PrivateNote for Stake
    /// @param deposit_identifier_hash Deposit identifier hash
    function acceptFullSetStake(
        uint128[] amount,
        bool[] isInc,
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
            if (isInc[i]) {
                _outcomeCounts[i] += 1;
            }
            _outcomePools[i] += amount[i];
            addedTotal += amount[i];
        }
        _totalPool += addedTotal;

        PrivateNote(wallet).onFullSetStakeAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, amount);
    }

    /// @notice Cancels a full set stake and refunds the user
    /// @param amount Stake amounts per outcome 
    /// @param isDecr Is it decrease of stake
    /// @param deposit_identifier_hash Deposit identifier hash
    function withdrawFullSet(
        uint128[] amount,
        bool[] isDecr,
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
            require(_outcomePools[i] >= amount[i], ERR_INVALID_PARAMS);
            if (isDecr[i]) {
                require(_outcomeCounts[i] > 0, ERR_INVALID_PARAMS);
                _outcomeCounts[i] -= 1;
            }
            _outcomePools[i] -= amount[i];
            removedTotal += amount[i];
        }
        _totalPool -= removedTotal;

        PrivateNote(wallet).onFullSetStakeCancelled{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, amount);
    }




    /// @notice Resolves the event outcome
    /// @param outcomeId Resolution outcome identifier (must be < _numOutcomes)
    function resolve(uint32 outcomeId) private {
        require(_approved, ERR_NOT_APPROVED);
        require(!_resolvedOutcome.hasValue(), ERR_ALREADY_RESOLVED);
        require(outcomeId < _numOutcomes, ERR_INVALID_OUTCOME_ID);
        require(block.timestamp >= _resultStart, ERR_RESULT_NOT_STARTED);
        require(block.timestamp <= _resultEnd, ERR_RESULT_ENDED);

        tvm.accept();
        ensureBalance();
        _resolvedOutcome = outcomeId;

        uint128 privateNoteFee = (_totalPool * FEE_PERCENT) / FULL_PERCENT;
        if (privateNoteFee > 0) {
            PrivateNote(_deployer).acceptFee{
                value: 0.1 vmshell,
                flag: 1
            }(privateNoteFee, _token_type, _event_id, _oracle_list_hash);
        }
        _totalPool -= privateNoteFee;

        address addrExtern = address.makeAddrExtern(PMP_RESOLVED, bitCntAddress);
        emit Resolved{dest: addrExtern}(outcomeId);
    }

    /// @notice Claims winnings for user
    /// @param stakeAmount Stake amount
    /// @param deposit_identifier_hash Deposit identifier hash
    function claim(
        uint128[] stakeAmount,
        uint256 deposit_identifier_hash
    ) public {
        require(_approved, ERR_NOT_APPROVED);
        require(stakeAmount.length == _numOutcomes, ERR_INVALID_OUTCOME_ID);

        address wallet = DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
        require(msg.sender == wallet, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();

        address addrExtern;

        if (!_resolvedOutcome.hasValue()) {
            PrivateNote(wallet).onClaimAccepted{
                value: 0.1 vmshell,
                flag: 1
            }(_event_id, _oracle_list_hash, _token_type, _resolvedOutcome, 0);

            addrExtern = address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress);
            emit ClaimProcessed{dest: addrExtern}(wallet, 0, false);
            return;
        }

        bool win = stakeAmount[_resolvedOutcome.get()] > 0;

        for (uint32 i = 0; i < _numOutcomes; i++)
            if (stakeAmount[i] > 0) 
                _outcomeCounts[i] -= 1;

        if (!win) {
            PrivateNote(wallet).onClaimAccepted{
                value: 0.1 vmshell,
                flag: 1
            }(_event_id, _oracle_list_hash, _token_type, _resolvedOutcome, 0);

            addrExtern = address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress);
            emit ClaimProcessed{dest: addrExtern}(wallet, 0, false);
            return;
        }

        tvm.accept();

        uint128 winningPool = _outcomePools[_resolvedOutcome.get()];
        uint128 payout = 0;
        if (winningPool != 0) {
            payout = uint128(
                (uint256(stakeAmount[_resolvedOutcome.get()]) * uint256(_totalPool)) / uint256(winningPool)
            );
        }

        PrivateNote(wallet).onClaimAccepted{
            value: 0.1 vmshell,
            flag: 1
        }(_event_id, _oracle_list_hash, _token_type, _resolvedOutcome, payout);

        addrExtern = address.makeAddrExtern(PMP_CLAIM_PROCESSED, bitCntAddress);
        emit ClaimProcessed{dest: addrExtern}(wallet, payout, true);

        if (_outcomeCounts[_resolvedOutcome.get()] == 0) {
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

    /// @notice Returns all contract details
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
        mapping(uint32 => uint128) outcomePoolAmounts,
        mapping(uint32 => uint128) outcomeStakeCounts,
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
            _outcomePools,
            _outcomeCounts,
            _outcomeNames
        );
    }

    /// @notice Returns contract name
    function getVersion() external pure returns (string, string) {
        return (version, "PMP");
    }
}