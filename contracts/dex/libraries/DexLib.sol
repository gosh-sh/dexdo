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
import "../../airegistry/InferenceOrderBook.sol";

/// @title DexLib
/// @notice Utility library for deterministic address and StateInit/code construction.
library DexLib {

    /// @notice Computes deterministic PrivateNote address for a deposit hash.
    /// @param PrivateNoteCode PrivateNote contract code.
    /// @param depositIdentifierHash Deposit identifier hash.
    /// @return PrivateNote deterministic address.
    function computePrivateNoteAddress(TvmCell PrivateNoteCode, uint256 depositIdentifierHash) public returns(address) {
        TvmCell s1 = buildPrivateNoteInitData(PrivateNoteCode, depositIdentifierHash);
        return address.makeAddrStd(0, tvm.hash(s1));
    }

    /// @notice Builds StateInit for PrivateNote contract
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param depositIdentifierHash Unique identifier for the deposit
    /// @return StateInit cell for PrivateNote
    function buildPrivateNoteInitData(TvmCell PrivateNoteCode, uint256 depositIdentifierHash) public returns (TvmCell) {
        return abi.encodeStateInit({
            contr: PrivateNote,
            varInit: { _depositIdentifierHash: depositIdentifierHash },
            code: PrivateNoteCode
        });
    }

    /// @notice Computes deterministic PMP address for event and oracle set.
    /// @param PrivateNoteCode Code of PrivateNote contract
    /// @param pmpCode Code of PMP contract
    /// @param eventId Event identifier
    /// @param oracleListHash Hash of Oracle list
    /// @param tokenType Token type
    /// @return PMP contract address
    function computePMPAddress(TvmCell PrivateNoteCode, TvmCell pmpCode, uint256 eventId, uint256 oracleListHash, uint32 tokenType) public returns (address) {
        TvmCell stateInit = buildPMPStateInit(PrivateNoteCode, pmpCode, eventId, oracleListHash, tokenType);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds PMP StateInit with salted code and static ids.
    /// @param PrivateNoteCode PrivateNote contract code used for salt.
    /// @param pmpCode PMP base code.
    /// @param eventId Event identifier.
    /// @param oracleListHash Hash of oracle set.
    /// @param tokenType Token type.
    /// @return PMP StateInit cell.
    function buildPMPStateInit(TvmCell PrivateNoteCode, TvmCell pmpCode, uint256 eventId, uint256 oracleListHash, uint32 tokenType) public returns (TvmCell) {
        TvmCell code = buildPMPCode(PrivateNoteCode, pmpCode);
        return abi.encodeStateInit({
            contr: PMP,
            varInit: { _eventId: eventId, _oracleListHash: oracleListHash, _tokenType: tokenType },
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
    /// @param eventId Event identifier.
    /// @param oracleListHash Oracle list hash.
    /// @param tokenType Token type.
    /// @return OrderBook deterministic address.
    function computeOrderBookAddress(TvmCell PrivateNoteCode, TvmCell orderBookCode, uint256 eventId, uint256 oracleListHash, uint32 tokenType) public returns (address) {
        TvmCell stateInit = buildOrderBookStateInit(PrivateNoteCode, orderBookCode, eventId, oracleListHash, tokenType);
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Builds OrderBook StateInit with salted code and static ids.
    /// @param PrivateNoteCode PrivateNote contract code used for salt.
    /// @param orderBookCode OrderBook base code.
    /// @param eventId Event identifier.
    /// @param oracleListHash Oracle list hash.
    /// @param tokenType Token type.
    /// @return OrderBook StateInit cell.
    function buildOrderBookStateInit(TvmCell PrivateNoteCode, TvmCell orderBookCode, uint256 eventId, uint256 oracleListHash, uint32 tokenType) public returns (TvmCell) {
        TvmCell code = buildOrderBookCode(PrivateNoteCode, orderBookCode);
        return abi.encodeStateInit({
            contr: OrderBook,
            varInit: { _eventId: eventId, _oracleListHash: oracleListHash, _tokenType: tokenType },
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

    // ═══ InferenceOrderBook (deployed FROM a PrivateNote) ═══
    // No code salt: the book binds to the note family via NOTE_CODE_HASH pinned
    // in the InferenceOrderBook code itself, and the deploy is gated in its ctor
    // (deployer must be a genuine note). The address is just (book code + §8
    // statics): same (model, tick) ⇒ same address ⇒ one book.

    /// @notice InferenceOrderBook StateInit: book code + the §8 static set
    ///         (model). One book per model.
    function buildInferenceOrderBookStateInit(TvmCell inferenceOrderBookCode, uint256 modelHash) public returns (TvmCell) {
        return abi.encodeStateInit({
            contr: InferenceOrderBook,
            varInit: { _modelHash: modelHash },
            code: inferenceOrderBookCode
        });
    }

    function computeInferenceOrderBookAddress(TvmCell inferenceOrderBookCode, uint256 modelHash) public returns (address) {
        TvmCell stateInit = buildInferenceOrderBookStateInit(inferenceOrderBookCode, modelHash);
        return address.makeAddrStd(0, tvm.hash(stateInit));
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
    /// @param eventId Event identifier.
    /// @param oracleListHash Oracle list hash.
    /// @param tokenType Token type.
    /// @return PMP deterministic address.
    function computePMPAddressFromHash(
        uint256 saltedCodeHash, uint16 saltedCodeDepth,
        uint256 eventId, uint256 oracleListHash, uint32 tokenType
    ) public returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: PMP, code: dummyCode,
            varInit: { _eventId: eventId, _oracleListHash: oracleListHash, _tokenType: tokenType }
        });
        TvmCell dataCell = _extractDataCell(si);
        return address.makeAddrStd(0, abi.stateInitHash(saltedCodeHash, tvm.hash(dataCell), saltedCodeDepth, dataCell.depth()));
    }

    /// @notice Computes deterministic PrivateNote address from its code hash/depth
    ///         (so callers can store the hash instead of the full note code).
    /// @param codeHash PrivateNote code hash.
    /// @param codeDepth PrivateNote code depth.
    /// @param depositIdentifierHash Deposit identifier hash.
    /// @return PrivateNote deterministic address.
    function computePrivateNoteAddressFromHash(
        uint256 codeHash, uint16 codeDepth, uint256 depositIdentifierHash
    ) public returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: PrivateNote, code: dummyCode,
            varInit: { _depositIdentifierHash: depositIdentifierHash }
        });
        TvmCell dataCell = _extractDataCell(si);
        return address.makeAddrStd(0, abi.stateInitHash(codeHash, tvm.hash(dataCell), codeDepth, dataCell.depth()));
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
    /// @param eventId Event identifier.
    /// @param oracleListHash Oracle list hash.
    /// @param tokenType Token type.
    /// @return OrderBook deterministic address.
    function computeOrderBookAddressFromHash(
        uint256 saltedCodeHash, uint16 saltedCodeDepth,
        uint256 eventId, uint256 oracleListHash, uint32 tokenType
    ) public returns (address) {
        TvmCell dummyCode;
        TvmCell si = abi.encodeStateInit({
            contr: OrderBook, code: dummyCode,
            varInit: { _eventId: eventId, _oracleListHash: oracleListHash, _tokenType: tokenType }
        });
        TvmCell dataCell = _extractDataCell(si);
        return address.makeAddrStd(0, abi.stateInitHash(saltedCodeHash, tvm.hash(dataCell), saltedCodeDepth, dataCell.depth()));
    }
}
