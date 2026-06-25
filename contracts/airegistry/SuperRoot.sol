pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./RootModel.sol";
import "./ManifestMetadata.sol";

/// @title SuperRoot
/// @notice Global registry for all RootModel and ManifestMetadata contracts.
///         Two supported deploy paths:
///           - **Zerostate**: pre-placed at a chosen vanity address (e.g.,
///             `0:a11a...a11a`) via `zerostate-helper add contract`.
///           - **External message**: deployed via constructor at the
///             deterministic `tvm.hash(stateInit)` address (one canonical
///             address per code version since there are no static fields).
///         Codes are passed once to the constructor and never changed;
///         RootModel/ManifestMetadata addresses derived by this SuperRoot
///         are stable for its lifetime. Children bind to this specific
///         SuperRoot instance via their `_superRootAddress` static, which
///         is mixed into the derivation here.
contract SuperRoot is AiRegistryModifiers {
    string constant version = "4.0.3";

    /// @notice Canonical code hashes of the child contracts. The constructor
    ///         rejects any caller-supplied code whose `tvm.hash` does not
    ///         match these constants — this locks the registry to one
    ///         specific RootModel/ManifestMetadata bytecode forever. To
    ///         bump versions, rebuild the children, recompute the hashes
    ///         below, recompile SuperRoot, and redeploy.
    /// @dev    Hashes are for the child *code* cells (not stateInit), i.e.
    ///         the value returned by `tvm.decodeStateInit(tvc).code` then
    ///         `tvm.hash`. They are the same as the `code_hash` field
    ///         printed by `tvm-cli decode stateinit --tvc <file>`.
    uint256 constant ROOT_MODEL_CODE_HASH        = 0x769dc56e5d3b8f99d4b253e4befefbd1360fbf098ed81b16ed09010246ea9f78;
    uint256 constant MANIFEST_METADATA_CODE_HASH = 0x416d76c8a885ae7914603886e5ea8b1b2fa0112466089ad30cbb53ad4519a0e0;

    event RootRegistered(address rootAddress);
    event ManifestRegistered(address manifestAddress);

    uint256 _ownerPubkey;
    TvmCell _rootModelCode;
    TvmCell _manifestCode;

    constructor(uint256 pubkey, TvmCell rootModelCode, TvmCell manifestCode) accept {
        require(tvm.hash(rootModelCode) == ROOT_MODEL_CODE_HASH, ERR_BAD_CODE_HASH);
        require(tvm.hash(manifestCode) == MANIFEST_METADATA_CODE_HASH, ERR_BAD_CODE_HASH);
        _ownerPubkey = pubkey;
        _rootModelCode = rootModelCode;
        _manifestCode = manifestCode;
    }

    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) { return; }
        gosh.mintshellq(MIN_BALANCE);
    }

    // ========================================================
    // Owner pubkey rotation (codes are immutable — see contract header)
    // ========================================================

    function setPubkey(uint256 pubkey) public onlyOwnerPubkey(_ownerPubkey) accept {
        ensureBalance();
        _ownerPubkey = pubkey;
    }

    // ========================================================
    // Address derivation
    // ========================================================

    function _calculateRootModelAddress(uint256 ownerPubkey) private view returns (address) {
        TvmCell s = abi.encodeStateInit({
            code: _rootModelCode,
            contr: RootModel,
            pubkey: ownerPubkey,
            varInit: {
                _ownerPubkey: ownerPubkey,
                _superRootAddress: address(this)
            }
        });
        return address.makeAddrStd(0, tvm.hash(s));
    }

    function _calculateManifestAddress(uint256 ownerPubkey, address rootModelAddress) private view returns (address) {
        TvmCell s = abi.encodeStateInit({
            code: _manifestCode,
            contr: ManifestMetadata,
            pubkey: ownerPubkey,
            varInit: {
                _ownerPubkey: ownerPubkey,
                _rootModelAddress: rootModelAddress,
                _superRootAddress: address(this)
            }
        });
        return address.makeAddrStd(0, tvm.hash(s));
    }

    // ========================================================
    // Registration entry points (called by child contracts)
    // ========================================================

    function registerRoot(uint256 ownerPubkey) public {
        ensureBalance();
        address expected = _calculateRootModelAddress(ownerPubkey);
        require(msg.sender == expected, ERR_INVALID_SENDER);
        tvm.accept();

        address regExtern = address.makeAddrExtern(RootRegisteredEmit, bitCntAddress);
        emit RootRegistered{dest: regExtern}(expected);
    }

    function registerManifest(uint256 ownerPubkey, address rootModelAddress) public {
        ensureBalance();
        address expected = _calculateManifestAddress(ownerPubkey, rootModelAddress);
        require(msg.sender == expected, ERR_INVALID_SENDER);
        tvm.accept();

        address regExtern = address.makeAddrExtern(ManifestRegisteredEmit, bitCntAddress);
        emit ManifestRegistered{dest: regExtern}(expected);
    }

    // ========================================================
    // Getters
    // ========================================================

    function getRootModelAddress(uint256 ownerPubkey) external view returns (address) {
        return _calculateRootModelAddress(ownerPubkey);
    }

    function getManifestAddress(uint256 ownerPubkey, address rootModelAddress) external view returns (address) {
        return _calculateManifestAddress(ownerPubkey, rootModelAddress);
    }

    function getOwnerPubkey() external view returns (uint256) { return _ownerPubkey; }

    function getVersion() external pure returns (string, string) {
        return (version, "SuperRoot");
    }
}
