/*
 * Copyright (c) GOSH Technology Ltd. All rights reserved.
 * 
 * Acki Nacki and GOSH are either registered trademarks or trademarks of GOSH
 * 
 * Licensed under the ANNL. See License.txt in the project root for license information.
*/
pragma gosh-solidity >=0.76.1;

/// @title PMP Errors
/// @notice Common error codes for PMP contracts
abstract contract Errors {    
    /// @notice Message sender address mismatch
    uint16 constant ERR_INVALID_SENDER = 101;

    /// @notice Deposit value is too low
    uint16 constant ERR_LOW_VALUE = 102;

    /// @notice Event already resolved
    uint16 constant ERR_ALREADY_RESOLVED = 103;

    /// @notice Wallet already initialized / stake confirmed
    uint16 constant ERR_ALREADY_INITIALIZED = 107;

    /// @notice User already claimed winnings
    uint16 constant ERR_ALREADY_CLAIMED = 108;

    /// @notice Wallet or stake not yet initialized/confirmed
    uint16 constant ERR_NOT_INITIALIZED = 114;

    /// @notice User is not a winner in this event
    uint16 constant ERR_NOT_WINNER = 115;

    /// @notice Contract not approved by oracle
    uint16 constant ERR_NOT_APPROVED = 116;

    /// @notice Contract already approved by oracle
    uint16 constant ERR_ALREADY_APPROVED = 117;

    /// @notice Insufficient network fee
    uint16 constant ERR_INSUFFICIENT_NETWORK_FEE = 118;

    /// @notice Stake submission period has ended
    uint16 constant ERR_STAKE_PERIOD_ENDED = 120;

    /// @notice PrivateNote is currently busy with another operation
    uint16 constant ERR_NOTE_BUSY = 121;

    /// @notice Stake candidate amount not zero
    uint16 constant ERR_STAKE_NOT_APPROVED = 122;
    
    /// @notice Wrong PMP deadline for stake
    uint16 constant ERR_WRONG_DEADLINE = 123;

    /// @notice Stake submission period has not started
    uint16 constant ERR_STAKE_NOT_STARTED = 124;

    /// @notice Result submission period has not started
    uint16 constant ERR_RESULT_NOT_STARTED = 125;

    /// @notice Result submission period has ended
    uint16 constant ERR_RESULT_ENDED = 126; 
    
    /// @notice Invalid currency count
    uint16 constant ERR_INVALID_CURRENCY_COUNT = 127;

    /// @notice Zero token amount provided
    uint16 constant ERR_ZERO_TOKEN_AMOUNT = 128;

    /// @notice Invalid parameters provided
    uint16 constant ERR_INVALID_PARAMS = 129;

    /// @notice Invalid outcome ID provided
    uint16 constant ERR_INVALID_OUTCOME_ID = 130;

    /// @notice Outcomes not set for the event
    uint16 constant ERR_OUTCOMES_NOT_SET = 131;

    /// @notice Order already cancelled
    uint16 constant ERR_ALREADY_CANCELLED = 132;

    /// @notice Order not cancelled
    uint16 constant ERR_NOT_CANCELLED = 133;

    /// @notice Long array provided
    uint16 constant ERR_LONG_ARRAY = 134;

    /// @notice User already voted on proposal
    uint16 constant ERR_ALREADY_VOTED = 135;

    /// @notice Wrong hash for oracle members
    uint16 constant ERR_WRONG_HASH = 136;

    /// @notice Invalid zero-knowledge proof
    uint16 constant ERR_INVALID_ZKPROOF = 137;

    /// @notice Invalid token type
    uint16 constant ERR_INVALID_TOKEN_TYPE = 138;

    /// @notice Not all oracle events approved
    uint16 constant ERR_NOT_APPROVED_BY_ORACLE = 139;

    /// @notice Proposal not exists
    uint16 constant ERR_PROPOSAL_NOT_EXISTS = 140;

    /// @notice voucher nominal not allow
    uint16 constant ERR_NOT_ALLOWED = 141;

    /// @notice Cancel for unexist stake
    uint16 constant ERR_STAKE_NOT_EXISTS = 142;

    /// @notice Cannot generate coupon: user has debt
    uint16 constant ERR_HAS_DEBT = 143;

    /// @notice Cannot generate coupon: non-zero balance exists
    uint16 constant ERR_NON_ZERO_BALANCE = 144;

    /// @notice Coupon pool limit exceeded
    uint16 constant ERR_COUPON_POOL_LIMIT_EXCEEDED = 145;

    /// @notice No coupon available for staking
    uint16 constant ERR_NO_COUPON_AVAILABLE = 146;

    /// @notice Invalid bet type specified
    uint16 constant ERR_INVALID_BET_TYPE = 147;

    /// @notice Coupon already exists for this token type
    uint16 constant ERR_COUPON_ALREADY_EXISTS = 148;

    /// @notice Cannot perform operation: coupon is active
    uint16 constant ERR_COUPON_ACTIVE = 149;

    /// @notice Debt is non-zero for this token type
    uint16 constant ERR_DEBT_NON_ZERO = 150;

    /// @notice Invalid state for this operation
    uint16 constant ERR_INVALID_STATE = 151;

    /// @notice Deployer has not staked on all outcomes before full-set window
    uint16 constant ERR_DEPLOYER_NOT_COVERED = 152;

    /// @notice Base pools not frozen yet
    uint16 constant ERR_NOT_FROZEN = 153;

    /// @notice Base pools already frozen
    uint16 constant ERR_ALREADY_FROZEN = 154;

    /// @notice Merge would make PMP insolvent
    uint16 constant ERR_MERGE_SOLVENCY = 155;

    /// @notice Stake period has not ended yet
    uint16 constant ERR_NOT_STAKEEND = 156;

    /// @notice Order book: invalid epoch
    uint16 constant ERR_INVALID_EPOCH = 157;

    /// @notice Order book: order not found
    uint16 constant ERR_ORDER_NOT_FOUND = 158;

    /// @notice Order book: epoch not ended yet
    uint16 constant ERR_EPOCH_NOT_ENDED = 159;

    /// @notice Order book: amount below minimum order size
    uint16 constant ERR_ORDER_TOO_SMALL = 160;

    /// @notice Order book: batch size exceeds MAX_BATCH_SIZE
    uint16 constant ERR_BATCH_TOO_LARGE = 161;

    /// @notice Order book: empty batch
    uint16 constant ERR_EMPTY_BATCH = 162;

    /// @notice Order book: amount is not a multiple of lot size
    uint16 constant ERR_AMOUNT_NOT_LOT_MULTIPLE = 163;

    /// @notice Order book: price is not a multiple of tick size
    uint16 constant ERR_PRICE_NOT_TICK_MULTIPLE = 164;
}