pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./RootModel.sol";

/// @title SuperRoot
/// @notice Global registry for all RootModel contracts.
///         Two supported deploy paths:
///           - **Zerostate**: pre-placed at a chosen vanity address (e.g.,
///             `0:a11a...a11a`) via `zerostate-helper add contract`.
///           - **External message**: deployed via constructor at the
///             deterministic `tvm.hash(stateInit)` address (one canonical
///             address per code version since there are no static fields).
///         The RootModel code is passed once to the constructor and never
///         changed; RootModel addresses derived by this SuperRoot are stable
///         for its lifetime. Children bind to this specific SuperRoot instance
///         via their `_superRootAddress` static, which is mixed into the
///         derivation here.
contract SuperRoot is AiRegistryModifiers {
    string constant version = "4.0.10";

    /// @notice Canonical code hash of the child RootModel. The constructor
    ///         rejects any caller-supplied code whose `tvm.hash` does not
    ///         match this constant — this locks the registry to one
    ///         specific RootModel bytecode forever. To bump versions,
    ///         rebuild RootModel, recompute the hash below, recompile
    ///         SuperRoot, and redeploy.
    /// @dev    Hash is for the child *code* cell (not stateInit), i.e.
    ///         the value returned by `tvm.decodeStateInit(tvc).code` then
    ///         `tvm.hash`. It is the same as the `code_hash` field
    ///         printed by `tvm-cli decode stateinit --tvc <file>`.
    uint256 constant ROOT_MODEL_CODE_HASH = 0x573211b372bb2a58349cc8d7ea3b93498bbb1ef5e02ef626a25146b3541cfb85;

    event RootRegistered(address rootAddress);

    uint256 _ownerPubkey;
    TvmCell _rootModelCode;

    constructor(uint256 pubkey, TvmCell rootModelCode) accept {
        require(tvm.hash(rootModelCode) == ROOT_MODEL_CODE_HASH, ERR_BAD_CODE_HASH);
        _ownerPubkey = pubkey;
        _rootModelCode = rootModelCode;
    }

    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) { return; }
        gosh.mintshellq(MIN_BALANCE);
    }

    // ========================================================
    // Owner pubkey rotation (code is immutable — see contract header)
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

    // ========================================================
    // Getters
    // ========================================================

    function getRootModelAddress(uint256 ownerPubkey) external view returns (address) {
        return _calculateRootModelAddress(ownerPubkey);
    }

    function getOwnerPubkey() external view returns (uint256) { return _ownerPubkey; }

    function getVersion() external pure returns (string, string) {
        return (version, "SuperRoot");
    }
}
