pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PMP.sol";
import "./RootPN.sol";
import "./libraries/DexLib.sol";

/// @notice Wallet that can deploy and interact with PMP contracts
contract PrivateNote is Modifiers {

    string constant version = "1.0.0";

    /// @notice Unique deposit identifier hash (static)
    uint256 public static _deposit_identifier_hash;

    /// @notice Ephemeral public key for authorization
    uint256 _ethemeral_pubkey;

    /// @notice PMP code for deployment
    TvmCell _pmpCode;

    /// @notice PrivateNote code for deployment
    TvmCell _PrivateNoteCode;

    /// @notice Oracle code 
    TvmCell _oracleCode;

    /// @notice OracleEventList code
    TvmCell _oracleEventList;

    /// @notice Mapping from stake hash to stake info
    mapping(uint256 => StakeInfo) public _stakes;

    /// @notice Address of PMP currently being interacted with
    optional(address) _busy;

    /// @notice Hash of the last stake operation
    uint256 _lastHash;

    /// @notice Current token balance
    mapping(uint32 => uint128) _balance;

    /// @notice User debt per token type (created after coupon win)
    /// @dev Debt is created when user wins with a coupon
    uint128 _debt;

    /// @notice Token type for the debt
    uint32 _debt_token_type;
    
    /// @notice Indicates that tokens were withdrawn before
    bool _has_withdrawn;

    /// @notice Available coupons
    uint128 _coupons_value;

    /// @notice Coupon type (used for tracking which token type the coupon is for)
    uint32 _coupons_token_type;

    // Events

    /// @notice Emitted when owner pubkey is changed
    /// @param oldPubkey Previous ephemeral public key
    /// @param newPubkey New ephemeral public key
    event OwnerChanged(uint256 oldPubkey, uint256 newPubkey);

    /// @notice Emitted when a single-outcome stake is confirmed by PMP
    /// @param stakeController PMP address (stake controller)
    /// @param outcome Outcome index used in stake
    /// @param amount Confirmed stake amount
    /// @param bet_type 0 - clean bet, 1 - debt bet, 2 - coupon bet
    event StakeConfirmed(address stakeController, uint32 outcome, uint128 amount, uint8 bet_type);

    /// @notice Emitted when a single-outcome stake is cancelled and funds returned
    /// @param stakeController PMP address
    /// @param value Returned token value to balance
    event StakeCancelled(address stakeController, uint128 value);

    /// @notice Emitted when a full-set stake is confirmed by PMP
    /// @param stakeController PMP address
    /// @param amount Confirmed stake amounts per outcome
    event FullSetStakeConfirmed(address stakeController, uint128[] amount);

    /// @notice Emitted when a full-set stake is cancelled and funds returned
    /// @param stakeController PMP address
    /// @param value Total returned token value to balance
    event FullSetStakeCancelled(address stakeController, uint128 value);

    /// @notice Emitted when claim is accepted by PMP
    /// @param stakeController PMP address
    /// @param outcome Optional outcome (set when market resolved)
    /// @param payout Total payout (including debt payout)
    event ClaimAccepted(address stakeController, optional(uint32) outcome, uint128 payout);

    /// @notice Emitted when a PMP is about to be deployed by this wallet
    /// @param event_id Event identifier
    /// @param token_type Token type used by PMP
    /// @param pmpAddress Deterministic PMP address
    /// @param oracleEventLists OracleEventList addresses used by PMP
    /// @param oracleFee Fee per oracle (same order as oracleEventLists)
    event PMPDeployed(uint256 event_id, uint32 token_type, address pmpAddress, address[] oracleEventLists, uint128[] oracleFee);

    /// @notice PrivateNote constructor
    /// @param value Initial token balance
    /// @param ethemeral_pubkey Ephemeral public key for authorization
    /// @param token_type Type of token
    constructor(uint128 value, uint256 ethemeral_pubkey, uint32 token_type, TvmCell pmpCode, TvmCell oracleEventList, TvmCell oracleCode) {
        tvm.accept();
        require(msg.sender == ROOT_PN_ADDRESS, ERR_INVALID_SENDER);
        _pmpCode = pmpCode;
        _PrivateNoteCode = tvm.code();
        _oracleCode = oracleCode;
        _oracleEventList = oracleEventList;
        _balance[token_type] = value;
        _ethemeral_pubkey = ethemeral_pubkey;
        RootPN(ROOT_PN_ADDRESS).privateNoteDeployed{value: 0.1 vmshell, flag: 1}(_deposit_identifier_hash, token_type, value);
    }

    /// @notice Changes the owner public key
    /// @param new_pubkey New public key
    function changeOwner(uint256 new_pubkey) public {
        require(msg.pubkey() == _ethemeral_pubkey, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();
        
        uint256 oldPubkey = _ethemeral_pubkey;
        _ethemeral_pubkey = new_pubkey;
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress);
        emit OwnerChanged{dest: addrExtern}(oldPubkey, new_pubkey);
    }

    /// @notice Ensures minimal native balance for operations
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    /// @notice Deploys a new PMP contract for a prediction market event.
    /// @dev This function deterministically computes oracle addresses and
    ///      OracleEventList contracts, prepares the required currency balances
    ///      for oracle and network fees, and deploys a new PMP instance.
    ///
    ///      Deployment flow:
    ///      1. Validate input array lengths.
    ///      2. Compute oracle addresses from `names`.
    ///      3. Compute OracleEventList addresses using oracle code and indexes.
    ///      4. Aggregate oracle fees and include network fee.
    ///      5. Build PMP StateInit and compute deterministic PMP address.
    ///      6. Emit `PMPDeployed` event.
    ///      7. Deploy PMP contract with required currencies.
    ///
    /// @param event_id Unique identifier of the PMP event.
    /// @param oracleFee Array of additional fees (in shell tokens) for each oracle.
    ///                  Must match the length of `names` and `index`.
    /// @param token_type Token type used by the PMP contract.
    /// @param names Array of oracle names used to compute oracle addresses.
    /// @param index Array of oracle indexes used to compute OracleEventList addresses.
    ///
    /// @custom:requirements
    /// - Caller must be authorized by the owner ephemeral public key.
    /// - `names.length == oracleFee.length == index.length`.
    ///
    /// @custom:effects
    /// - Emits `PMPDeployed` with the computed PMP address and oracle configuration.
    /// - Deploys a new PMP contract at a deterministic address.
    ///
    /// @custom:reverts
    /// - If input array lengths mismatch.
    /// - If caller is not the owner.
    function deployPMP(uint256 event_id, uint128[] oracleFee, uint32 token_type, string[] names, uint128[] index) public view onlyOwnerPubkey(_ethemeral_pubkey) {
        uint256 length = names.length;
        require(length == oracleFee.length, ERR_INVALID_PARAMS);
        require(length == index.length, ERR_INVALID_PARAMS);
        
        tvm.accept();
        ensureBalance();
        mapping(uint256 => bool) for_oracle_hash;
        
        // Include shell tokens for network fee
        uint128 sumFee = 0;
        address[] oracleEventLists;
        for (uint32 i = 0; i < length; i++) {
            sumFee += oracleFee[i];
            oracleEventLists.push(DexLib.computeOracleEventListAddress(_oracleEventList, DexLib.computeOracleAddress(_oracleCode, names[i]), index[i]));
            for_oracle_hash[tvm.hash(names[i])] = true;
        }
        mapping(uint32 => varuint32) data_cur;
        data_cur[CURRENCIES_ID_SHELL] = sumFee + NETWORK_FEE_AMOUNT;

        uint256 oracle_list_hash = tvm.hash(abi.encode(for_oracle_hash));
        TvmCell stateInit = DexLib.buildPMPStateInit(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type);
        address pmpAddress = DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type);
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress);
        emit PMPDeployed{dest: addrExtern}(event_id, token_type, pmpAddress, oracleEventLists, oracleFee);
        
        new PMP{
            stateInit: stateInit,
            value: 50 vmshell,
            currencies: data_cur,
            flag: 1,
            bounce: true
        }(_deposit_identifier_hash, token_type, oracleEventLists, oracleFee);
    }

    /// @notice Deletes a stake record
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    function deleteStake(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public onlyOwnerPubkey(_ethemeral_pubkey) accept {
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        ensureBalance();
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
    }

    /// @notice Cancels a stake on a PMP contract
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    function cancelStake(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public onlyOwnerPubkey(_ethemeral_pubkey) accept {
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        ensureBalance();
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);
        address pmpAddress = DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, _stakes[hash].oracle_list_hash, token_type);
        _busy = pmpAddress;
        _lastHash = hash;
        PMP(pmpAddress).cancelStake{
            value: 0.1 vmshell, 
            flag: 1
        }(_stakes[hash].amount, _stakes[hash].debt_amount, _stakes[hash].coupons_amount,_deposit_identifier_hash);
    }

    /// @notice Called by PMP after stake is cancelled
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    /// @param value Amount refunded
    /// @param coupon_value Coupon amount refunded
    function onStakeCancelled(uint256 event_id, uint256 oracle_list_hash, uint32 token_type, uint128 value, uint128 coupon_value) 
        public senderIs(_busy.get()) accept
    {
        ensureBalance();
        
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
        _balance[token_type] += value;
        _coupons_value += coupon_value;
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_STAKE_CANCELLED, bitCntAddress);
        emit StakeCancelled{dest: addrExtern}(_busy.get(), value);
        delete _busy;
    }

    /// @notice Withdraws a full set stake from PMP
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    /// @param amount Stake amounts per outcome
    function withdrawFullSet(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128[] amount
    ) public onlyOwnerPubkey(_ethemeral_pubkey) accept {
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(_debt == 0, ERR_DEPT_NON_ZERO);

        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);

        StakeInfo stake = _stakes[hash];
        require(amount.length == stake.amount.length, ERR_INVALID_PARAMS);

        _stakes[hash] = stake;

        address pmpAddress = DexLib.computePMPAddress(
            _PrivateNoteCode,
            _pmpCode,
            event_id,
            oracle_list_hash,
            token_type
        );

        _busy = pmpAddress;
        _lastHash = hash;

        PMP(pmpAddress).withdrawFullSet{
            value: 0.1 vmshell,
            flag: 1
        }(amount, _deposit_identifier_hash);
    }

    /// @notice Called by PMP after full set stake is cancelled
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    /// @param amount Amount refunded
    function onFullSetStakeCancelled(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128[] amount
    ) public senderIs(_busy.get()) accept {
        ensureBalance();
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        StakeInfo stake = _stakes[hash];
        bool isEmpty = true;
        uint128 total = 0;
        for (uint32 i = 0; i < amount.length; i++) {
            stake.amount[i] -= amount[i];
            if ((stake.amount[i] > 0) || (stake.debt_amount[i] > 0)  || (stake.coupons_amount[i] > 0)) {
                isEmpty = false;
            }
            total += amount[i];
        }
        _balance[token_type] += total;
        
        if (isEmpty) {
            delete _stakes[hash];
        } else {
            _stakes[hash] = stake;
        }
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_FULLSET_STAKE_CANCELLED, bitCntAddress);
        emit FullSetStakeCancelled{dest: addrExtern}(_busy.get(), total);

        delete _busy;
    }

    /// @notice Places a stake on a specific outcome in PMP.
    ///
    /// @dev Deducts funds from wallet balance or coupons immediately.
    ///      The stake is first stored as candidate and finalized only
    ///      after `onStakeAccepted` callback from PMP.
    ///
    /// @param event_id PMP event identifier.
    /// @param oracle_list_hash Hash of oracle configuration.
    /// @param token_type Token type.
    /// @param outcome Outcome index.
    /// @param amount Stake amount.
    /// @param use_coupon Whether to use coupon for this stake (if true, amount will be taken from available coupons instead of balance)
    ///
    /// Requirements:
    /// - Wallet must not be busy.
    /// - Amount must be greater than zero.
    /// - Sufficient balance or coupons must exist.
    function setStake(uint256 event_id, uint256 oracle_list_hash, uint32 token_type, uint32 outcome, uint128 amount, bool use_coupon) 
        public onlyOwnerPubkey(_ethemeral_pubkey) accept
    {        
        require(amount > 0, ERR_LOW_VALUE);
        if (use_coupon) {
            require(_coupons_value >= amount, ERR_LOW_VALUE);
        } else {
            require(_balance[token_type] >= amount, ERR_LOW_VALUE);
        }
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        ensureBalance();
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        address pmpAddress = DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type);
        uint8 bet_type = _debt > 0 && _debt_token_type == token_type ? 1 : 0;
        bet_type = use_coupon ? 2 : bet_type;

        if (_stakes.exists(hash)) {
            StakeInfo stake = _stakes[hash];
            require(stake.candidate_amount == 0, ERR_STAKE_NOT_APPROVED);
            stake.candidate_amount = amount;
            stake.candidate_outcome = outcome;
            _stakes[hash] = stake;
        } else {
            _stakes[hash] = StakeInfo({
                amount: new uint128[](0),
                debt_amount: new uint128[](0),
                coupons_amount: new uint128[](0),
                candidate_amount: amount,
                candidate_outcome: outcome,
                oracle_list_hash: oracle_list_hash,
                token_type: token_type
            });
        }
        
        _busy = pmpAddress;
        _lastHash = hash;
        if (use_coupon) {
            _coupons_value -= amount;
        } else {
            _balance[token_type] -= amount;
        }
        PMP(pmpAddress).acceptStake{
            value: 0.1 vmshell, 
            flag: 1
        }(outcome, amount, _deposit_identifier_hash, bet_type);
    }

    /// @notice Sets a full set stake on a PMP contract
    /// @dev Deducts total amount from balance immediately.
    ///      Stake is finalized after `onFullSetStakeAccepted`
    ///
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    /// @param amount Array of stake amounts per outcome
    function setFullSetStake(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128[] amount
    ) public onlyOwnerPubkey(_ethemeral_pubkey) accept {
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(amount.length > 0, ERR_INVALID_PARAMS);
        require(_debt == 0, ERR_DEPT_NON_ZERO);

        ensureBalance();

        uint128 total = 0;
        for (uint32 i = 0; i < amount.length; i++) {
            total += amount[i];
        }
        require(_balance[token_type] >= total, ERR_LOW_VALUE);

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        address pmpAddress = DexLib.computePMPAddress(
            _PrivateNoteCode,
            _pmpCode,
            event_id,
            oracle_list_hash,
            token_type
        );

        if (_stakes.exists(hash)) {
            StakeInfo stake = _stakes[hash];
            require(stake.candidate_amount == 0, ERR_STAKE_NOT_APPROVED);
            require(amount.length == stake.amount.length, ERR_INVALID_PARAMS);
            stake.candidate_amount = total;
            _stakes[hash] = stake;
        } else {
            _stakes[hash] = StakeInfo({
                amount: new uint128[](amount.length),
                debt_amount: new uint128[](0),
                coupons_amount: new uint128[](0),
                candidate_amount: total,
                candidate_outcome: 0,
                oracle_list_hash: oracle_list_hash,
                token_type: token_type
            });
        }

        _balance[token_type] -= total;
        _busy = pmpAddress;
        _lastHash = hash;

        PMP(pmpAddress).acceptFullSetStake{
            value: 0.1 vmshell,
            flag: 1
        }(amount, _deposit_identifier_hash);
    }


    /// @notice Called by PMP after stake is accepted
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    /// @param bet_type 0 - clean bet, 1 - debt bet, 2 - coupon bet
    function onStakeAccepted(uint256 event_id, uint256 oracle_list_hash, uint32 token_type, uint128 outcome_count, uint8 bet_type) 
        public senderIs(_busy.get()) 
    {
        tvm.accept();
        ensureBalance();
        
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        StakeInfo stake = _stakes[hash];
        uint128 amount = stake.candidate_amount;
        if (bet_type == 2) {
            if (stake.coupons_amount.length == 0) {
                stake.coupons_amount = new uint128[](outcome_count);
            }   
            stake.coupons_amount[stake.candidate_outcome] += amount;
        } 
        if (bet_type == 1) {
            if (stake.debt_amount.length == 0) {
                stake.debt_amount = new uint128[](outcome_count);
            }   
            stake.debt_amount[stake.candidate_outcome] += amount;
        }
        if (bet_type == 0) {
            if (stake.amount.length == 0) {
                stake.amount = new uint128[](outcome_count);
            }   
            stake.amount[stake.candidate_outcome] += amount;
        }
        stake.candidate_amount = 0;
        _stakes[hash] = stake;
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress);
        emit StakeConfirmed{dest: addrExtern}(_busy.get(), stake.candidate_outcome, amount, bet_type);
        delete _busy;
    }

    /// @notice Called by PMP after full set stake is accepted
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    /// @param amount Stake amounts per outcome
    function onFullSetStakeAccepted(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128[] amount
    ) public senderIs(_busy.get()) accept {
        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        StakeInfo stake = _stakes[hash];
        if (stake.amount.length == 0) {
            stake.amount = new uint128[](amount.length);
        }

        for (uint32 i = 0; i < amount.length; i++) {
            stake.amount[i] += amount[i];
        }

        stake.candidate_amount = 0;
        _stakes[hash] = stake;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_FULLSET_STAKE_CONFIRMED, bitCntAddress);
        emit FullSetStakeConfirmed{dest: addrExtern}(_busy.get(), amount);

        delete _busy;
    }

    /// @notice Claims winnings from PMP
    /// @dev Sends claim request to PMP. Wallet enters busy state
    ///      until `onClaimAccepted` callback is received.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    function claim(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public onlyOwnerPubkey(_ethemeral_pubkey) {
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        StakeInfo stake = _stakes[hash];
        
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(stake.candidate_amount == 0, ERR_STAKE_NOT_APPROVED);
        
        tvm.accept();
        ensureBalance();
        
        address pmpaddress = DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, stake.oracle_list_hash, stake.token_type);
        _busy = pmpaddress;
        _lastHash = hash;
        
        PMP(pmpaddress).claim{
            value: 0.1 vmshell, 
            flag: 1
        }(stake.amount, stake.debt_amount, stake.coupons_amount, _deposit_identifier_hash);
    }

    /// @notice Called by PMP after claim is processed
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param outcome Optional outcome (if resolved)
    /// @param payoutClean Payout amount for clean bets
    /// @param payout_debt Debt payout amount
    /// @param payout_coupon Coupon payout amount
    function onClaimAccepted(uint256 event_id, uint256 oracle_list_hash, uint32 token_type, optional(uint32) outcome, uint128 payoutClean, uint128 payout_debt, uint128 payout_coupon) 
        public senderIs(_busy.get()) 
    {        
        tvm.accept();
        ensureBalance();
        
        if (!outcome.hasValue()) {
            delete _busy;
            return;
        } 
        
        _balance[token_type] += payoutClean + payout_debt + payout_coupon;
        if (payout_coupon > 0) {
            _debt += payout_coupon;
        }
        uint128 diff = payout_debt * FULL_PERCENT / (FULL_PERCENT - DEBT_REDISTRIBUTION_PERCENT) * DEBT_REDISTRIBUTION_PERCENT; // Assuming 5% fee on debt payout
        if (_debt > diff) {
            _debt -= diff;
        } else {
            _debt = 0;
        }
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress);
        emit ClaimAccepted{dest: addrExtern}(_busy.get(), outcome, payoutClean + payout_debt + payout_coupon);
        
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
        delete _busy;
    }

    function acceptFee(uint128 fee, uint32 token_type, uint256 event_id, uint256 oracle_list_hash) public senderIs(DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type)) accept {
        ensureBalance();
        _balance[token_type] += fee;
    }

    function getCouponValue(uint32 token_type) private pure returns (uint128) {
        if (token_type == CURRENCIES_ID_SHELL) {
            return SHELL_COUPON_VALUE;
        } else if (token_type == CURRENCIES_ID) {
            return NACKL_COUPON_VALUE;
        } else if (token_type == CURRENCIES_ID_USDC) {
            return USDC_COUPON_VALUE;
        } else {
            return 0;
        }
    }

    /// @notice Generates a free coupon for the specified token type
    /// @param token_type Token type for which to generate coupon
    /// @dev Can only generate coupon when:
    ///      - all token balances == 0
    ///      - debt == 0
    ///      - No active stakes exist.
    ///      - No coupon currently exists.
    function generateCoupon(uint32 token_type) public onlyOwnerPubkey(_ethemeral_pubkey) accept {
        ensureBalance();
        require(_debt == 0, ERR_HAS_DEBT);
        require(!_has_withdrawn, ERR_INVALID_STATE);
        require(_stakes.empty(), ERR_NOTE_BUSY);
        for ((, uint128 bal) : _balance) {
            require(bal == 0, ERR_NON_ZERO_BALANCE);
        }        
        require(_coupons_value == 0, ERR_COUPON_ALREADY_EXISTS);
        _coupons_value = getCouponValue(token_type);
        _coupons_token_type = token_type;

        uint128 base_debt = _coupons_value * 5 / 100;
        _debt = base_debt;
        _debt_token_type = token_type;
    }

    /// @notice Receives funds to the wallet
    receive() external {
        tvm.accept();
        ensureBalance();
    }

    /// @notice Handles bounced messages from PMP contracts
    onBounce(TvmSlice body) external {
        tvm.accept();
        ensureBalance();
        body;
        if (_busy.hasValue() && msg.sender == _busy.get()) {
            delete _busy;
            StakeInfo stake = _stakes[_lastHash];
            _balance[stake.token_type] += stake.candidate_amount;
            stake.candidate_amount = 0;
            _stakes[_lastHash] = stake;
            if (stake.amount.length == 0) {
                delete _stakes[_lastHash];
            }
        }
    }

    /// @notice Withdraws tokens to a specified wallet
    /// @param flags Transfer flags 
    /// @param dest_wallet_addr Destination wallet address
    function withdrawTokens(uint8 flags, address dest_wallet_addr, uint32 token_type) public onlyOwnerPubkey(_ethemeral_pubkey) accept {
        ensureBalance();
        require(_stakes.empty(), ERR_NOTE_BUSY);
        require(_debt == 0, ERR_DEPT_NON_ZERO);
        RootPN(ROOT_PN_ADDRESS).withdrawTokens{value: 0.1 vmshell, bounce: false, flag: 1}(_balance[token_type], token_type, flags, dest_wallet_addr, _deposit_identifier_hash);
        _balance[token_type] = 0;
        _has_withdrawn = true;
	}

    /// @notice Reverts a withdraw operation (called by Vault)
    /// @param token_type Type of token
    /// @param value Amount to revert
    function revertWithdraw(uint32 token_type, uint128 value) public senderIs(ROOT_PN_ADDRESS) accept {
        ensureBalance();
        _balance[token_type] += value;
    }



    /// @notice Returns the salted PMP contract code
    /// @return pmpCode The salted PMP contract code as TvmCell
    /// @return pmpCodeHash Hash of PMP contract code
    function getPMPCode() external view returns(TvmCell pmpCode, uint256 pmpCodeHash) {
        TvmCell salt = abi.encode(_PrivateNoteCode);
        TvmCell code = abi.setCodeSalt(_pmpCode, salt);
        return (code, tvm.hash(code));
    }

    /// @notice Returns all global variables
    /// @return depositIdentifierHash Deposit identifier hash
    /// @return etherealPubkey Ephemeral public key
    /// @return balance Current token balance
    /// @return pmpCodeHash Hash of PMP code
    /// @return privateNoteCodeHash Hash of PrivateNote code
    /// @return busyAddress Current busy PMP address (if any)
    function getDetails() external view returns (
        uint256 depositIdentifierHash,
        uint256 etherealPubkey,
        mapping(uint32 => uint128) balance,
        uint256 pmpCodeHash,
        uint256 privateNoteCodeHash,
        optional(address) busyAddress
    ) {        
        return (
            _deposit_identifier_hash,
            _ethemeral_pubkey,
            _balance,
            tvm.hash(_pmpCode),
            tvm.hash(_PrivateNoteCode),
            _busy
        );
    }

    /// @notice Returns contract name
    /// @return Contract name
    function getVersion() external pure returns (string, string) {
        return (version, "PrivateNote");
    }
}