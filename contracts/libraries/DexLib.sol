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
import "../OrderBook.sol";

/// @title DexLib
/// @notice Utility library for deterministic address and StateInit/code construction.
library DexLib {

    /// @notice Computes deterministic PrivateNote address for a deposit hash.
    /// @param PrivateNoteCode PrivateNote contract code.
    /// @param deposit_identifier_hash Deposit identifier hash.
    /// @return PrivateNote deterministic address.
    function computePrivateNoteAddress(TvmCell PrivateNoteCode, uint256 deposit_identifier_hash) public returns(address) {
        TvmCell s1 = buildPrivateNoteInitData(PrivateNoteCode, deposit_identifier_hash);
        return address.makeAddrStd(0, tvm.hash(s1));
    }

    /// @notice Builds StateInit for PrivateNote contract
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param deposit_identifier_hash Unique identifier for the deposit
    /// @return StateInit cell for PrivateNote
    function buildPrivateNoteInitData(TvmCell PrivateNoteCode, uint256 deposit_identifier_hash) public returns (TvmCell) {
        return abi.encodeStateInit({
            contr: PrivateNote,
            varInit: { _deposit_identifier_hash: deposit_identifier_hash },
            code: PrivateNoteCode
        });
    }

    /// @notice Computes deterministic PMP address for event and oracle set.
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

    /// @notice Builds PMP StateInit with salted code and static ids.
    /// @param PrivateNoteCode PrivateNote contract code used for salt.
    /// @param pmpCode PMP base code.
    /// @param event_id Event identifier.
    /// @param oracle_list_hash Hash of oracle set.
    /// @param token_type Token type.
    /// @return PMP StateInit cell.
    function buildPMPStateInit(TvmCell PrivateNoteCode, TvmCell pmpCode, uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public returns (TvmCell) {
        TvmCell code = buildPMPCode(PrivateNoteCode, pmpCode);
        return abi.encodeStateInit({
            contr: PMP,
            varInit: { _event_id: event_id, _oracle_list_hash: oracle_list_hash, _token_type: token_type },
            code: code
        });
    }

    /// @notice Produces salted PMP code where salt stores PrivateNote code.
    /// @param PrivateNoteCode PrivateNote contract code used as salt payload.
    /// @param pmpCode PMP base code.
    /// @return Salted PMP code cell.
    function buildPMPCode(TvmCell PrivateNoteCode, TvmCell pmpCode) public returns (TvmCell) {
        TvmCell salt = abi.encode(PrivateNoteCode);
        return abi.setCodeSalt(pmpCode, salt);
    }

    /// @notice Computes deterministic Oracle address by name.
    /// @param oracleCode Oracle contract code.
    /// @param name Oracle unique name.
    /// @return Oracle deterministic address.
    function computeOracleAddress(TvmCell oracleCode, string name) public returns (address) {
        TvmCell stateInit = buildOracleStateInit(oracleCode, name);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds Oracle StateInit for a given name.
    /// @param oracleCode Oracle contract code.
    /// @param name Oracle unique name.
    /// @return Oracle StateInit cell.
    function buildOracleStateInit(TvmCell oracleCode, string name) public returns (TvmCell) {
        return abi.encodeStateInit({
            contr: Oracle,
            varInit: { _name: name },
            code: oracleCode
        });
    }

    /// @notice Computes deterministic OracleEventList address.
    /// @param oracleEventListCode OracleEventList contract code.
    /// @param oracle Oracle contract address.
    /// @param index OracleEventList index.
    /// @return OracleEventList deterministic address.
    function computeOracleEventListAddress(TvmCell oracleEventListCode, address oracle, uint128 index) public returns (address) {
        TvmCell stateInit = buildOracleEventListStateInit(oracleEventListCode, oracle, index);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds OracleEventList StateInit for `(oracle, index)`.
    /// @param oracleEventListCode OracleEventList contract code.
    /// @param oracle Oracle contract address.
    /// @param index OracleEventList index.
    /// @return OracleEventList StateInit cell.
    function buildOracleEventListStateInit(TvmCell oracleEventListCode, address oracle, uint128 index) public returns (TvmCell) {
        return abi.encodeStateInit({
            contr: OracleEventList,
            varInit: { _oracle: oracle, _index: index },
            code: oracleEventListCode
        });
    }

    /// @notice Computes deterministic OrderBook address for a market.
    /// @param PrivateNoteCode PrivateNote contract code used for salt.
    /// @param orderBookCode OrderBook base code.
    /// @param event_id Event identifier.
    /// @param oracle_list_hash Oracle list hash.
    /// @param token_type Token type.
    /// @return OrderBook deterministic address.
    function computeOrderBookAddress(TvmCell PrivateNoteCode, TvmCell orderBookCode, uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public returns (address) {
        TvmCell stateInit = buildOrderBookStateInit(PrivateNoteCode, orderBookCode, event_id, oracle_list_hash, token_type);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds OrderBook StateInit with salted code and static ids.
    /// @param PrivateNoteCode PrivateNote contract code used for salt.
    /// @param orderBookCode OrderBook base code.
    /// @param event_id Event identifier.
    /// @param oracle_list_hash Oracle list hash.
    /// @param token_type Token type.
    /// @return OrderBook StateInit cell.
    function buildOrderBookStateInit(TvmCell PrivateNoteCode, TvmCell orderBookCode, uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public returns (TvmCell) {
        TvmCell code = buildOrderBookCode(PrivateNoteCode, orderBookCode);
        return abi.encodeStateInit({
            contr: OrderBook,
            varInit: { _event_id: event_id, _oracle_list_hash: oracle_list_hash, _token_type: token_type },
            code: code
        });
    }

    /// @notice Produces salted OrderBook code where salt stores PrivateNote code.
    /// @param PrivateNoteCode PrivateNote contract code used as salt payload.
    /// @param orderBookCode OrderBook base code.
    /// @return Salted OrderBook code cell.
    function buildOrderBookCode(TvmCell PrivateNoteCode, TvmCell orderBookCode) public returns (TvmCell) {
        TvmCell salt = abi.encode(PrivateNoteCode);
        return abi.setCodeSalt(orderBookCode, salt);
    }

    // ═══ Hash-based address computation (stores uint256+uint16 instead of TvmCell) ═══

    /// @notice Extracts data cell from StateInit.
    /// @param stateInit StateInit cell.
    /// @return Extracted data cell.
    function _extractDataCell(TvmCell stateInit) private returns (TvmCell) {
        TvmSlice s = stateInit.toSlice();
        s.skip(5);
        s.loadRef();
        return s.loadRef();
    }

    /// @notice Computes deterministic PMP address from salted code hash/depth.
    /// @param saltedCodeHash Salted PMP code hash.
    /// @param saltedCodeDepth Salted PMP code depth.
    /// @param event_id Event identifier.
    /// @param oracle_list_hash Oracle list hash.
    /// @param token_type Token type.
    /// @return PMP deterministic address.
    function computePMPAddressFromHash(
        uint256 saltedCodeHash, uint16 saltedCodeDepth,
        uint256 event_id, uint256 oracle_list_hash, uint32 token_type
    ) public returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: PMP, code: dummyCode,
            varInit: { _event_id: event_id, _oracle_list_hash: oracle_list_hash, _token_type: token_type }
        });
        TvmCell dataCell = _extractDataCell(si);
        return address.makeAddrStd(0, abi.stateInitHash(saltedCodeHash, tvm.hash(dataCell), saltedCodeDepth, dataCell.depth()));
    }

    /// @notice Computes deterministic Oracle address from code hash/depth.
    /// @param codeHash Oracle code hash.
    /// @param codeDepth Oracle code depth.
    /// @param name Oracle unique name.
    /// @return Oracle deterministic address.
    function computeOracleAddressFromHash(uint256 codeHash, uint16 codeDepth, string name) public returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: Oracle, code: dummyCode,
            varInit: { _name: name }
        });
        TvmCell dataCell = _extractDataCell(si);
        return address.makeAddrStd(0, abi.stateInitHash(codeHash, tvm.hash(dataCell), codeDepth, dataCell.depth()));
    }

    /// @notice Computes deterministic OracleEventList address from code hash/depth.
    /// @param codeHash OracleEventList code hash.
    /// @param codeDepth OracleEventList code depth.
    /// @param oracle Oracle address.
    /// @param index OracleEventList index.
    /// @return OracleEventList deterministic address.
    function computeOracleEventListAddressFromHash(uint256 codeHash, uint16 codeDepth, address oracle, uint128 index) public returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: OracleEventList, code: dummyCode,
            varInit: { _oracle: oracle, _index: index }
        });
        TvmCell dataCell = _extractDataCell(si);
        return address.makeAddrStd(0, abi.stateInitHash(codeHash, tvm.hash(dataCell), codeDepth, dataCell.depth()));
    }

    /// @notice Computes deterministic OrderBook address from salted code hash/depth.
    /// @param saltedCodeHash Salted OrderBook code hash.
    /// @param saltedCodeDepth Salted OrderBook code depth.
    /// @param event_id Event identifier.
    /// @param oracle_list_hash Oracle list hash.
    /// @param token_type Token type.
    /// @return OrderBook deterministic address.
    function computeOrderBookAddressFromHash(
        uint256 saltedCodeHash, uint16 saltedCodeDepth,
        uint256 event_id, uint256 oracle_list_hash, uint32 token_type
    ) public returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: OrderBook, code: dummyCode,
            varInit: { _event_id: event_id, _oracle_list_hash: oracle_list_hash, _token_type: token_type }
        });
        TvmCell dataCell = _extractDataCell(si);
        return address.makeAddrStd(0, abi.stateInitHash(saltedCodeHash, tvm.hash(dataCell), saltedCodeDepth, dataCell.depth()));
    }
}
