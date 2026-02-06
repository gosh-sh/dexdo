/*
 * Copyright (c) GOSH Technology Ltd. All rights reserved.
 * 
 * Acki Nacki and GOSH are either registered trademarks or trademarks of GOSH
 * 
 * Licensed under the ANNL. See License.txt in the project root for license information.
*/
pragma gosh-solidity >=0.76.1;

import "./errors.sol";

/// @title Modifiers and Constants Contract
/// @notice Provides common modifiers, constants and configuration for PMP contracts
abstract contract Modifiers is Errors {
    // Constants for external event emission
    uint constant bitCntAddress = 256;
    
    // RootPN events
    uint128 constant ROOTPN_PRIVATE_NOTE_DEPLOYED = 1;
    uint128 constant ROOTPN_NULLIFIER_DEPLOYED = 2;
    uint128 constant ROOTPN_ORACLE_DEPLOYED = 3;

    // Oracle events
    uint128 constant ORACLE_DEPLOYED = 4;
    uint128 constant ORACLE_EVENT_LIST_DEPLOYED = 5;
    uint128 constant ORACLE_EVENT_CONFIRMED = 6;
    
    // PrivateNote events
    uint128 constant PRIVATENOTE_PMP_DEPLOYED = 11;
    uint128 constant PRIVATENOTE_OWNER_CHANGED = 12;
    uint128 constant PRIVATENOTE_STAKE_CONFIRMED = 13;
    uint128 constant PRIVATENOTE_CLAIM_ACCEPTED = 14;
    uint128 constant PRIVATENOTE_STAKE_CANCELLED = 15;
    uint128 constant PRIVATENOTE_FULLSET_STAKE_CONFIRMED = 16;
    uint128 constant PRIVATENOTE_FULLSET_STAKE_CANCELLED = 17;
    
    // PMP events
    uint128 constant PMP_STAKE_ACCEPTED = 18;
    uint128 constant PMP_APPROVED_BY_ORACLE = 19;
    uint128 constant PMP_RESOLVED = 20;
    uint128 constant PMP_CLAIM_PROCESSED = 21;
    uint128 constant PMP_NETWORK_FEE_BURNED = 22;
    uint128 constant PMP_STAKE_DEADLINE_SET = 23;
    uint128 constant PMP_SET_TIMINGS = 24;
    uint128 constant PMP_NUM_OUTCOMES_SET = 25;
    uint128 constant PMP_EVENT_CANCELLED = 26;
    uint128 constant PMP_ORACLE_CONFIRMED = 27;
    uint128 constant PMP_ALL_ORACLES_CONFIRMED = 28;
    uint128 constant PMP_INITIALIZED = 29;
    uint128 constant PMP_PROPOSAL_CREATED = 30;
    uint128 constant PMP_PROPOSAL_EXECUTED = 31;
    uint128 constant PMP_CANCELLED_BY_ORACLE = 32;

    // OracleList events
    uint128 constant ORACLE_EVENT_ADDED = 33;
    uint128 constant ORACLE_EVENT_PUBLISHED = 34;

    // Vault events
    uint128 constant VAULT_VAUCHER_GENERATED = 35;
    // Root Oracle event
    uint128 constant ROOTORACLE_ORACLE_DEPLOYED = 36;

    // Function type identifiers for OracleUnion proposals
    uint32 constant FUNCTION_TYPE_SET_STAKE_DEADLINE = 1;
    uint32 constant FUNCTION_TYPE_SET_RESOLVE = 2;
    uint32 constant FUNCTION_TYPE_CANCEL_EVENT = 3;
    
    /// @notice Minimum native balance required for contract operation
    uint64 constant MIN_BALANCE = 100 vmshell;

    /// @notice Minimum allowed deposit value
    uint64 constant MIN_VALUE = 10_000_000; // 0.01 NACKL

    /// @notice Currency ID used for PMP pools (staking tokens)
    uint32 constant CURRENCIES_ID = 1;

    /// @notice Currency ID used for shell tokens 
    uint32 constant CURRENCIES_ID_SHELL = 2;

    /// @notice Currency ID used for shell tokens (network fees)
    uint32 constant CURRENCIES_ID_SHELL_FEE = 300;

    /// @notice Fixed network fee to burn on approval
    uint64 constant NETWORK_FEE_AMOUNT = 1_000_000_000; // 1 shell tokens

    /// @notice Address of RootPN contract
    address constant ROOT_PN_ADDRESS = address.makeAddrStd(0, 0x1010101010101010101010101010101010101010101010101010101010101010);

    /// @notice Address of RootOracle contract
    address constant ROOT_ORACLE_ADDRESS = address.makeAddrStd(0, 0x1515151515151515151515151515151515151515151515151515151515151515);

    /// @notice Voting threshold for OracleUnion decisions
    uint32 THRESHOLD = 6600; // 66% = 6600

    /// @notice Full percentage constant
    uint128 constant FULL_PERCENT = 10000; // 100% = 10000

    /// @notice Full set percentage for Stake distribution
    uint128 constant FULL_SET_PERCENT = 1000; 

    /// @notice Fee percentage for staking operations
    uint128 constant FEE_PERCENT = 1; // 0.01% = 1

    /// @notice Allowed nominal for vault
    uint128[] constant ALLOWED_NOMINALS = [
        uint128(1000000000),
        uint128(100000000000),
        uint128(1000000000000),
        uint128(10000000000000)
    ];

    /// @notice Modifier for owner authorization using public key
    /// @param rootpubkey Expected owner public key
    modifier onlyOwnerPubkey(uint256 rootpubkey) {
        require(msg.pubkey() == rootpubkey, ERR_INVALID_SENDER);
        _;
    }

    /// @notice Modifier for accepting incoming messages
    modifier accept() {
        tvm.accept();
        _;
    }

    /// @notice Modifier for sender address validation
    /// @param sender Expected sender address
    modifier senderIs(address sender) {
        require(msg.sender == sender, ERR_INVALID_SENDER);
        _;
    }

    /// @notice Stake information per PMP
    struct StakeInfo {
        uint128[] amount;             // confirmed stakes per outcome
        uint128 candidate_amount;   // pending stakes per outcome
        uint32 candidate_outcome;  // pending stake outcome
        uint32 token_type;
        uint256 oracle_list_hash;
    }

    /// @notice Proposal structure
    struct Proposal {
        uint32 function_type;
        TvmCell data;
        uint64 deadline;
        uint32 voteCount;
        mapping(uint256 => bool) votes;
    }

    /// @notice Event information structure
    struct EventInfo {
        string event_name;
        uint128 oracle_fee;
        uint64 deadline;
        string describe;
        mapping(uint32 => string) outcomeNames;
        uint128 count;
        optional(uint256) trustAddr;
    }
}