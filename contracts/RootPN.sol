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

    /// @notice Contract semantic version.
    string constant version = "1.4.0";

    /// @notice Stored code of PrivateNote contract
    TvmCell _privateNoteCode;

    /// @notice Stored code of PMP contract
    TvmCell _pmpCode;

    /// @notice Stored code of Nullifier contract
    TvmCell _nullifierCode;

    /// @notice Stored code of Oracle contract
    TvmCell _oracleCode;

    /// @notice Stored code of OracleEventList contract
    TvmCell _oracleEventListCode;

    /// @notice Stored code of OrderBook contract
    TvmCell _orderBookCode;

    /// @notice Root owner public key
    uint256 _ownerPubkey;

    /// @notice Mapping of deployed PrivateNote values
    mapping(uint32 => uint128) _deployedValues;

    /// @notice Encode a uint64 into the bn254 Fr representation that halo2
    ///         emits for `voucherNominal` (32 LE bytes, padded with zeros,
    ///         then read as a big-endian uint256).
    function _u64ToFr(uint64 v) private pure returns (uint256) {
        uint64 swapped =
            ((v >> 56) & 0xff)
          | (((v >> 48) & 0xff) << 8)
          | (((v >> 40) & 0xff) << 16)
          | (((v >> 32) & 0xff) << 24)
          | (((v >> 24) & 0xff) << 32)
          | (((v >> 16) & 0xff) << 40)
          | (((v >>  8) & 0xff) << 48)
          | (( v        & 0xff) << 56);
        return uint256(swapped) << 192;
    }

    /// @notice Encode a uint32 into the bn254 Fr representation halo2 emits
    ///         for `tokenType`.
    function _u32ToFr(uint32 v) private pure returns (uint256) {
        uint32 swapped =
            ((v >> 24) & 0xff)
          | (((v >> 16) & 0xff) << 8)
          | (((v >>  8) & 0xff) << 16)
          | (( v        & 0xff) << 24);
        return uint256(swapped) << 224;
    }

    /// @notice BN254 Fr modulus (a.k.a. group order of the bn254 G1 group's
    ///         scalar field). All halo2 instances are Fr elements; raw
    ///         uint256 inputs that exceed this must be reduced.
    uint256 constant FR_MODULUS = 0x30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001;

    /// @notice Encode an arbitrary uint256 (e.g. ephemeralPubkey) into the
    ///         bn254 Fr representation halo2 emits — that is,
    ///         `(v mod p).to_bytes_le()` packed into a uint256 whose
    ///         big-endian 32-byte view IS the LE-bytes (so when later
    ///         shipped to gosh.zkhalo2verify via `bytes(bytes32(...))` the
    ///         on-wire bytes are exactly the LE Fr canonical form).
    ///
    ///         Without the mod-reduce step, a pubkey whose top 2 bits are
    ///         set (above Fr) gets reduced inside the prover but not
    ///         on-chain → instance mismatch and verification fails.
    function _u256ToFr(uint256 v) private pure returns (uint256) {
        uint256 reduced = v % FR_MODULUS;
        uint256 r = 0;
        // 32-byte byte-swap (BE → LE)
        for (uint8 i = 0; i < 32; i++) {
            r = (r << 8) | (reduced & 0xff);
            reduced >>= 8;
        }
        return r;
    }
    
    // Events

    /// @notice Emitted when a new voucher is generated.
    /// @param skUCommit Commitment of the secret key
    /// @param voucherNominal Nominal value of the voucher
    /// @param tokenType Type of token for the voucher
	event VoucherGenerated(uint256 skUCommit, uint voucherNominal, uint32 tokenType);

    /// @notice Emitted when a PrivateNote contract is successfully deployed and registered.
    /// @param depositIdentifierHash Deposit identifier hash
    /// @param noteAddress — Deployed PrivateNote address
    /// @param initialBalance — Initial token balance
    event PrivateNoteDeployed(uint256 depositIdentifierHash, address noteAddress, uint128 initialBalance);

    /// @notice Emitted when a Nullifier contract is deployed.
    /// @param nullifierAddress — Address associated with the deployment
    /// @param value — Value linked to the nullifier
    event NullifierDeployed(address nullifierAddress, uint64 value);

    /// @notice Emitted when tokens are withdrawn from a PrivateNote to a wallet.
    /// @param amount — Amount of tokens withdrawn
    /// @param noteAddress — PrivateNote the tokens were withdrawn from
    /// @param to — Destination wallet address
    /// @param dapp_id — DApp id passed through from the withdraw call
    event TokensWithdrawn(uint128 amount, address noteAddress, address to, uint256 dapp_id);

    /// @notice Root constructor
    constructor() {
        tvm.accept();
    }

    /// @notice Ensures minimal native balance for root operations
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    /// @notice Verifies a zero-knowledge proof together with a node-side
    ///         historical-hash check, then deploys a Nullifier contract
    ///         linked to a deterministic PrivateNote address.
    ///
    /// @dev Public inputs for zk verification are 4 × 32 bytes, in order:
    ///      0. `depositIdentifierHash` — Poseidon commitment.
    ///      1. `finalLayerHistoricalHashRoot` — dense chain root that
    ///         the node verifies against `GlobalHistoricalData[layerNumber]`.
    ///      2. `voucherNominalFr` — voucher nominal as Fr.
    ///      3. `tokenTypeFr` — token type as Fr.
    ///
    ///      The flow is:
    ///      1. `gosh.check_layer_hash` confirms the node still has this
    ///         historical root in the requested layer's window. If the
    ///         hash has aged out of the historical window, the caller
    ///         must rebuild the proof against an older layer (L+1) and
    ///         retry — this is the L1 → L2 fallback used by the live
    ///         test (generate_vouchers_with_live_event_proving.py).
    ///      2. `gosh.zkhalo2verify` validates the zk proof against the
    ///         4-field public-input vector built above.
    ///      3. On success, deploys the Nullifier and emits
    ///         `NullifierDeployed`.
    ///
    /// @param proof Zero-knowledge proof of ownership of the source SHELL_FEE
    ///        voucher (whose dih is `nullifierHash`).
    /// @param nullifierHash Source SHELL_FEE voucher's deposit identifier hash
    ///        (Poseidon commitment), used as the proof's instance 0 and as the
    ///        Nullifier contract's varInit. Replays of the same source voucher
    ///        produce the same Nullifier address, so the second `new` is a
    ///        no-op — natural one-shot semantics, no state tracking needed.
    /// @param depositIdentifierHash Destination PrivateNote's dih — derives
    ///        the recipient PN address. Independent of the source voucher.
    /// @param finalLayerHistoricalHashRoot Dense chain root (instance 1).
    /// @param voucherNominalFr Source voucher nominal as Fr (instance 2);
    ///        must encode the user-supplied `value`.
    /// @param tokenTypeFr Source voucher token type as Fr (instance 3); must
    ///        encode CURRENCIES_ID_SHELL_FEE (300) — only fee vouchers may
    ///        top up a PN's fee balance via this path.
    /// @param value Amount of SHELL ECC forwarded to the Nullifier and on to
    ///        the PrivateNote. Must equal the source voucher's nominal.
    /// @param layerNumber Historical layer being proved against (L1, L2, …).
    function sendEccShellToPrivateNote(
        bytes proof,
        uint256 nullifierHash,
        uint256 depositIdentifierHash,
        uint256 finalLayerHistoricalHashRoot,
        uint256 voucherNominalFr,
        uint256 tokenTypeFr,
        uint64 value,
        uint8 layerNumber,
        uint256 recipientEphemeralPubkey
    ) public view {
        tvm.accept();
        ensureBalance();
        // Frontrun defense: `depositIdentifierHash` (recipient) is NOT in the
        // zk proof's public inputs, so the proof alone doesn't bind the
        // destination. Without a signature gate, an attacker could replay a
        // pending user's proof with their own `depositIdentifierHash` and
        // reroute the SHELL-fee ECC into a PN they control. Require the caller
        // to sign the ext message with the destination PN's ephemeral key.
        // (This restricts third-party SHELL top-ups; use case is typically
        // self-funding, and the owner holds this key.)
        require(msg.pubkey() == recipientEphemeralPubkey, ERR_INVALID_SENDER);
        require(recipientEphemeralPubkey != 0, ERR_INVALID_PARAMS);

        require(
            gosh.check_layer_hash(finalLayerHistoricalHashRoot, layerNumber),
            ERR_INVALID_HISTORY_PROOF
        );

        // Bind `value` to the proof's nominal: an attacker cannot mint
        // arbitrary balance from a tiny voucher.
        require(_u64ToFr(value) == voucherNominalFr, ERR_INVALID_ZKPROOF);
        // Only SHELL_FEE vouchers may be spent here. A SHELL voucher (type 2)
        // would otherwise let an attacker route a regular voucher's nominal
        // into a PN they don't own.
        require(_u32ToFr(CURRENCIES_ID_SHELL_FEE) == tokenTypeFr, ERR_INVALID_ZKPROOF);

        // Instance 0 = nullifierHash (the source voucher's dih). The proof
        // attests ownership of the SHELL_FEE voucher whose dih is
        // nullifierHash. `depositIdentifierHash` (recipient) is NOT in
        // the proof — to prevent an attacker from replaying a pending tx
        // with a different recipient we rely on:
        //   a) instance 4 = recipientEphemeralPubkey: the voucher was
        //      generated with a commit to this exact pubkey (see
        //      generateVoucher). Attacker's swap of recipientEphemeralPubkey
        //      breaks zk verification.
        //   b) msg.pubkey() == recipientEphemeralPubkey gate above:
        //      belt-and-suspenders so a replay by an unrelated key also
        //      fails at signature check.
        bytes pubInputs;
        pubInputs.append(bytes(bytes32(nullifierHash)));
        pubInputs.append(bytes(bytes32(finalLayerHistoricalHashRoot)));
        pubInputs.append(bytes(bytes32(voucherNominalFr)));
        pubInputs.append(bytes(bytes32(tokenTypeFr)));
        pubInputs.append(bytes(bytes32(_u256ToFr(recipientEphemeralPubkey))));

        require(gosh.zkhalo2verify(pubInputs, proof), ERR_INVALID_ZKPROOF);

        mapping(uint32 => varuint32) dataCur;
        dataCur[CURRENCIES_ID_SHELL] = value;

        TvmCell stateInit = abi.encodeStateInit({
            contr: Nullifier,
            varInit: {
                _nullifierHash: nullifierHash
            },
            code: _nullifierCode
        });

        TvmCell stateInitNote = DexLib.buildPrivateNoteInitData(_privateNoteCode, depositIdentifierHash);
        address noteAddress = address.makeAddrStd(0, tvm.hash(stateInitNote));

        address nullifier = address.makeAddrStd(0, tvm.hash(stateInit));

        new Nullifier{
            stateInit: stateInit,
            value: 10 vmshell,
            flag: 1,
            currencies: dataCur
        }(noteAddress);

        address addrExtern = address.makeAddrExtern(ROOTPN_NULLIFIER_DEPLOYED, bitCntAddress);
        emit NullifierDeployed{dest: addrExtern}(nullifier, value);
    }

    /// @notice Deploys a new PrivateNote contract.
    ///         Same proof-verification flow as `sendEccShellToPrivateNote`:
    ///         the node-side `check_layer_hash` must succeed for the supplied
    ///         `finalLayerHistoricalHashRoot` at `layerNumber`, and the
    ///         halo2 proof must validate against the 4-field public-input
    ///         vector below.
    ///
    /// @param zkproof Zero-knowledge proof used to validate the deposit public inputs.
    /// @param depositIdentifierHash Poseidon commitment (instance 0), also
    ///        derives the PrivateNote address.
    /// @param finalLayerHistoricalHashRoot Dense chain root (instance 1),
    ///        checked against the node's `GlobalHistoricalData[layerNumber]`.
    /// @param voucherNominalFr Voucher nominal as Fr (instance 2).
    /// @param tokenTypeFr Token type as Fr (instance 3).
    /// @param ephemeralPubkey Ephemeral public key for authorizing the deployed PrivateNote.
    /// @param value Initial token balance.
    /// @param tokenType Token type used by the deployed PrivateNote.
    /// @param layerNumber Historical layer being proved against (L1, L2, …).
    function deployPrivateNote(
        bytes zkproof,
        uint256 depositIdentifierHash,
        uint256 finalLayerHistoricalHashRoot,
        uint256 voucherNominalFr,
        uint256 tokenTypeFr,
        uint256 ephemeralPubkey,
        uint64 value,
        uint32 tokenType,
        uint8 layerNumber
    ) public view accept {
        ensureBalance();

        require(
            gosh.check_layer_hash(finalLayerHistoricalHashRoot, layerNumber),
            ERR_INVALID_HISTORY_PROOF
        );

        // Bind user's `value` and `tokenType` to the proof's pubInputs so
        // a 100-shell voucher proof cannot be replayed as e.g. a 1M-NACKL
        // deposit (BUG #6 — money minting from a stale/cheap voucher).
        require(_u64ToFr(value) == voucherNominalFr, ERR_INVALID_ZKPROOF);
        require(_u32ToFr(tokenType) == tokenTypeFr, ERR_INVALID_ZKPROOF);
        // BUG #7 footgun: eph=0 means msg.pubkey()==0 passes onlyOwnerPubkey on
        // every PN method — any keyless tx can drain the PN. Reject up front.
        require(ephemeralPubkey != 0, ERR_INVALID_PARAMS);
        // BUG #9: SHELL_FEE (300) is the gas-only token used by
        // sendEccShellToPrivateNote. A PN whose main ledger holds SHELL_FEE
        // creates a phantom asset (RootPN doesn't custody type-300 ECC, only
        // type-2), so trading flows would silently break. Reject deployment
        // for the fee-only token.
        require(tokenType != CURRENCIES_ID_SHELL_FEE, ERR_INVALID_PARAMS);

        // Frontrun defense — bind ephemeralPubkey to the proof. The halo2
        // circuit emits ephemeralPubkey as instance 4; any mismatch between
        // the caller-supplied value and the one baked into the proof aborts
        // zkhalo2verify. Attacker cannot substitute their own pubkey without
        // re-running the prover against a secret they don't own.
        //
        // CIRCUIT/PROVER CONTRACT:
        //   pubInputs[0] = depositIdentifierHash
        //   pubInputs[1] = finalLayerHistoricalHashRoot
        //   pubInputs[2] = voucherNominalFr
        //   pubInputs[3] = tokenTypeFr
        //   pubInputs[4] = ephemeralPubkey (raw uint256 big-endian 32 bytes)
        // The halo2 prover MUST expose the same 5-field instance vector.
        bytes pubInputs;
        pubInputs.append(bytes(bytes32(depositIdentifierHash)));
        pubInputs.append(bytes(bytes32(finalLayerHistoricalHashRoot)));
        pubInputs.append(bytes(bytes32(voucherNominalFr)));
        pubInputs.append(bytes(bytes32(tokenTypeFr)));
        pubInputs.append(bytes(bytes32(_u256ToFr(ephemeralPubkey))));

        require(gosh.zkhalo2verify(pubInputs, zkproof), ERR_INVALID_ZKPROOF);
        TvmCell stateInit = DexLib.buildPrivateNoteInitData(_privateNoteCode, depositIdentifierHash);

        new PrivateNote{
            stateInit: stateInit,
            value: 50 vmshell,
            flag: 1
        }(value, ephemeralPubkey, tokenType, _pmpCode, _orderBookCode,
          tvm.hash(_oracleCode), _oracleCode.depth(), tvm.hash(_oracleEventListCode), _oracleEventListCode.depth());
    }

    /// @notice Records deployment of a PrivateNote contract
    /// @param depositIdentifierHash Unique identifier for the deposit
    /// @param tokenType Type of token deployed 
    /// @param deployedValue Value of the deployed token
    function privateNoteDeployed(uint256 depositIdentifierHash, uint32 tokenType, uint128 deployedValue) public senderIs(address.makeAddrStd(0, tvm.hash(DexLib.buildPrivateNoteInitData(_privateNoteCode, depositIdentifierHash)))) accept {
        _deployedValues[tokenType] += deployedValue;
        address addrExtern = address.makeAddrExtern(ROOTPN_PRIVATE_NOTE_DEPLOYED, bitCntAddress);
        emit PrivateNoteDeployed{dest: addrExtern}(depositIdentifierHash, msg.sender, deployedValue);
    }

    /// @notice Updates the contract code for RootPN
    /// @param newcode New contract code
    /// @param cell Encoded persistent state used by `onCodeUpgrade`.
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
        (_pmpCode, _privateNoteCode, _nullifierCode, _oracleCode, _oracleEventListCode, _orderBookCode, _ownerPubkey) = abi.decode(cell, (TvmCell, TvmCell, TvmCell, TvmCell, TvmCell, TvmCell, uint256));
    }

    /// @notice Returns the salted PrivateNote contract code
    /// @return privateNoteCode The salted PrivateNote contract code as TvmCell
    /// @return privateNoteHash Hash of PrivateNote contract code
    function getPrivateNoteCode() external view returns(TvmCell privateNoteCode, uint256 privateNoteHash) {
        TvmCell saltPN = abi.encode(_privateNoteCode);
        TvmCell codePN = abi.setCodeSalt(_privateNoteCode, saltPN);
        return (codePN, tvm.hash(codePN));
    }

    /// @notice Returns the deterministic address of a PrivateNote by deposit identifier hash
    /// @param depositIdentifierHash Unique identifier hash for the deposit
    /// @return privateNoteAddress Deterministic PrivateNote contract address
    function getPrivateNoteAddress(uint256 depositIdentifierHash) external view returns(address privateNoteAddress) {
        return DexLib.computePrivateNoteAddress(_privateNoteCode, depositIdentifierHash);
    }

    /// @notice Returns the deterministic address of a PMP for the given event and oracle set
    /// @param eventId Event identifier used by the PMP
    /// @param names List of oracle names used to build the oracle list hash
    /// @param tokenType Token type used by the PMP
    /// @return pmpAddress Deterministic PMP contract address
    function getPMPAddress(uint256 eventId, string[] names, uint32 tokenType) external view returns(address pmpAddress) {
        mapping(uint256 => bool) forOracleHash;
        uint256 length = names.length;

        for (uint32 i = 0; i < length; i++) {
            forOracleHash[tvm.hash(names[i])] = true;
        }
        uint256 oracleListHash = tvm.hash(abi.encode(forOracleHash));
        return DexLib.computePMPAddress(_privateNoteCode, _pmpCode, eventId, oracleListHash, tokenType);
    }

    /// VAULT FUNCTIONS

    /// @notice Check is it Allowed
    /// @param nominal Voucher nominal in token base units.
    /// @param tokenType Token type to validate.
    /// @return isAllowed True if nominal matches one of `ALLOWED_NOMINALS` for the token decimals.
    function isAllowedNominal(uint128 nominal, uint32 tokenType) private view returns (bool) {
        uint128 decimals = tokenDecimals(tokenType);
        for (uint i = 0; i < ALLOWED_NOMINALS.length; i++) {
            if (ALLOWED_NOMINALS[i] * decimals == nominal) {
                return true;
            }
        }
        return false;
    }

    /// @notice Checks if the nominal is allowed for vault operations
    /// @param skUCommit Commitment of user secret key used in off-chain flows.
    /// @param isFee Whether incoming shell tokens must be treated as fee token type.
    function generateVoucher(uint256 skUCommit, bool isFee) public view internalMsg {
		require(msg.currencies.keys().length == 1, 300);
		uint32 tokenType = msg.currencies.keys()[0];
		require(msg.currencies[tokenType] > 0, 303);
		tvm.accept();
        ensureBalance();

		uint voucherNominal = msg.currencies[tokenType];
        require(isAllowedNominal(uint128(voucherNominal), tokenType), ERR_NOT_ALLOWED);

        if ((tokenType == CURRENCIES_ID_SHELL) && (isFee)) {
            tokenType = CURRENCIES_ID_SHELL_FEE;
        }

        address addrExtern = address.makeAddrExtern(VAULT_voucher_GENERATED, bitCntAddress);
		emit VoucherGenerated{dest: addrExtern}(skUCommit, voucherNominal, tokenType);
	}

    /// @notice Withdraws tokens to a specified wallet.
    /// @dev The inner `transfer` flag is hard-coded to 1. Allowing a
    ///      caller-supplied flag here would let any PN owner pass TVM flags
    ///      128 (CARRY_ALL_BALANCE) or 32 (DELETE_IF_EMPTY) — draining or
    ///      destroying this RootPN. Since RootPN custodies every PN's ECC,
    ///      that single tx would steal or brick the entire DEX.
    /// @param withdrawedValue Amount to withdraw
    /// @param tokenType Type of token to withdraw
    /// @param walletAddr Destination wallet address
    /// @param initialDataHash Initial data hash for verification
    function withdrawTokens(
        uint128 withdrawedValue,
        uint32 tokenType,
        address walletAddr,
        uint256 initialDataHash,
        uint256 dapp_id
    ) public senderIs(DexLib.computePrivateNoteAddress(_privateNoteCode, initialDataHash)) accept {
        // `dapp_id` drives no logic here — only surfaced in the TokensWithdrawn
        // event below; kept for forward compatibility / off-chain context.
        ensureBalance();
        // Verify sufficient balance — both real currency reserves and the
        // bookkeeping pool must cover the withdrawal. Either gap → revert
        // the PN-side debit via `revertWithdraw` instead of throwing here
        // (a plain require would leave PN's `_balance` permanently low).
        if (address(this).currencies[tokenType] < withdrawedValue
            || _deployedValues[tokenType] < withdrawedValue) {
            PrivateNote(msg.sender).revertWithdraw{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}(
                tokenType,
                withdrawedValue
            );
            return;
        }

        // Prepare currency transfer data
        mapping(uint32 => varuint32) cc;
        cc[tokenType] = varuint32(withdrawedValue);
        _deployedValues[tokenType] -= withdrawedValue;

        // Transfer tokens to wallet — flag is intentionally hard-coded to 1.
        walletAddr.transfer(varuint16(withdrawedValue), false, 1, TvmCell(), cc);

        // External event: how much was withdrawn, from which PrivateNote
        // (msg.sender) and to which destination wallet.
        address addrExtern = address.makeAddrExtern(ROOTPN_TOKENS_WITHDRAWN, bitCntAddress);
        emit TokensWithdrawn{dest: addrExtern}(withdrawedValue, msg.sender, walletAddr, dapp_id);
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
            tvm.hash(_privateNoteCode),
            _ownerPubkey,
            address(this).balance
        );
    }

    /// @notice Returns root version
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "RootPN");
    }
}
