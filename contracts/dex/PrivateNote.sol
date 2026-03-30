pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PMP.sol";
import "./RootPN.sol";
import "./OrderBook.sol";
import "./libraries/DexLib.sol";

/// @notice Wallet that can deploy and interact with PMP contracts
contract PrivateNote is Modifiers {

    /// @notice Contract semantic version.
    string constant version = "1.0.2";

    /// @notice Unique deposit identifier hash (static)
    uint256 public static _deposit_identifier_hash;

    /// @notice Ephemeral public key for authorization
    uint256 _ephemeral_pubkey;

    /// @notice PMP code for deployment
    TvmCell _pmpCode;

    /// @notice PrivateNote code for deployment
    TvmCell _PrivateNoteCode;

    /// @notice Oracle code hash + depth for address computation
    uint256 _oracleCodeHash;
    uint16  _oracleCodeDepth;

    /// @notice OracleEventList code hash + depth for address computation
    uint256 _oracleEventListCodeHash;
    uint16  _oracleEventListCodeDepth;

    /// @notice OrderBook code for deployment
    TvmCell _orderBookCode;

    /// @notice Mapping from stake hash to stake info
    mapping(uint256 => StakeInfo) public _stakes;

    /// @notice Address of PMP currently being interacted with
    optional(address) _busy;

    /// @notice Hash of the last stake operation
    uint256 _lastHash;

    /// @notice Amount locked in an outbound transfer (cleared on accept/bounce)
    uint128 _pendingTransferAmount;

    /// @notice Token type of the pending outbound transfer
    uint32 _pendingTransferTokenType;

    /// @notice Fee reserve locked at order placement, saved per orderId in onOrderPlaced
    uint128 _pendingFeeReserve;

    /// @notice Per-order fee reserves (orderId => remaining fee reserve)
    mapping(uint128 => uint128) _orderFeeReserves;

    /// @notice Current token balance
    mapping(uint32 => uint128) _balance;

    /// @notice User debt per token type (created after coupon win)
    /// @dev Debt is created when user wins with a coupon
    uint128 _debt;

    /// @notice Token type for the debt
    uint32 _debt_token_type;
    
    /// @notice Indicates that tokens were withdrawn before
    bool _has_withdrawn;

    /// @notice Indicates that a P2P transfer was sent from this wallet
    bool _has_transferred;

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

    /// @notice Emitted when an order is confirmed placed on OrderBook
    /// @param orderBook OrderBook address
    /// @param orderId Assigned order ID
    event OrderPlacedConfirmed(address orderBook, uint128 orderId);

    /// @notice Emitted when an order fill callback is received from OrderBook
    /// @param orderBook OrderBook address
    /// @param filledAmount Amount of outcome tokens filled
    /// @param isBuy Whether this was a buy fill
    event OrderFilledConfirmed(address orderBook, uint128 filledAmount, bool isBuy);

    /// @notice Emitted when an outbound transfer is initiated
    event TransferInitiated(address dest, uint32 token_type, uint128 amount);

    /// @notice Emitted when an inbound transfer is received and credited
    event TransferReceived(address from, uint32 token_type, uint128 amount);

    /// @notice PrivateNote constructor
    /// @param value Initial token balance
    /// @param ephemeral_pubkey Ephemeral public key for authorization
    /// @param token_type Type of token
    /// @param pmpCode PMP contract code used for deterministic PMP derivation.
    /// @param orderBookCode OrderBook contract code used for deterministic OB derivation.
    /// @param oracleCodeHash Oracle contract code hash.
    /// @param oracleCodeDepth Oracle contract code depth.
    /// @param oracleEventListCodeHash OracleEventList contract code hash.
    /// @param oracleEventListCodeDepth OracleEventList contract code depth.
    constructor(uint128 value, uint256 ephemeral_pubkey, uint32 token_type, TvmCell pmpCode, TvmCell orderBookCode,
                uint256 oracleCodeHash, uint16 oracleCodeDepth, uint256 oracleEventListCodeHash, uint16 oracleEventListCodeDepth) {
        tvm.accept();
        require(msg.sender == ROOT_PN_ADDRESS, ERR_INVALID_SENDER);
        _pmpCode = pmpCode;
        _PrivateNoteCode = tvm.code();
        _oracleCodeHash = oracleCodeHash;
        _oracleCodeDepth = oracleCodeDepth;
        _oracleEventListCodeHash = oracleEventListCodeHash;
        _oracleEventListCodeDepth = oracleEventListCodeDepth;
        _orderBookCode = orderBookCode;
        _balance[token_type] = value;
        _ephemeral_pubkey = ephemeral_pubkey;
        RootPN(ROOT_PN_ADDRESS).privateNoteDeployed{value: 0.1 vmshell, flag: 1}(_deposit_identifier_hash, token_type, value);
    }

    /// @notice Changes the owner public key
    /// @param new_pubkey New public key
    function changeOwner(uint256 new_pubkey) public {
        require(msg.pubkey() == _ephemeral_pubkey, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();
        
        uint256 oldPubkey = _ephemeral_pubkey;
        _ephemeral_pubkey = new_pubkey;
        
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
    /// @param initialStakes Initial clean stakes for each outcome submitted with deployment.
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
    function deployPMP(uint256 event_id, uint128[] oracleFee, uint32 token_type, string[] names, uint128[] index, uint128[] initialStakes) public onlyOwnerPubkey(_ephemeral_pubkey) {
        uint256 length = names.length;
        require(!_has_withdrawn, ERR_INVALID_STATE);
        require(length == oracleFee.length, ERR_INVALID_PARAMS);
        require(length == index.length, ERR_INVALID_PARAMS);
        require(initialStakes.length > 0, ERR_INVALID_PARAMS);
        require(_debt == 0, ERR_DEBT_NON_ZERO);

        // Validate initial stakes and compute total
        uint128 initialTotal = 0;
        for (uint32 i = 0; i < initialStakes.length; i++) {
            require(initialStakes[i] >= minStakeValue(token_type), ERR_LOW_VALUE);
            initialTotal += initialStakes[i];
        }
        require(_balance[token_type] >= initialTotal, ERR_LOW_VALUE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);

        tvm.accept();
        ensureBalance();

        mapping(uint256 => bool) for_oracle_hash;

        // Include shell tokens for network fee
        uint128 sumFee = 0;
        address[] oracleEventLists;
        for (uint32 i = 0; i < length; i++) {
            sumFee += oracleFee[i];
            oracleEventLists.push(DexLib.computeOracleEventListAddressFromHash(
                _oracleEventListCodeHash, _oracleEventListCodeDepth,
                DexLib.computeOracleAddressFromHash(_oracleCodeHash, _oracleCodeDepth, names[i]),
                index[i]
            ));
            for_oracle_hash[tvm.hash(names[i])] = true;
        }
        mapping(uint32 => varuint32) data_cur;
        data_cur[CURRENCIES_ID_SHELL] = sumFee + NETWORK_FEE_AMOUNT;

        uint256 oracle_list_hash = tvm.hash(abi.encode(for_oracle_hash));
        TvmCell stateInit = DexLib.buildPMPStateInit(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type);
        address pmpAddress = DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type);

        // Deduct initial stakes from balance and set pending stake record
        _balance[token_type] -= initialTotal;
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        // candidate_amount = initialTotal signals pending initial stake
        _stakes[hash] = StakeInfo({
            amount: new uint128[](initialStakes.length),
            debt_amount: new uint128[](initialStakes.length),
            coupons_amount: new uint128[](initialStakes.length),
            candidate_amount: initialTotal,
            candidate_outcome: 0,
            candidate_bet_type: BET_TYPE_CLEAN,
            oracle_list_hash: oracle_list_hash,
            token_type: token_type
        });
        _busy = pmpAddress;
        _lastHash = hash;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress);
        emit PMPDeployed{dest: addrExtern}(event_id, token_type, pmpAddress, oracleEventLists, oracleFee);

        new PMP{
            stateInit: stateInit,
            value: 50 vmshell,
            currencies: data_cur,
            flag: 1,
            bounce: true
        }(_deposit_identifier_hash, token_type, oracleEventLists, oracleFee, initialStakes, _orderBookCode);
    }

    /// @notice Called by PMP after initial stakes (passed with deployPMP) are accepted
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param amounts Per-outcome initial stake amounts confirmed by PMP
    function onInitialStakesAccepted(uint256 event_id, uint256 oracle_list_hash, uint32 token_type, uint128[] amounts)
        public senderIs(_busy.get()) accept
    {
        ensureBalance();
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        StakeInfo stake = _stakes[hash];
        for (uint32 i = 0; i < amounts.length; i++) {
            stake.amount[i] = amounts[i];
        }
        stake.candidate_amount = 0;
        _stakes[hash] = stake;
        delete _busy;
    }

    /// @notice Called by PMP when initial stakes are invalid (outcome count mismatch → PMP cancelled)
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param refundTotal Total amount to refund to balance
    function onInitialStakesFailed(uint256 event_id, uint256 oracle_list_hash, uint32 token_type, uint128 refundTotal)
        public senderIs(_busy.get()) accept
    {
        ensureBalance();
        _balance[token_type] += refundTotal;
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
        delete _busy;
    }

    /// @notice Deletes a stake record
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    function deleteStake(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
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
    function cancelStake(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        require(!_has_withdrawn, ERR_INVALID_STATE);
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



    // ===== Split/Merge Functions =====

    /// @notice Splits collateral into proportional outcome tokens via PMP.
    /// @dev Deducts collateral from balance immediately. PMP computes
    ///      outcome token amounts and calls back onSplitAccepted.
    ///      Requires PMP pools to be frozen (after stakeEnd).
    ///
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param collateral Amount of collateral to split
    function splitFullSet(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128 collateral
    ) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(collateral > 0, ERR_LOW_VALUE);
        require(_balance[token_type] >= collateral, ERR_LOW_VALUE);
        require(_debt == 0, ERR_DEBT_NON_ZERO);

        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        _balance[token_type] -= collateral;

        address pmpAddress = DexLib.computePMPAddress(
            _PrivateNoteCode,
            _pmpCode,
            event_id,
            oracle_list_hash,
            token_type
        );

        _busy = pmpAddress;
        _lastHash = hash;

        StakeInfo stake = _stakes[hash];
        stake.candidate_amount = collateral;
        stake.candidate_bet_type = BET_TYPE_CLEAN;
        stake.oracle_list_hash = oracle_list_hash;
        stake.token_type = token_type;
        _stakes[hash] = stake;

        PMP(pmpAddress).splitFullSet{
            value: 0.1 vmshell,
            flag: 1
        }(collateral, _deposit_identifier_hash);
    }

    /// @notice Called by PMP after split is accepted.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param amounts Outcome token amounts received from split
    function onSplitAccepted(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128[] amounts
    ) public senderIs(_busy.get()) accept {
        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        StakeInfo stake = _stakes[hash];
        // Initialize arrays if this is the first split (no prior staking)
        if (stake.amount.length == 0) {
            stake.amount = new uint128[](amounts.length);
            stake.debt_amount = new uint128[](amounts.length);
            stake.coupons_amount = new uint128[](amounts.length);
        }
        for (uint32 i = 0; i < amounts.length; i++) {
            stake.amount[i] += amounts[i];
        }
        stake.candidate_amount = 0;
        _stakes[hash] = stake;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress);
        emit FullSetStakeConfirmed{dest: addrExtern}(_busy.get(), amounts);

        delete _busy;
    }

    /// @notice Merges proportional outcome tokens back into collateral via PMP.
    /// @dev Sends outcome token amounts to PMP for merge. PMP verifies
    ///      proportionality, checks solvency, and calls back onMergeAccepted.
    ///
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param amount Array of outcome token amounts to merge
    function mergeFullSet(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128[] amount
    ) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(amount.length > 0, ERR_INVALID_PARAMS);
        require(_debt == 0, ERR_DEBT_NON_ZERO);

        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);

        StakeInfo stake = _stakes[hash];
        require(amount.length == stake.amount.length, ERR_INVALID_PARAMS);

        // Verify PN has enough outcome tokens to merge
        uint128 total = 0;
        for (uint32 i = 0; i < amount.length; i++) {
            require(stake.amount[i] >= amount[i], ERR_LOW_VALUE);
            total += amount[i];
        }

        stake.candidate_amount = total;
        stake.candidate_bet_type = BET_TYPE_MERGE;
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

        PMP(pmpAddress).mergeFullSet{
            value: 0.1 vmshell,
            flag: 1
        }(amount, _deposit_identifier_hash);
    }

    /// @notice Called by PMP after merge is accepted.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param collateral Collateral amount returned from merge
    /// @param amounts Per-outcome token amounts that were merged
    function onMergeAccepted(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128 collateral,
        uint128[] amounts
    ) public senderIs(_busy.get()) accept {
        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        StakeInfo stake = _stakes[hash];
        bool isEmpty = true;
        for (uint32 i = 0; i < amounts.length; i++) {
            stake.amount[i] -= amounts[i];
            if ((stake.amount[i] > 0) || (stake.debt_amount[i] > 0) || (stake.coupons_amount[i] > 0)) {
                isEmpty = false;
            }
        }
        stake.candidate_amount = 0;

        if (isEmpty) {
            delete _stakes[hash];
        } else {
            _stakes[hash] = stake;
        }

        _balance[token_type] += collateral;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_MERGE_CONFIRMED, bitCntAddress);
        emit FullSetStakeCancelled{dest: addrExtern}(_busy.get(), collateral);

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
        public onlyOwnerPubkey(_ephemeral_pubkey) accept
    {
        require(!_has_withdrawn, ERR_INVALID_STATE);
        require(amount >= minStakeValue(token_type), ERR_LOW_VALUE);
        if (use_coupon) {
            require(_coupons_value >= amount, ERR_LOW_VALUE);
            require(token_type == _coupons_token_type, ERR_INVALID_TOKEN_TYPE);
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
            stake.candidate_bet_type = bet_type;
            _stakes[hash] = stake;
        } else {
            _stakes[hash] = StakeInfo({
                amount: new uint128[](0),
                debt_amount: new uint128[](0),
                coupons_amount: new uint128[](0),
                candidate_amount: amount,
                candidate_outcome: outcome,
                candidate_bet_type: bet_type,
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

    /// @notice Called by PMP after stake is accepted
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of Oracles
    /// @param token_type Token type
    /// @param outcome_count Number of outcomes configured in PMP for this event.
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

        // Initialize all arrays on first stake (regardless of type)
        // This ensures claim() always has properly sized arrays to pass to PMP
        if (stake.amount.length == 0) {
            stake.amount = new uint128[](outcome_count);
            stake.debt_amount = new uint128[](outcome_count);
            stake.coupons_amount = new uint128[](outcome_count);
        }

        if (bet_type == BET_TYPE_COUPON) {
            stake.coupons_amount[stake.candidate_outcome] += amount;
        } else if (bet_type == BET_TYPE_DEBT) {
            stake.debt_amount[stake.candidate_outcome] += amount;
        } else {
            stake.amount[stake.candidate_outcome] += amount;
        }
        stake.candidate_amount = 0;
        _stakes[hash] = stake;
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress);
        emit StakeConfirmed{dest: addrExtern}(_busy.get(), stake.candidate_outcome, amount, bet_type);
        delete _busy;
    }

    /// @notice Claims winnings from PMP
    /// @dev Sends claim request to PMP. Wallet enters busy state
    ///      until `onClaimAccepted` callback is received.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    function claim(uint256 event_id, uint256 oracle_list_hash, uint32 token_type) public onlyOwnerPubkey(_ephemeral_pubkey) {
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        StakeInfo stake = _stakes[hash];

        require(!_has_withdrawn, ERR_INVALID_STATE);
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
    /// @param token_type Token type for payout and debt accounting.
    /// @param outcome Optional outcome (if resolved)
    /// @param payoutClean Payout amount for clean bets
    /// @param payout_debt Debt payout amount
    /// @param payout_coupon Coupon payout amount
    /// @param debtPaid Amount of debt repaid from this payout (formula 17)
    function onClaimAccepted(uint256 event_id, uint256 oracle_list_hash, uint32 token_type, optional(uint32) outcome, uint128 payoutClean, uint128 payout_debt, uint128 payout_coupon, uint128 debtPaid)
        public senderIs(_busy.get()) 
    {        
        tvm.accept();
        ensureBalance();
        
        if (!outcome.hasValue()) {
            delete _busy;
            return;
        } 
        
        _balance[token_type] += payoutClean + payout_debt + payout_coupon;

        // Formula 10: Increase debt from coupon profit
        if (payout_coupon > 0) {
            _debt += payout_coupon;
        }

        // Formula 18: Decrease debt by debtPaid
        if (_debt > debtPaid) {
            _debt -= debtPaid;
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

    /// @notice Accepts creator fee transfer from PMP and credits local token balance.
    /// @param fee Creator fee amount transferred from PMP.
    /// @param token_type Token type in which the fee is credited.
    /// @param event_id Event identifier of the source PMP.
    /// @param oracle_list_hash Oracle set hash of the source PMP.
    function acceptFee(uint128 fee, uint32 token_type, uint256 event_id, uint256 oracle_list_hash) public senderIs(DexLib.computePMPAddress(_PrivateNoteCode, _pmpCode, event_id, oracle_list_hash, token_type)) accept {
        ensureBalance();
        _balance[token_type] += fee;
    }

    /// @notice Returns fixed coupon nominal for a token type.
    /// @param token_type Token type used for coupon issuance.
    /// @return couponValue Coupon nominal value for the given token type (0 if unsupported).
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
    ///      - all token balances < minStakeValue (i.e. too small to stake)
    ///      - debt == 0
    ///      - No withdrawals from this wallet have been performed.
    ///      - No active stakes exist.
    ///      - No coupon currently exists.
    function generateCoupon(uint32 token_type) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        ensureBalance();
        require(_debt == 0, ERR_HAS_DEBT);
        require(!_has_withdrawn, ERR_INVALID_STATE);
        require(!_has_transferred, ERR_INVALID_STATE);
        require(_stakes.empty(), ERR_NOTE_BUSY);
        for ((uint32 tt, uint128 bal) : _balance) {
            require(bal < minStakeValue(tt), ERR_NON_ZERO_BALANCE);
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

    /// @notice Handles bounced messages from PMP contracts and remote PrivateNotes.
    /// @dev Distinguishes between a transfer bounce (_pendingTransferAmount > 0)
    ///      and a PMP operation bounce (candidate_amount in _stakes[_lastHash]).
    /// @param body Bounced message body (kept for ABI compatibility; not decoded).
    onBounce(TvmSlice body) external {
        tvm.accept();
        ensureBalance();
        body;
        if (!_busy.hasValue() || msg.sender != _busy.get()) {
            return;
        }

        // --- Transfer bounce: offerTransfer bounced back ---
        if (_pendingTransferAmount > 0) {
            _balance[_pendingTransferTokenType] += _pendingTransferAmount;
            _pendingTransferAmount = 0;
            delete _busy;
            return;
        }

        // --- PMP bounce: acceptStake / acceptFullSetStake / cancelStake etc. ---
        delete _busy;
        StakeInfo stake = _stakes[_lastHash];

        // Return funds to proper balance based on bet type
        if (stake.candidate_bet_type == BET_TYPE_COUPON) {
            _coupons_value += stake.candidate_amount;
        } else if (stake.candidate_bet_type == BET_TYPE_OB_SELL) {
            // Sell order bounce: return outcome tokens to stake
            stake.amount[stake.candidate_outcome] += stake.candidate_amount;
        } else if (stake.candidate_bet_type == BET_TYPE_MERGE) {
            // Merge bounce: nothing was deducted from _balance, outcome tokens
            // are still in stake.amount — just clear candidate, no restoration needed.
        } else {
            _balance[stake.token_type] += stake.candidate_amount;
        }

        stake.candidate_amount = 0;
        _stakes[_lastHash] = stake;

        // Delete stake record if no confirmed amounts remain
        // (covers both: regular stake with no history, and full-set
        //  stake that bounced on the very first attempt)
        bool allEmpty = true;
        for (uint32 i = 0; i < stake.amount.length; i++) {
            if (stake.amount[i] > 0) { allEmpty = false; break; }
        }
        for (uint32 i = 0; i < stake.debt_amount.length; i++) {
            if (stake.debt_amount[i] > 0) { allEmpty = false; break; }
        }
        for (uint32 i = 0; i < stake.coupons_amount.length; i++) {
            if (stake.coupons_amount[i] > 0) { allEmpty = false; break; }
        }
        if (allEmpty) {
            delete _stakes[_lastHash];
        }
    }

    // ── PrivateNote-to-PrivateNote transfer ──────────────────────────────────────

    /// @notice Initiates a token transfer to another PrivateNote.
    /// @dev Destination address is derived deterministically from dest_deposit_hash.
    ///      Deducts amount from balance immediately and sets _busy = dest.
    ///      The receiving PrivateNote credits the tokens automatically.
    ///      If offerTransfer bounces, onBounce restores the balance.
    /// @param dest_deposit_hash _deposit_identifier_hash of the destination PrivateNote
    /// @param token_type Token type to transfer
    /// @param amount Amount to transfer (must be >= minStakeValue)
    function initTransfer(uint256 dest_deposit_hash, uint32 token_type, uint128 amount)
        public onlyOwnerPubkey(_ephemeral_pubkey) accept
    {
        require(!_has_withdrawn, ERR_INVALID_STATE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(_stakes.empty(), ERR_NOTE_BUSY);
        require(amount >= minStakeValue(token_type), ERR_LOW_VALUE);
        require(_balance[token_type] >= amount, ERR_LOW_VALUE);
        require(_debt == 0, ERR_DEBT_NON_ZERO);
        require(_coupons_value == 0, ERR_COUPON_ACTIVE);
        require(dest_deposit_hash != _deposit_identifier_hash, ERR_INVALID_PARAMS);
        ensureBalance();

        address dest = DexLib.computePrivateNoteAddress(_PrivateNoteCode, dest_deposit_hash);

        _balance[token_type] -= amount;
        _pendingTransferAmount = amount;
        _pendingTransferTokenType = token_type;
        _has_transferred = true;
        _busy = dest;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress);
        emit TransferInitiated{dest: addrExtern}(dest, token_type, amount);

        PrivateNote(dest).offerTransfer{value: 0.1 vmshell, flag: 1, bounce: true}(
            token_type, amount, _deposit_identifier_hash
        );
    }

    /// @notice Called by a remote PrivateNote to deliver a transfer.
    /// @dev Verifies the sender is a valid PrivateNote via deterministic address derivation.
    ///      Credits tokens immediately and notifies the sender.
    /// @param token_type Token type being transferred
    /// @param amount Amount being transferred
    /// @param sender_deposit_hash _deposit_identifier_hash of the sending PrivateNote
    function offerTransfer(uint32 token_type, uint128 amount, uint256 sender_deposit_hash) public accept {
        require(!_has_withdrawn, ERR_INVALID_STATE);
        require(
            msg.sender == DexLib.computePrivateNoteAddress(_PrivateNoteCode, sender_deposit_hash),
            ERR_INVALID_SENDER
        );
        ensureBalance();
        _balance[token_type] += amount;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress);
        emit TransferReceived{dest: addrExtern}(msg.sender, token_type, amount);

        PrivateNote(msg.sender).onTransferAccepted{value: 0.1 vmshell, flag: 1, bounce: false}();
    }

    /// @notice Called by the receiving PrivateNote after crediting the transfer.
    /// @dev Clears busy state. Sent with bounce: false — if it fails to arrive,
    ///      use clearTransferBusy() as a recovery hatch.
    function onTransferAccepted() public senderIs(_busy.get()) accept {
        ensureBalance();
        _pendingTransferAmount = 0;
        delete _busy;
    }

    /// @notice Recovery hatch: owner can force-clear a stuck transfer state.
    /// @dev Only callable when a pending transfer exists (_pendingTransferAmount > 0).
    ///      Does NOT restore balance — tokens are already at the destination.
    ///      Use only after verifying off-chain that the receiver credited the tokens.
    function clearTransferBusy() public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        require(_pendingTransferAmount > 0, ERR_INVALID_STATE);
        ensureBalance();
        _pendingTransferAmount = 0;
        delete _busy;
    }

    // ── Coupon management ─────────────────────────────────────────────────────────

    /// @notice Discards the current coupon without using it.
    /// @dev Allowed only when coupon exists and no active stakes are pending.
    ///      The debt created at coupon issuance remains and must be repaid normally.
    function discardCoupon() public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        require(_coupons_value > 0, ERR_NO_COUPON_AVAILABLE);
        ensureBalance();
        _coupons_value = 0;
        _coupons_token_type = 0;
    }

    /// @notice Withdraws tokens to a specified wallet
    /// @param flags Transfer flags
    /// @param dest_wallet_addr Destination wallet address
    /// @param token_type Token type to withdraw.
    function withdrawTokens(uint8 flags, address dest_wallet_addr, uint32 token_type) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(_stakes.empty(), ERR_NOTE_BUSY);
        require(_debt == 0, ERR_DEBT_NON_ZERO);
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



    // ===== Order Book Functions =====

    /// @notice Places a limit order on the order book for a specific outcome.
    /// @dev For sell orders: locks outcome tokens (reduces stake.amount[outcomeId]).
    ///      For buy orders: locks collateral from balance.
    ///      Sets _busy until onOrderPlaced callback.
    ///
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param outcomeId Outcome to trade
    /// @param isBuy True for buy, false for sell
    /// @param priceBps Limit price in basis points (ignored for market orders)
    /// @param amount Amount of outcome tokens to trade
    /// @param flags Order flags: IOC=0x01, FOK=0x02, MARKET=0x04
    /// @param minAmount Minimum fill amount (0 = no minimum)
    /// @param epochId Epoch identifier used by dark order book matching.
    function placeOrder(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint32 outcomeId,
        bool isBuy,
        uint256 priceBps,
        uint128 amount,
        uint8 flags,
        uint128 minAmount,
        uint64 epochId
    ) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        require(!_has_withdrawn, ERR_INVALID_STATE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(amount > 0, ERR_LOW_VALUE);
        require(_debt == 0, ERR_DEBT_NON_ZERO);

        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        if (isBuy) {
            // Buy: lock collateral + max fee reserve from balance
            // Market orders lock full amount; limit orders lock amount * priceBps / FULL_PERCENT
            uint128 cost;
            if (flags & 0x04 != 0) {
                cost = amount; // market buy: lock full collateral
            } else {
                cost = uint128((uint256(amount) * uint256(priceBps)) / uint256(FULL_PERCENT));
            }
            // Reserve max possible fee (taker rate) upfront to guarantee fee coverage
            uint128 maxFee = uint128(
                (uint256(cost) * uint256(TAKER_FEE_RATE)) / uint256(FEE_DENOMINATOR)
            );
            require(_balance[token_type] >= cost + maxFee, ERR_LOW_VALUE);
            _balance[token_type] -= (cost + maxFee);
            // Track locked collateral + fee reserve so onBounce can restore it
            _pendingTransferAmount = cost + maxFee;
            _pendingTransferTokenType = token_type;
            _pendingFeeReserve = maxFee;
        } else {
            // Sell: lock outcome tokens
            require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);
            StakeInfo stake = _stakes[hash];
            require(outcomeId < uint32(stake.amount.length), ERR_INVALID_OUTCOME_ID);
            require(stake.amount[outcomeId] >= amount, ERR_LOW_VALUE);
            stake.amount[outcomeId] -= amount;
            // Track locked tokens so onBounce can restore them if execute() bounces
            stake.candidate_amount = amount;
            stake.candidate_outcome = outcomeId;
            stake.candidate_bet_type = BET_TYPE_OB_SELL;
            _stakes[hash] = stake;
        }

        address obAddress = DexLib.computeOrderBookAddress(
            _PrivateNoteCode,
            _orderBookCode,
            event_id,
            oracle_list_hash,
            token_type
        );

        _busy = obAddress;
        _lastHash = hash;

        OrderBook(obAddress).execute{
            value: 0.1 vmshell,
            flag: 1
        }(1, outcomeId, isBuy, priceBps, amount, 0, _deposit_identifier_hash, flags, minAmount, epochId);
    }

    /// @notice Called by OrderBook after order is placed.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param orderId Assigned order ID
    function onOrderPlaced(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128 orderId
    ) public senderIs(_busy.get()) accept {
        ensureBalance();

        // Clear buy bounce-protection field and save fee reserve per orderId
        _pendingTransferAmount = 0;
        if (_pendingFeeReserve > 0) {
            _orderFeeReserves[orderId] = _pendingFeeReserve;
            _pendingFeeReserve = 0;
        }

        // Clear sell bounce-protection field
        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);
        if (_stakes.exists(hash)) {
            StakeInfo stake = _stakes[hash];
            if (stake.candidate_amount > 0) {
                stake.candidate_amount = 0;
                _stakes[hash] = stake;
            }
        }

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress);
        emit OrderPlacedConfirmed{dest: addrExtern}(msg.sender, orderId);

        delete _busy;
    }

    /// @notice Cancels an existing order on the order book.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param orderId Order ID to cancel
    function cancelOrder(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128 orderId
    ) public onlyOwnerPubkey(_ephemeral_pubkey) accept {
        require(!_busy.hasValue(), ERR_NOTE_BUSY);

        ensureBalance();

        address obAddress = DexLib.computeOrderBookAddress(
            _PrivateNoteCode,
            _orderBookCode,
            event_id,
            oracle_list_hash,
            token_type
        );

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        _lastHash = tvm.hash(data);
        _busy = obAddress;

        OrderBook(obAddress).execute{
            value: 0.1 vmshell,
            flag: 1
        }(2, 0, false, 0, 0, orderId, _deposit_identifier_hash, 0, 0, 0);
    }

    /// @notice Called by OrderBook after order is cancelled. Returns locked tokens.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param orderId Cancelled order ID
    /// @param outcomeId Outcome of the cancelled order
    /// @param isBuy Whether it was a buy order
    /// @param amount Amount that was locked
    function onOrderCancelled(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint128 orderId,
        uint32 outcomeId,
        bool isBuy,
        uint128 amount
    ) public accept {
        // Verify sender is the correct OrderBook (same pattern as onOrderFilled)
        address expectedOb = DexLib.computeOrderBookAddress(
            _PrivateNoteCode,
            _orderBookCode,
            event_id,
            oracle_list_hash,
            token_type
        );
        require(msg.sender == expectedOb, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();
        orderId; // suppress unused warning

        if (isBuy) {
            // Return collateral + fee reserve
            uint128 feeReserve = _orderFeeReserves[orderId];
            _balance[token_type] += amount + feeReserve;
            delete _orderFeeReserves[orderId];
        } else {
            // Return outcome tokens
            TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
            uint256 hash = tvm.hash(data);
            if (_stakes.exists(hash)) {
                StakeInfo stake = _stakes[hash];
                stake.amount[outcomeId] += amount;
                _stakes[hash] = stake;
            }
        }

        // Clear _busy only if it still points to this OrderBook (explicit cancelOrder flow).
        // For IOC/FOK auto-cancels, _busy was already cleared by onOrderPlaced.
        if (_busy.hasValue() && _busy.get() == msg.sender) {
            delete _busy;
        }
    }

    /// @notice Called by OrderBook when an order is filled during epoch settlement.
    /// @param event_id PMP event ID
    /// @param oracle_list_hash Hash of oracle configuration
    /// @param token_type Token type
    /// @param outcomeId Outcome that was traded
    /// @param filledAmount Amount of outcome tokens filled
    /// @param clearingPrice Clearing price in basis points
    /// @param isBuy Whether this was a buy fill
    /// @param refundAmount Collateral refund for buy orders (overpaid above clearing price)
    /// @param feeAmount Trading fee (maker or taker) calculated by OrderBook
    function onOrderFilled(
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32 token_type,
        uint32 outcomeId,
        uint128 filledAmount,
        uint256 clearingPrice,
        bool isBuy,
        uint128 refundAmount,
        uint128 feeAmount,
        uint128 orderId
    ) public accept {
        // Verify sender is the OrderBook for this event
        address expectedOb = DexLib.computeOrderBookAddress(
            _PrivateNoteCode,
            _orderBookCode,
            event_id,
            oracle_list_hash,
            token_type
        );
        require(msg.sender == expectedOb, ERR_INVALID_SENDER);

        ensureBalance();

        TvmCell data = abi.encode(event_id, oracle_list_hash, token_type);
        uint256 hash = tvm.hash(data);

        if (isBuy) {
            // Bought outcome tokens: add to stakes.
            // If stake record was deleted (user called deleteStake while order was resting),
            // the fill is accepted but outcome tokens are burned — the user explicitly
            // abandoned this event, so tokens remain in the PMP pool.
            if (_stakes.exists(hash)) {
                StakeInfo stake = _stakes[hash];
                stake.amount[outcomeId] += filledAmount;
                _stakes[hash] = stake;
            }
            // Fee is covered by per-order fee reserve (locked at placeOrder).
            // Reserve may be in _pendingFeeReserve (immediate fill before onOrderPlaced)
            // or in _orderFeeReserves[orderId] (resting order fill).
            uint128 reserve = _orderFeeReserves[orderId];
            if (reserve == 0 && _pendingFeeReserve > 0) {
                reserve = _pendingFeeReserve;
                _pendingFeeReserve = 0;
            }
            uint128 feeRefund = reserve >= feeAmount ? reserve - feeAmount : 0;
            _balance[token_type] += refundAmount + feeRefund;
            if (_orderFeeReserves.exists(orderId)) {
                delete _orderFeeReserves[orderId];
            }
        } else {
            // Sold outcome tokens: receive collateral minus fee
            uint128 proceeds = uint128(
                (uint256(filledAmount) * uint256(clearingPrice)) / uint256(FULL_PERCENT)
            );
            if (proceeds > feeAmount) {
                _balance[token_type] += (proceeds - feeAmount);
            }
        }

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress);
        emit OrderFilledConfirmed{dest: addrExtern}(msg.sender, filledAmount, isBuy);
    }

    /// @notice Helper to return empty optional for event emission
    function _resolvedOutcomeNone() private pure returns (optional(uint32)) {
        optional(uint32) none;
        return none;
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
    /// @return ephemeralPubkey Ephemeral public key
    /// @return balance Current token balance
    /// @return pmpCodeHash Hash of PMP code
    /// @return privateNoteCodeHash Hash of PrivateNote code
    /// @return busyAddress Current busy PMP address (if any)
    function getDetails() external view returns (
        uint256 depositIdentifierHash,
        uint256 ephemeralPubkey,
        mapping(uint32 => uint128) balance,
        uint256 pmpCodeHash,
        uint256 privateNoteCodeHash,
        optional(address) busyAddress,
        uint128 couponsValue,
        bool hasWithdrawn
    ) {
        return (
            _deposit_identifier_hash,
            _ephemeral_pubkey,
            _balance,
            tvm.hash(_pmpCode),
            tvm.hash(_PrivateNoteCode),
            _busy,
            _coupons_value,
            _has_withdrawn
        );
    }

    /// @notice Returns contract name
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "PrivateNote");
    }
}
