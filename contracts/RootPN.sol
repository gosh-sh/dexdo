pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PrivateNote.sol";
import "./Nullifier.sol";
import "./Oracle.sol";
import "./libraries/DexLib.sol";

/// @notice Root contract responsible for deploying PrivateNote contracts
contract RootPN is Modifiers {

    string constant version = "1.0.0";

    /// @notice Stored code of PrivateNote contract
    TvmCell _PrivateNoteCode;

    /// @notice Stored code of PMP contract
    TvmCell _pmpCode;

    /// @notice Stored code of Nullifier contract
    TvmCell _nullifierCode;

    /// @notice Stored code of Oracle contract
    TvmCell _oracleCode;

    /// @notice Stored code of OracleEventList contract
    TvmCell _oracleEventListCode;

    /// @notice Root owner public key
    uint256 _ownerPubkey;

    /// @notice Mapping of deployed PrivateNote values
    mapping(uint32 => uint128) _deployedValues;

    /// @notice Emitted when a new voucher is generated
    /// @param sk_u_commit Commitment of the secret key
    /// @param vaucher_nominal Nominal value of the voucher
    /// @param token_type Type of token for the voucher
	event vaucherGenerated(uint256 sk_u_commit, uint vaucher_nominal, uint32 token_type);

    // Events
    event PrivateNoteDeployed(uint256 depositIdentifierHash, address noteAddress, uint128 initialBalance);
    event NullifierDeployed(address nullifierAddress, uint64 value);

    /// @notice Root constructor
    constructor() {
        tvm.accept();
    }

    /// @notice Ensures minimal native balance for root operations
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    function sendEccShellToPrivateNote(bytes proof, uint256 nullifier_hash, uint256 deposit_identifier_hash, uint64 value) public view accept {
        ensureBalance();

        bytes pub_inputs = hex"000000000000000000000000000000000000000000000000"; // 24 zero bytes
        bytes private_note_sum_bytes = bytes(bytes8(value));
        pub_inputs.append(private_note_sum_bytes);
        pub_inputs.append(hex"000000000000000000000000000000000000000000000000");
        bytes token_type_bytes = bytes(bytes8(uint64(CURRENCIES_ID_SHELL_FEE)));
        pub_inputs.append(token_type_bytes);
        bytes private_note_digest_bytes = bytes(bytes32(nullifier_hash));
        pub_inputs.append(private_note_digest_bytes);

        require(gosh.zkhalo2verify(pub_inputs, proof), ERR_INVALID_ZKPROOF);
        mapping(uint32 => varuint32) data_cur;
        data_cur[CURRENCIES_ID_SHELL] = value;
        TvmCell stateInit = abi.encodeStateInit({
            contr: Nullifier,
            varInit: {
                _nullifier_hash: nullifier_hash
            },
            code: _nullifierCode
        });

        TvmCell stateInitNote = DexLib.buildPrivateNoteInitData(_PrivateNoteCode, deposit_identifier_hash);
        address noteAddress = address.makeAddrStd(0, tvm.hash(stateInitNote));

        new Nullifier{
            stateInit: stateInit,
            value: 10 vmshell,
            flag: 1,
            currencies: data_cur
        }(noteAddress);

        address addrExtern = address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress);
        emit NullifierDeployed{dest: addrExtern}(noteAddress, value);
    }

    /// @notice Deploys a new PrivateNote contract
    /// @param zkproof Zero-knowledge proof used to validate the deposit public inputs
    /// @param deposit_identifier_hash Unique identifier hash for the deposit (used to derive the PrivateNote address)
    /// @param ethemeral_pubkey Ephemeral public key for authorizing the deployed PrivateNote
    /// @param value Initial token balance (encoded into the ZK public inputs)
    /// @param token_type Type of token for the PrivateNote (must match the supported token type) (1 for NACKL, etc.)
    function deployPrivateNote(bytes zkproof, uint256 deposit_identifier_hash, uint256 ethemeral_pubkey, uint64 value, uint32 token_type)
        public view accept
    {
        ensureBalance();

        bytes pub_inputs = hex"000000000000000000000000000000000000000000000000"; // 24 zero bytes
        bytes private_note_sum_bytes = bytes(bytes8(value));
        pub_inputs.append(private_note_sum_bytes);
        pub_inputs.append(hex"000000000000000000000000000000000000000000000000");
        bytes token_type_bytes = bytes(bytes8(uint64(token_type)));
        pub_inputs.append(token_type_bytes);
        bytes private_note_digest_bytes = bytes(bytes32(deposit_identifier_hash));
        pub_inputs.append(private_note_digest_bytes);

        require(token_type == CURRENCIES_ID, ERR_INVALID_TOKEN_TYPE);
        require(gosh.zkhalo2verify(pub_inputs, zkproof), ERR_INVALID_ZKPROOF);
        TvmCell stateInit = DexLib.buildPrivateNoteInitData(_PrivateNoteCode, deposit_identifier_hash);
        address noteAddress = address.makeAddrStd(0, tvm.hash(stateInit));

        new PrivateNote{
            stateInit: stateInit,
            value: 50 vmshell,
            flag: 1
        }(value, ethemeral_pubkey, token_type, _pmpCode, _oracleEventListCode, _oracleCode);
        address addrExtern = address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress);
        emit PrivateNoteDeployed{dest: addrExtern}(deposit_identifier_hash, noteAddress, value);
    }

    /// @notice Records deployment of a PrivateNote contract
    /// @param deposit_identifier_hash Unique identifier for the deposit
    /// @param token_type Type of token deployed 
    /// @param deployed_value Value of the deployed token
    function privateNoteDeployed(uint256 deposit_identifier_hash, uint32 token_type, uint128 deployed_value) public senderIs(address.makeAddrStd(0, tvm.hash(DexLib.buildPrivateNoteInitData(_PrivateNoteCode, deposit_identifier_hash)))) accept {
        _deployedValues[token_type] += deployed_value;
    }

    /// @notice Updates the contract code for RootPN
    /// @param newcode New contract code
    function updateCode(TvmCell newcode, TvmCell cell) public onlyOwnerPubkey(_ownerPubkey) accept {
        ensureBalance();
        tvm.setcode(newcode);
        tvm.setCurrentCode(newcode);
        onCodeUpgrade(cell);
    }

    /// @notice Handles root code upgrade
    /// @param cell Code upgrade data cell
    function onCodeUpgrade(TvmCell cell) private {
        tvm.accept();
        ensureBalance();
        tvm.resetStorage();
        (_pmpCode, _PrivateNoteCode, _nullifierCode, _oracleCode, _oracleEventListCode, _ownerPubkey) = abi.decode(cell, (TvmCell, TvmCell, TvmCell, TvmCell, TvmCell, uint256));
    }

    /// @notice Returns the salted PrivateNote contract code
    /// @return privateNoteCode The salted PrivateNote contract code as TvmCell
    /// @return privateNoteHash Hash of PrivateNote contract code
    function getPrivateNoteCode() external view returns(TvmCell privateNoteCode, uint256 privateNoteHash) {
        TvmCell saltPN = abi.encode(_PrivateNoteCode);
        TvmCell codePN = abi.setCodeSalt(_PrivateNoteCode, saltPN);
        return (codePN, tvm.hash(codePN));
    }

    /// @notice Returns the deterministic address of a PrivateNote by deposit identifier hash
    /// @param deposit_identifier_hash Unique identifier hash for the deposit
    /// @return privateNoteAddress Deterministic PrivateNote contract address
    function getPrivateNoteAddress(uint256 deposit_identifier_hash) external view returns(address privateNoteAddress) {
        return DexLib.computePrivateNoteAddress(_PrivateNoteCode, deposit_identifier_hash);
    }

    /// @notice Returns the deterministic address of a PMP for the given event and oracle set
    /// @param event_id Event identifier used by the PMP
    /// @param names List of oracle names used to build the oracle list hash
    /// @param token_type Token type used by the PMP
    /// @return pmpAddress Deterministic PMP contract address
    function getPMPAddress(uint256 event_id, string[] names, uint32 token_type) external view returns(address pmpAddress) {
        mapping(uint256 => bool) for_oracle_hash;
        uint256 length = names.length;

        for (uint32 i = 0; i < length; i++) {
            for_oracle_hash[tvm.hash(names[i])] = true;
        }
        uint256 oracle_list_hash = tvm.hash(abi.encode(for_oracle_hash));
        return DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type);
    }

    /// VAULT FUNCTIONS

    /// @notice Check is it Allowed
    function isAllowedNominal(uint128 nominal) private view returns (bool) {
        for (uint i = 0; i < ALLOWED_NOMINALS.length; i++) {
            if (ALLOWED_NOMINALS[i] == nominal) {
                return true;
            }
        }
        return false;
    }

    /// @notice Checks if the nominal is allowed for vault operations
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
    ) public senderIs(DexLib.computePrivateNoteAddress(_PrivateNoteCode, initial_data_hash)) accept {
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
        _deployedValues[token_type] -= withdrawed_value;
        
        // Transfer tokens to wallet
        wallet_addr.transfer(varuint16(withdrawed_value), bounce, flags, TvmCell(), cc);
    }

    /// @notice Returns all global variables
    /// @return pmpCodeHash Hash of PMP code
    /// @return privateNoteCodeHash Hash of PrivateNote code
    /// @return ownerPubkey Root owner public key
    /// @return balance Current contract balance
    function getDetails() external view returns (
        uint256 pmpCodeHash,
        uint256 privateNoteCodeHash,
        uint256 ownerPubkey,
        uint128 balance
    ) {
        return (
            tvm.hash(_pmpCode),
            tvm.hash(_PrivateNoteCode),
            _ownerPubkey,
            address(this).balance
        );
    }

    /// @notice Returns root version
    /// @return Contract name
    function getVersion() external pure returns (string, string) {
        return (version, "RootPN");
    }
}