pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PMP.sol";
import "./libraries/DexLib.sol";

/// @title Oracle Event List Contract
contract OracleEventList is Modifiers {

    string constant version = "1.0.0";
    
    /// @notice Oracle owner address
    address static _oracle;

    /// @notice Index of Oracle event list
    uint128 static _index;

    /// @notice Public key of the oracle owner
    uint256 _oracle_pubkey;

    /// @notice PrivateNote contract code
    TvmCell _PrivateNoteCode;

    /// @notice PMP contract code
    TvmCell _pmpCode;
    
    /// @notice Mapping from event_id to event info
    mapping(uint256 => EventInfo) public _events;
    
    event EventAdded(uint256 event_id, string event_name, uint128 oracle_fee, uint64 deadline);
    event EventConfirmed(uint256 event_id, address pmpAddress);
    
    constructor(uint256 pubkey, TvmCell PrivateNoteCode, TvmCell pmpCode) {
        tvm.accept();
        _oracle = msg.sender;
        _oracle_pubkey = pubkey;
        _PrivateNoteCode = PrivateNoteCode;
        _pmpCode = pmpCode;
    }

    /// @notice Ensures minimal native balance for operations
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }
    
    /// @notice Adds a new event that Oracle is willing to service
    /// @param event_name Human-readable event name
    /// @param oracle_fee Oracle fee
    /// @param deadline Timestamp when Oracle is ready to service
    /// @param trustAddr Trusted addr for oracle event
    function addEvent(
        string event_name,
        uint128 oracle_fee,
        uint64 deadline,
        string describe,
        mapping(uint32 => string) outcomeNames,
        optional(uint256) trustAddr
    ) public onlyOwnerPubkey(_oracle_pubkey) accept {        
        require(deadline > block.timestamp, ERR_INVALID_PARAMS);
        ensureBalance();
        require(outcomeNames.keys().length >= 2, ERR_INVALID_PARAMS); // At least 2 outcomes required
        require(outcomeNames.keys().length < 20, ERR_INVALID_PARAMS); 
        uint256 event_id = tvm.hash(abi.encode(event_name, deadline, describe, outcomeNames));        
        _events[event_id] = EventInfo({
            event_name: event_name,
            oracle_fee: oracle_fee,
            deadline: deadline,
            outcomeNames: outcomeNames,
            describe: describe,
            count: 0,
            trustAddr: trustAddr
        });
        
        address addrExtern = address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress);
        emit EventAdded{dest: addrExtern}(event_id, event_name, oracle_fee, deadline);
    }
    
    /// @notice Confirms Oracle's willingness to service an event
    /// @param event_id Event identifier hash
    /// @param oracle_list_hash Hash of oracles
    /// @param token_type Token type
    function confirmEvent(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) 
        public senderIs(DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type)) accept 
    {
        ensureBalance();
        _oracle.transfer({value: 0.1 vmshell, flag: 1, currencies: msg.currencies});
        if (!_events.exists(event_id)) {
            PMP(msg.sender).rejectEvent{value: 0.1 vmshell, flag: 1}();
            return;
        }
        EventInfo eventInfo = _events[event_id];
        eventInfo.count += 1;
        if ((eventInfo.deadline < block.timestamp) || (msg.currencies[CURRENCIES_ID_SHELL] < eventInfo.oracle_fee)) {
            PMP(msg.sender).rejectEvent{value: 0.1 vmshell, flag: 1}();
        } else {
            eventInfo.count += 1;
            _events[event_id] = eventInfo;
            PMP(msg.sender).approveEvent{value: 0.1 vmshell, flag: 1}(_oracle_pubkey, eventInfo.outcomeNames, eventInfo.describe, eventInfo.event_name, eventInfo.trustAddr);
            address addrExtern = address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress);
            emit EventConfirmed{dest: addrExtern}(event_id, msg.sender);
        }
    }

    /// @notice Cancel Oracle's willingness to service an event
    /// @param event_id Event identifier hash
    /// @param oracle_list_hash Hash of oracles
    /// @param token_type Token type
    function cancelEvent(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) 
        public senderIs(DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type)) accept 
    {
        ensureBalance();
        EventInfo eventInfo = _events[event_id];
        eventInfo.count -= 1;
        _events[event_id] = eventInfo;
    }

    function deleteEvent(uint256 event_id) public onlyOwnerPubkey(_oracle_pubkey) accept {
        ensureBalance();
        EventInfo eventInfo = _events[event_id];
        if ((eventInfo.count == 0) || (eventInfo.deadline < block.timestamp)) {
            delete _events[event_id];
        }
    } 
    
    /// @notice Returns contract version
    function getVersion() external pure returns (string, string) {
        return (version, "OracleEventList");
    }
}