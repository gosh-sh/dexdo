pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PMP.sol";
import "./libraries/DexLib.sol";

/// @title Oracle Event List Contract
contract OracleEventList is Modifiers {

    /// @notice Contract semantic version.
    string constant version = "1.4.0";

    /// @notice Oracle contract address bound to this list (state-init static field).
    address static _oracle;
    /// @notice OracleEventList index for deterministic deployment (state-init static field).
    uint128 static _index;

    /// @notice Oracle owner pubkey used for access control and approvals.
    uint256 _oraclePubkey;

    /// @notice Hash of salted PMP code used to validate caller PMP address.
    uint256 _pmpSaltedCodeHash;
    /// @notice Depth of salted PMP code used to validate caller PMP address.
    uint16  _pmpSaltedCodeDepth;

    /// @notice Human-readable description of this OracleEventList (set at deploy).
    string _description;

    /// @notice Registry of events managed by this OracleEventList.
    mapping(uint256 => EventInfo) public _events;

    /// @notice Emitted when a new event is added to the registry.
    /// @param eventId Deterministic event identifier hash.
    /// @param eventName Human-readable event name.
    /// @param oracleFee Oracle fee required to confirm the event.
    /// @param deadline Service deadline timestamp.
    event EventAdded(uint256 eventId, string eventName, uint128 oracleFee, uint64 deadline);
    
    /// @notice Emitted when an event is confirmed for a PMP.
    /// @param eventId Event identifier hash.
    /// @param pmpAddress PMP address that received confirmation.
    event EventConfirmed(uint256 eventId, address pmpAddress);

    /// @notice Emitted when the list description is updated via setDescription.
    /// @param description New description string.
    event DescriptionUpdated(string description);

    /// @notice Initializes OracleEventList parameters.
    /// @param pubkey Oracle owner pubkey.
    /// @param pmpSaltedCodeHash Hash of salted PMP code.
    /// @param pmpSaltedCodeDepth Depth of salted PMP code.
    /// @param description Human-readable description of this list.
    constructor(
        uint256 pubkey,
        uint256 pmpSaltedCodeHash,
        uint16 pmpSaltedCodeDepth,
        string description
    ) {
        tvm.accept();
        // `_oracle` is a static field (set via stateInit to the legitimate
        // Oracle address). Without this check, the old code was
        //     _oracle = msg.sender;
        // which an attacker could exploit by race-deploying OEL at the
        // deterministic address using a stateInit where _oracle == legit
        // Oracle, letting the constructor overwrite _oracle with the
        // attacker's own address. Check sender matches static field instead.
        require(msg.sender == _oracle, ERR_INVALID_SENDER);
        // pubkey=0 would hand OEL admin access (addEvent/deleteEvent) to any
        // keyless ext tx — and since PMP.approveEvent propagates this into
        // _oracleEventsPubkeys, it would also break governance on downstream
        // PMPs. Reject at deploy.
        require(pubkey != 0, ERR_INVALID_PARAMS);
        _oraclePubkey = pubkey;
        _pmpSaltedCodeHash = pmpSaltedCodeHash;
        _pmpSaltedCodeDepth = pmpSaltedCodeDepth;
        _description = description;
    }

    /// @notice Ensures minimal native balance for operations.
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    /// @notice Updates the human-readable description of this list. Only the
    ///         Oracle owner pubkey may rotate it.
    /// @param description New description string.
    function setDescription(string description) public onlyOwnerPubkey(_oraclePubkey) accept {
        ensureBalance();
        _description = description;
        address addrExtern = address.makeAddrExtern(ORACLE_LIST_DESCRIPTION_UPDATED, bitCntAddress);
        emit DescriptionUpdated{dest: addrExtern}(description);
    }
    
    /// @notice Adds a new event that Oracle is willing to service
    /// @param eventName Human-readable event name
    /// @param oracleFee Oracle fee
    /// @param deadline Timestamp when Oracle is ready to service
    /// @param describe Human-readable event description passed to PMP on approval.
    /// @param outcomeNames Mapping of outcome id to outcome label.
    /// @param trustAddr Trusted addr for oracle event
    function addEvent(
        string eventName,
        uint128 oracleFee,
        uint64 deadline,
        string describe,
        mapping(uint32 => string) outcomeNames,
        optional(uint256) trustAddr
    ) public onlyOwnerPubkey(_oraclePubkey) accept {
        require(deadline > block.timestamp, ERR_INVALID_PARAMS);
        ensureBalance();
        require(outcomeNames.keys().length >= 2, ERR_INVALID_PARAMS);
        require(outcomeNames.keys().length < 20, ERR_INVALID_PARAMS);
        uint256 eventId = tvm.hash(abi.encode(eventName, deadline, describe, outcomeNames));
        require(!_events.exists(eventId), ERR_ALREADY_INITIALIZED);
        _events[eventId] = EventInfo({
            eventName: eventName,
            oracleFee: oracleFee,
            deadline: deadline,
            outcomeNames: outcomeNames,
            describe: describe,
            count: 0,
            trustAddr: trustAddr
        });

        address addrExtern = address.makeAddrExtern(ORACLE_EVENT_ADDED, bitCntAddress);
        emit EventAdded{dest: addrExtern}(eventId, eventName, oracleFee, deadline);
    }

    /// @notice Confirms an event for a PMP after fee and deadline checks.
    /// @param eventId Event identifier hash.
    /// @param oracleListHash Hash of PMP oracle list.
    /// @param tokenType PMP token type.
    function confirmEvent(uint256 eventId, uint256 oracleListHash, uint32 tokenType)
        public senderIs(DexLib.computePMPAddressFromHash(_pmpSaltedCodeHash, _pmpSaltedCodeDepth, eventId, oracleListHash, tokenType)) accept
    {
        ensureBalance();
        _oracle.transfer({value: 0.1 vmshell, flag: 1, currencies: msg.currencies, dest_dapp_id: ORACLE_DAPP_ID});
        if (!_events.exists(eventId)) {
            PMP(msg.sender).rejectEvent{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}();
            return;
        }
        EventInfo eventInfo = _events[eventId];
        if ((eventInfo.deadline < block.timestamp) || (msg.currencies[CURRENCIES_ID_SHELL] < eventInfo.oracleFee)) {
            PMP(msg.sender).rejectEvent{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}();
        } else {
            eventInfo.count += 1;
            _events[eventId] = eventInfo;
            PMP(msg.sender).approveEvent{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}(_oraclePubkey, eventInfo.outcomeNames, eventInfo.describe, eventInfo.eventName, eventInfo.trustAddr);
            address addrExtern = address.makeAddrExtern(ORACLE_EVENT_CONFIRMED, bitCntAddress);
            emit EventConfirmed{dest: addrExtern}(eventId, msg.sender);
        }
    }

    /// @notice Decrements active confirmation counter for an event when PMP is canceled.
    /// @param eventId Event identifier hash.
    /// @param oracleListHash Hash of PMP oracle list.
    /// @param tokenType PMP token type.
    function cancelEvent(uint256 eventId, uint256 oracleListHash, uint32 tokenType)
        public senderIs(DexLib.computePMPAddressFromHash(_pmpSaltedCodeHash, _pmpSaltedCodeDepth, eventId, oracleListHash, tokenType)) accept
    {
        ensureBalance();
        EventInfo eventInfo = _events[eventId];
        eventInfo.count -= 1;
        _events[eventId] = eventInfo;
    }

    /// @notice Deletes an event when there are no active confirmations or deadline is expired.
    /// @param eventId Event identifier hash.
    function deleteEvent(uint256 eventId) public onlyOwnerPubkey(_oraclePubkey) accept {
        ensureBalance();
        EventInfo eventInfo = _events[eventId];
        if ((eventInfo.count == 0) && (eventInfo.deadline < block.timestamp)) {
            delete _events[eventId];
        }
    } 
    
    /// @notice Returns contract version
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "OracleEventList");
    }
}
