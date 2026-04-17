pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PMP.sol";
import "./libraries/DexLib.sol";

/// @title Oracle Event List Contract
contract OracleEventList is Modifiers {

    /// @notice Contract semantic version.
    string constant version = "1.1.0";

    /// @notice Oracle contract address bound to this list (state-init static field).
    address static _oracle;
    /// @notice OracleEventList index for deterministic deployment (state-init static field).
    uint128 static _index;

    /// @notice Oracle owner pubkey used for access control and approvals.
    uint256 _oracle_pubkey;

    /// @notice Hash of salted PMP code used to validate caller PMP address.
    uint256 _pmpSaltedCodeHash;
    /// @notice Depth of salted PMP code used to validate caller PMP address.
    uint16  _pmpSaltedCodeDepth;

    /// @notice Registry of events managed by this OracleEventList.
    mapping(uint256 => EventInfo) public _events;

    /// @notice Emitted when a new event is added to the registry.
    /// @param event_id Deterministic event identifier hash.
    /// @param event_name Human-readable event name.
    /// @param oracle_fee Oracle fee required to confirm the event.
    /// @param deadline Service deadline timestamp.
    event EventAdded(uint256 event_id, string event_name, uint128 oracle_fee, uint64 deadline);
    
    /// @notice Emitted when an event is confirmed for a PMP.
    /// @param event_id Event identifier hash.
    /// @param pmpAddress PMP address that received confirmation.
    event EventConfirmed(uint256 event_id, address pmpAddress);

    /// @notice Initializes OracleEventList parameters.
    /// @param pubkey Oracle owner pubkey.
    /// @param pmpSaltedCodeHash Hash of salted PMP code.
    /// @param pmpSaltedCodeDepth Depth of salted PMP code.
    constructor(uint256 pubkey, uint256 pmpSaltedCodeHash, uint16 pmpSaltedCodeDepth) {
        tvm.accept();
        _oracle = msg.sender;
        _oracle_pubkey = pubkey;
        _pmpSaltedCodeHash = pmpSaltedCodeHash;
        _pmpSaltedCodeDepth = pmpSaltedCodeDepth;
    }

    /// @notice Ensures minimal native balance for operations.
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }
    
    /// @notice Adds a new event that Oracle is willing to service
    /// @param event_name Human-readable event name
    /// @param oracle_fee Oracle fee
    /// @param deadline Timestamp when Oracle is ready to service
    /// @param describe Human-readable event description passed to PMP on approval.
    /// @param outcomeNames Mapping of outcome id to outcome label.
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
        require(outcomeNames.keys().length >= 2, ERR_INVALID_PARAMS);
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

    /// @notice Confirms an event for a PMP after fee and deadline checks.
    /// @param event_id Event identifier hash.
    /// @param oracle_list_hash Hash of PMP oracle list.
    /// @param token_type PMP token type.
    function confirmEvent(uint256 event_id, uint256 oracle_list_hash, uint32 token_type)
        public senderIs(DexLib.computePMPAddressFromHash(_pmpSaltedCodeHash, _pmpSaltedCodeDepth, event_id, oracle_list_hash, token_type)) accept
    {
        ensureBalance();
        _oracle.transfer({value: 0.1 vmshell, flag: 1, currencies: msg.currencies, dest_dapp_id: ORACLE_DAPP_ID});
        if (!_events.exists(event_id)) {
            PMP(msg.sender).rejectEvent{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}();
            return;
        }
        EventInfo eventInfo = _events[event_id];
        if ((eventInfo.deadline < block.timestamp) || (msg.currencies[CURRENCIES_ID_SHELL] < eventInfo.oracle_fee)) {
            PMP(msg.sender).rejectEvent{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}();
        } else {
            eventInfo.count += 1;
            _events[event_id] = eventInfo;
            PMP(msg.sender).approveEvent{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}(_oracle_pubkey, eventInfo.outcomeNames, eventInfo.describe, eventInfo.event_name, eventInfo.trustAddr);
            address addrExtern = address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress);
            emit EventConfirmed{dest: addrExtern}(event_id, msg.sender);
        }
    }

    /// @notice Decrements active confirmation counter for an event when PMP is canceled.
    /// @param event_id Event identifier hash.
    /// @param oracle_list_hash Hash of PMP oracle list.
    /// @param token_type PMP token type.
    function cancelEvent(uint256 event_id, uint256 oracle_list_hash, uint32 token_type)
        public senderIs(DexLib.computePMPAddressFromHash(_pmpSaltedCodeHash, _pmpSaltedCodeDepth, event_id, oracle_list_hash, token_type)) accept
    {
        ensureBalance();
        EventInfo eventInfo = _events[event_id];
        eventInfo.count -= 1;
        _events[event_id] = eventInfo;
    }

    /// @notice Deletes an event when there are no active confirmations or deadline is expired.
    /// @param event_id Event identifier hash.
    function deleteEvent(uint256 event_id) public onlyOwnerPubkey(_oracle_pubkey) accept {
        ensureBalance();
        EventInfo eventInfo = _events[event_id];
        if ((eventInfo.count == 0) || (eventInfo.deadline < block.timestamp)) {
            delete _events[event_id];
        }
    } 
    
    /// @notice Returns contract version
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "OracleEventList");
    }
}
