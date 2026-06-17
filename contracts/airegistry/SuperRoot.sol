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
    string constant version = "1.0.0";

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
    uint256 constant ROOT_MODEL_CODE_HASH        = 0xca737bfbf97d22527fb0685c1241aeb4b02e677949f5054eb8186ea537d89fc6;
    uint256 constant MANIFEST_METADATA_CODE_HASH = 0x537fc452f2514c56a831f67d265b478a969e42a3597ae78abb7552efbc4420e1;

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
