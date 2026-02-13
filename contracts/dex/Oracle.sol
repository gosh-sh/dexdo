pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./OracleEventList.sol";
import "./libraries/DexLib.sol";

/// @title Oracle Contract
contract Oracle is Modifiers {

    string constant version = "1.0.0";

    /// @notice Oracle's public key for authorization
    uint256 _oraclePubkey;

    /// @notice Code of OracleEventList contract for deployment
    TvmCell _oracleEventListCode;

    /// @notice Code of PrivateNote contract for address computations
    TvmCell _PrivateNoteCode;

    /// @notice Code of PMP contract for event management
    TvmCell _pmpCode;

    /// @notice Static name of the Oracle (unique identifier)
    string static _name;

    /// Events

    /// @notice Emitted when a new OracleEventList is deployed.
    /// @param eventListAddress Address of the deployed OracleEventList contract.
    /// @param index Index of the deployed list (shard/partition identifier).
    event OracleEventListDeployed(address eventListAddress, uint128 index);

    /// @notice Emitted when an event is published by the oracle subsystem.
    /// @dev This event may be emitted by external flows; keeping it here for a unified external ABI.
    /// @param event_id Unique identifier of the published event.
    /// @param event_name Human-readable name of the event.
    event EventPublished(uint256 event_id, string event_name);
    
    /// @notice Oracle constructor - deploys initial OracleEventList
    /// @param oraclePubkey Oracle's public key for authorization
    /// @param oracleEventListCode Code of OracleEventList contract
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param pmpCode Code of PMP contract
    constructor(
        uint256 oraclePubkey, 
        TvmCell oracleEventListCode, 
        TvmCell PrivateNoteCode, 
        TvmCell pmpCode
    ) {
        tvm.accept();
        
        require(msg.sender == ROOT_ORACLE_ADDRESS, ERR_INVALID_SENDER);
        
        _oraclePubkey = oraclePubkey;
        _oracleEventListCode = oracleEventListCode;
        _PrivateNoteCode = PrivateNoteCode;
        _pmpCode = pmpCode;
        
        address oracleEventList = new OracleEventList{
            stateInit: DexLib.buildOracleEventListStateInit(
                _oracleEventListCode, 
                address(this), 
                0
            ),
            value: 10 vmshell, 
            flag: 1            
        }(_oraclePubkey, _PrivateNoteCode, _pmpCode);
        
        address addrExtern = address.makeAddrExtern(ORACLE_DEPLOYED, bitCntAddress);
        emit OracleEventListDeployed{dest: addrExtern}(oracleEventList, 0);
    }

    /// @notice Deploys a new OracleEventList with specified index
    /// @param index Index identifier for the new OracleEventList
    function deployEventList(uint128 index) public view onlyOwnerPubkey(_oraclePubkey) accept {
        ensureBalance();
        
        address oracleEventList = new OracleEventList{
            stateInit: DexLib.buildOracleEventListStateInit(
                _oracleEventListCode, 
                address(this), 
                index
            ),
            value: 10 vmshell,  
            flag: 1            
        }(_oraclePubkey, _PrivateNoteCode, _pmpCode);

        address addrExtern = address.makeAddrExtern(ORACLE_DEPLOYED, bitCntAddress);
        emit OracleEventListDeployed{dest: addrExtern}(oracleEventList, index);
    }

    /// @notice Ensures minimal native balance for operations
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    /// @notice Withdraws accumulated fees to specified address
    /// @param to Recipient address for the fees
    /// @param amount Amount of fees to withdraw
    function withdrawFees(address to, uint128 amount) public view onlyOwnerPubkey(_oraclePubkey) accept {
        mapping(uint32 => varuint32) data;
        data[CURRENCIES_ID_SHELL] = amount;
        to.transfer({
            value: 0.1 vmshell, 
            flag: 1,             
            currencies: data    
        });
    }

    /// @notice Fallback function to receive incoming payments
    receive() external pure {
        tvm.accept();        
        ensureBalance();        
    }

    /// @notice Helper function to encode data for setting stake deadlines
    /// @param stakeStart Timestamp for the start of staking period
    /// @param stakeEnd Timestamp for the end of staking period
    /// @param resultStart Timestamp for the start of result submission period
    /// @param resultEnd Timestamp for the end of result submission period
    /// @return TvmCell Encoded data cell for stake deadline proposal
    function getCellForProposalSetStakeDeadline(
        uint64 stakeStart, 
        uint64 stakeEnd, 
        uint64 resultStart, 
        uint64 resultEnd
    ) public pure returns (TvmCell) {
        return abi.encode(stakeStart, stakeEnd, resultStart, resultEnd);
    }

    /// @notice Helper function to encode data for setting event resolution
    /// @param outcomeId Identifier of the winning outcome
    /// @return TvmCell Encoded data cell for resolution proposal
    function getCellForProposalSetResolve(uint32 outcomeId) public pure returns (TvmCell) {
        return abi.encode(outcomeId);
    }
    
    /// @notice Returns OracleEventList address for specified index
    /// @param index Index of the OracleEventList (currently only 0 supported)
    /// @return address OracleEventList contract address
    function getEventListAddress(uint128 index) external view returns (address) {
        return DexLib.computeOracleEventListAddress(
            _oracleEventListCode, 
            address(this), 
            index
        );
    }
    
    /// @notice Returns contract version identifier
    /// @return string Contract name for version identification
    function getVersion() external pure returns (string, string) {
        return (version, "Oracle");
    }
}