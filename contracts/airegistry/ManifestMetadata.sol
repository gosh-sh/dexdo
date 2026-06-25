pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./interfaces.sol";

/// @title ManifestMetadata
/// @notice Mutable owner-editable metadata store for a model's API schema.
///         One per (rootModelAddress, ownerPubkey).
///
///         The schema is stored as a **mapping of chunks** keyed by an
///         explicit `uint32` index chosen by the owner. This avoids the
///         O(N) cell-tree rebuild cost of concatenating into a single
///         growing `string`, so the contract can hold arbitrarily large
///         schemas (each `setApiSchemaChunk` call writes only its one
///         chunk regardless of how many already exist).
///
///         Conventional layout: keys `0, 1, 2, ...` containing the
///         concatenated schema in order; off-chain readers stitch them
///         back together. Gaps and out-of-order writes are allowed —
///         `_apiSchemaChunkCount` tracks `max(idx) + 1` over the lifetime
///         (the "logical length" of the chunk array), so a sparse layout
///         is observable.
///
///         This is intentionally **not** an authoritative SPC package
///         contract: there is no canonical package hash, version binding,
///         schema validation, finalization, or content-addressed
///         immutability.
contract ManifestMetadata is AiRegistryModifiers {
    string constant version = "4.0.3";

    event ContractDeployed(address self);
    event ManifestUpdated(address self, uint32 chunkIdx);

    // Static (part of stateInit, contribute to address derivation).
    // `_superRootAddress` binds this ManifestMetadata to a specific
    // SuperRoot instance — see RootModel for the rationale.
    uint256 static _ownerPubkey;
    address static _rootModelAddress;
    address static _superRootAddress;

    mapping(uint32 => string) _apiSchemaChunks;
    uint32 _apiSchemaChunkCount;   // max(written idx) + 1; never decreases

    /// @param firstChunk the chunk written at index 0. Pass `""` to deploy
    ///                   an empty manifest and populate it later via
    ///                   `setApiSchemaChunk`.
    constructor(string firstChunk) {
        tvm.accept();
        if (bytes(firstChunk).length > 0) {
            _apiSchemaChunks[0] = firstChunk;
            _apiSchemaChunkCount = 1;
        }

        ensureBalance();

        address selfExtern = address.makeAddrExtern(ContractDeployedEmit, bitCntAddress);
        emit ContractDeployed{dest: selfExtern}(address(this));

        ISuperRootRegistry(_superRootAddress).registerManifest{value: REGISTER_FORWARD_VALUE, flag: 1}(_ownerPubkey, _rootModelAddress);
    }

    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) { return; }
        gosh.mintshellq(MIN_BALANCE);
    }

    /// @notice Write (or overwrite) the chunk at index `idx`. `_apiSchemaChunkCount`
    ///         advances to `max(prevCount, idx + 1)`.
    function setApiSchemaChunk(uint32 idx, string chunk) public onlyOwnerPubkey(_ownerPubkey) accept {
        ensureBalance();
        _apiSchemaChunks[idx] = chunk;
        if (idx + 1 > _apiSchemaChunkCount) {
            _apiSchemaChunkCount = idx + 1;
        }
        address upExtern = address.makeAddrExtern(ManifestUpdatedEmit, bitCntAddress);
        emit ManifestUpdated{dest: upExtern}(address(this), idx);
    }

    /// @notice Remove the chunk at index `idx`. `_apiSchemaChunkCount` is
    ///         intentionally left unchanged — a hole in the middle is the
    ///         caller's responsibility to handle off-chain.
    function deleteApiSchemaChunk(uint32 idx) public onlyOwnerPubkey(_ownerPubkey) accept {
        ensureBalance();
        delete _apiSchemaChunks[idx];
        address upExtern = address.makeAddrExtern(ManifestUpdatedEmit, bitCntAddress);
        emit ManifestUpdated{dest: upExtern}(address(this), idx);
    }

    // ========================================================
    // Getters
    // ========================================================

    function getApiSchemaChunk(uint32 idx) external view returns (string) {
        return _apiSchemaChunks[idx];
    }

    function getApiSchemaChunkCount() external view returns (uint32) {
        return _apiSchemaChunkCount;
    }

    function getRootModel() external view returns (address) { return _rootModelAddress; }
    function getOwnerPubkey() external view returns (uint256) { return _ownerPubkey; }

    function getVersion() external pure returns (string, string) {
        return (version, "ManifestMetadata");
    }
}
