pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PrivateNote.sol";

/// @title Vault - Contract for generating and managing token vouchers
/// @notice Handles voucher generation, token withdrawals, and code upgrades
contract Vault is Modifiers {

    string constant version = "1.0.0";

    // Events
    /// @notice Emitted when a new voucher is generated
    /// @param sk_u_commit Commitment of the secret key
    /// @param vaucher_nominal Nominal value of the voucher
    /// @param token_type Type of token for the voucher
	event vaucherGenerated(uint256 sk_u_commit, uint vaucher_nominal, uint32 token_type);
    
    /// @notice Root contract address
    address _root;
    
    /// @notice PrivateNote contract code for address computation
    TvmCell _PrivateNoteCode;
    
    /// @notice Vault owner public key
    uint256 _ownerPubkey;

    /// @notice Mapping of withdrow values
    mapping(uint32 => uint128) _withdrowValues;

    /// @notice Vault constructor
    constructor() {
        tvm.accept();
    }

    /// @notice Ensures minimal native balance for operations
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    /// @notice Computes deterministic PrivateNote address for a deposit
    /// @param deposit_identifier_hash Deposit identifier hash
    /// @return PrivateNote contract address
    function computePrivateNoteAddress(uint256 deposit_identifier_hash) private view returns (address) {
        TvmCell stateInit = abi.encodeStateInit({
            contr: PrivateNote,
            varInit: {
                _deposit_identifier_hash: deposit_identifier_hash
            },
            code: _PrivateNoteCode
        });
        return address.makeAddrStd(0, tvm.hash(stateInit));
    }

    /// @notice Check is it Allowed
    function isAllowedNominal(uint128 nominal) private view returns (bool) {
        for (uint i = 0; i < ALLOWED_NOMINALS.length; i++) {
            if (ALLOWED_NOMINALS[i] == nominal) {
                return true;
            }
        }
        return false;
    }

    function generateVaucher(uint256 sk_u_commit, bool isFee) public view internalMsg {
		require(msg.currencies.keys().length == 1, 300);
		uint32 token_type = msg.currencies.keys()[0];
        if ((token_type == CURRENCIES_ID_SHELL) && (isFee)) {
            token_type = CURRENCIES_ID_SHELL_FEE;
        }
		require(msg.currencies[token_type] > 0, 303);
		tvm.accept();
        ensureBalance();

		uint vaucher_nominal = msg.currencies[token_type];
        require(isAllowedNominal(uint128(vaucher_nominal)), ERR_NOT_ALLOWED);

        address addrExtern = address.makeAddrExtern(VAULT_VAUCHER_GENERATED, bitCntAddress);
		emit vaucherGenerated{dest: addrExtern}(sk_u_commit, vaucher_nominal, token_type);
	}
    
    /// @notice Withdraws tokens to a specified wallet
    /// @param withdrawed_value Amount to withdraw
    /// @param token_type Type of token to withdraw
    /// @param flags Transfer flags
    /// @param wallet_addr Destination wallet address
    /// @param initial_data_hash Initial data hash for verification
    function withdrawTokens(
        uint128 withdrawed_value, 
        uint32 token_type, 
        uint8 flags, 
        address wallet_addr, 
        uint256 initial_data_hash
    ) public senderIs(computePrivateNoteAddress(initial_data_hash)) accept {
        ensureBalance();
        // Verify sufficient balance
        if (address(this).currencies[token_type] < withdrawed_value) {
            PrivateNote(msg.sender).revertWithdraw{value: 0.1 vmshell, flag: 1}(
                token_type,
                withdrawed_value
            );
            return;
        }
        
        // Prepare currency transfer data
        mapping(uint32 => varuint32) cc;
        cc[token_type] = varuint32(withdrawed_value);
        bool bounce = false;
        _withdrowValues[token_type] += withdrawed_value;
        
        // Transfer tokens to wallet
        wallet_addr.transfer(varuint16(withdrawed_value), bounce, flags, TvmCell(), cc);
    }

    /// @notice Updates the contract code
    /// @param newcode New contract code cell
    /// @param cell Upgrade data cell
    function updateCode(TvmCell newcode, TvmCell cell) public onlyOwnerPubkey(_ownerPubkey) accept {
        ensureBalance();
        tvm.setcode(newcode);
        tvm.setCurrentCode(newcode);
        onCodeUpgrade(cell);
    }

    /// @notice Handles contract code upgrade
    /// @param cell Code upgrade data cell containing new storage values
    function onCodeUpgrade(TvmCell cell) private {
        tvm.accept();
        ensureBalance();
        tvm.resetStorage();
        (_PrivateNoteCode, _ownerPubkey, _root) = abi.decode(cell, (TvmCell, uint256, address));
    }

    /// @notice Returns all global variables and contract details
    /// @return ownerPubkey Vault owner public key
    /// @return root Root contract address
    /// @return privateNoteCodeHash Hash of PrivateNote contract code
    function getDetails() external view returns (
        uint256 ownerPubkey,
        address root,
        uint256 privateNoteCodeHash
    ) {
        
        return (
            _ownerPubkey,
            _root,
            tvm.hash(_PrivateNoteCode)
        );
    }

    /// @notice Returns root version
    /// @return Contract name
    function getVersion() external pure returns (string, string) {
        return (version, "Vault");
    }
}