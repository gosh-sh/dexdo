/*
 * Copyright (c) GOSH Technology Ltd. All rights reserved.
 * 
 * Acki Nacki and GOSH are either registered trademarks or trademarks of GOSH
 * 
 * Licensed under the ANNL. See License.txt in the project root for license information.
*/
pragma gosh-solidity >=0.76.1;

import "../PrivateNote.sol";
import "../PMP.sol";
import "../Oracle.sol";
import "../OracleEventList.sol";

library DexLib {

    /// @notice Computes PrivateNote address by deposit identifier hash
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param deposit_identifier_hash Unique identifier for the deposit
    /// @return PrivateNote contract address
    function computePrivateNoteAddress(TvmCell PrivateNoteCode, uint256 deposit_identifier_hash) public returns(address) {
        TvmCell s1 = buildPrivateNoteInitData(PrivateNoteCode, deposit_identifier_hash);
        return address.makeAddrStd(0, tvm.hash(s1));
    }

    /// @notice Builds StateInit for PrivateNote contract
    /// @param deposit_identifier_hash Unique identifier for the deposit
    /// @return StateInit cell for PrivateNote
    function buildPrivateNoteInitData(TvmCell PrivateNoteCode, uint256 deposit_identifier_hash)
        public returns (TvmCell)
    {

        return abi.encodeStateInit({
            contr: PrivateNote,
            varInit: {
                _deposit_identifier_hash: deposit_identifier_hash
            },
            code: PrivateNoteCode
        });
    }

    /// @notice Computes PMP address by name
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param pmpCode Code of PMP contract
    /// @param event_id Event identifier
    /// @param oracle_list_hash Hash of Oracle list
    /// @param token_type Token type
    /// @return PMP contract address
    function computePMPAddress(TvmCell PrivateNoteCode, TvmCell pmpCode, uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public returns (address) {
        TvmCell stateInit = buildPMPStateInit(PrivateNoteCode, pmpCode, event_id, oracle_list_hash, token_type);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds StateInit for PMP contract
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param pmpCode Code of PMP contract
    /// @param event_id Event identifier
    /// @param oracle_list_hash Hash of Oracle list
    /// @param token_type Token type
    /// @return StateInit cell for PMP
    function buildPMPStateInit(TvmCell PrivateNoteCode, TvmCell pmpCode, uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public returns (TvmCell) {
        TvmCell code = buildPMPCode(PrivateNoteCode, pmpCode);
        return abi.encodeStateInit({
            contr: PMP,
            varInit: {
                _event_id: event_id,
                _oracle_list_hash: oracle_list_hash,
                _token_type: token_type
            },
            code: code
        });
    }

    /// @notice Builds PMP code with embedded PrivateNote code
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param pmpCode Code of PMP contract
    /// @return PMP code with embedded PrivateNote code
    function buildPMPCode(
        TvmCell PrivateNoteCode,
        TvmCell pmpCode
    ) public returns (TvmCell) {
        TvmCell salt = abi.encode(PrivateNoteCode);
        return abi.setCodeSalt(pmpCode, salt);
    }

    /// @notice Computes Oracle address by name
    /// @param oracleCode Code of Oracle contract
    /// @param name Oracle name
    /// @return Oracle contract address
    function computeOracleAddress(TvmCell oracleCode, string name) public returns (address) {
        TvmCell stateInit = buildOracleStateInit(oracleCode, name);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds StateInit for Oracle contract
    /// @param oracleCode Code of Oracle contract
    /// @param name Oracle name
    /// @return StateInit cell for Oracle
    function buildOracleStateInit(TvmCell oracleCode, string name)
        public returns (TvmCell)
    {
        return abi.encodeStateInit({
            contr: Oracle,
            varInit: {
                _name: name
            },
            code: oracleCode
        });
    }

    /// @notice Computes OracleEventList address by oracle address
    /// @param oracleEventListCode Code of OracleEventList contract
    /// @param oracle Oracle address
    /// @param index EventList index
    /// @return OracleEventList contract address
    function computeOracleEventListAddress(TvmCell oracleEventListCode, address oracle, uint128 index) public returns (address) {
        TvmCell stateInit = buildOracleEventListStateInit(oracleEventListCode, oracle, index);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds StateInit for OracleEventList contract
    /// @param oracleEventListCode Code of OracleEventList contract
    /// @param oracle Oracle address
    /// @param index EventList index
    /// @return StateInit cell for OracleEventList
    function buildOracleEventListStateInit(TvmCell oracleEventListCode, address oracle, uint128 index)
        public returns (TvmCell)
    {
        return abi.encodeStateInit({
            contr: OracleEventList,
            varInit: {
                _oracle: oracle,
                _index: index
            },
            code: oracleEventListCode
        });
    }
}
