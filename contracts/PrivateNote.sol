pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./modifiers/replayprotection.sol";
import "./PMP.sol";
import "./RootPN.sol";
import "./OrderBook.sol";
import "./libraries/DexLib.sol";

/// @notice Wallet that can deploy and interact with PMP contracts
contract PrivateNote is Modifiers, ReplayProtection {

    /// @notice Contract semantic version.
    string constant version = "1.4.0";

    /// @notice Unique deposit identifier hash (static)
    uint256 public static _depositIdentifierHash;

    /// @notice Ephemeral public key for authorization
    uint256 _ephemeralPubkey;

    /// @notice PMP code for deployment
    TvmCell _pmpCode;

    /// @notice PrivateNote code for deployment
    TvmCell _privateNoteCode;

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

    /// @notice Single-order placeOrder buy: collateral + fee reserve locked
    ///         (mirrored into _lockedInOrders). Restored on bounce / cleared
    ///         on onOrderPlaced. Kept SEPARATE from _pendingTransferAmount so
    ///         the bounce handler can correctly distinguish a bounced
    ///         offerTransfer (no _lockedInOrders touch) from a bounced
    ///         placeOrder (must release _lockedInOrders). Before this split,
    ///         every offerTransfer bounce also mutated _lockedInOrders,
    ///         corrupting open-orders accounting and opening a double-spend
    ///         path for users with resting orders.
    uint128 _pendingPlaceBuyLock;
    uint32  _pendingPlaceBuyTokenType;

    /// @notice Per-OB-per-order fee reserves
    ///         (obAddress => orderId => remaining fee reserve).
    ///         Populated in onOrderPlaced from feeReserve provided by OrderBook.
    ///         Keyed by OB address because a PN can trade on several OBs
    ///         (one per PMP event) and each OB's `_nextOrderId` is
    ///         independent — two OBs may hand out the same orderId, which
    ///         would collide in a flat mapping.
    mapping(address => mapping(uint128 => uint128)) _orderFeeReserves;

    /// @notice Per-OB-per-order total lock
    ///         (obAddress => orderId => remaining collateral+fee lock).
    ///         Populated in onOrderPlaced from the authoritative `lock`
    ///         param that OrderBook computes as `cost + maxFee`. Each
    ///         `onOrderFilled` decrements it by the consumed amount; on
    ///         `isFinal` fill (or cancel) any residual — accumulated from
    ///         `floor(amount*price/10000)` per-fill truncation — is
    ///         refunded to `_balance` so `_lockedInOrders` drains exactly.
    ///         Buy orders only (sells lock outcome tokens, not collateral).
    ///         Keyed by OB address to avoid cross-OB orderId collisions.
    mapping(address => mapping(uint128 => uint128)) _orderLocks;

    /// @notice Collateral locked in open OrderBook orders, per token type.
    ///         Incremented on order placement, decremented on fill/cancel.
    mapping(uint32 => uint128) _lockedInOrders;

    /// @notice True while a batch order-book operation is in flight between the
    ///         outbound executePlaceBatch/executeCancelBatch/executeCancelAll call
    ///         and the terminating onBatchComplete callback. Prevents per-order
    ///         callbacks from clearing _busy prematurely.
    bool _pendingBatchActive;

    /// @notice Total balance locked on _balance for an in-flight place-batch
    ///         (cost + max taker fee, summed across all buy orders). Restored
    ///         by onBounce if executePlaceBatch bounces.
    uint128 _pendingBatchBuyLock;

    /// @notice Token type of the pending batch buy lock.
    uint32  _pendingBatchTokenType;

    /// @notice Stake hash associated with the pending batch (for sell-side restore).
    uint256 _pendingBatchStakeHash;

    /// @notice Single record per sell order in the pending batch (used for
    ///         bounce-protection restore). Combined into one array (vs two
    ///         parallel arrays) to halve cell writes per push.
    struct PendingBatchSell {
        uint32  outcomeId;
        uint128 amount;
    }
    PendingBatchSell[] _pendingBatchSells;

    /// @notice Current token balance
    mapping(uint32 => uint128) _balance;

    /// @notice User debt per token type (created after coupon win)
    /// @dev Debt is created when user wins with a coupon
    uint128 _debt;

    /// @notice Token type for the debt
    uint32 _debtTokenType;
    
    /// @notice Indicates that tokens were withdrawn before
    bool _hasWithdrawn;

    /// @notice Indicates that a P2P transfer was sent from this wallet
    bool _hasTransferred;

    /// @notice Available coupons
    uint128 _couponsValue;

    /// @notice Coupon type (used for tracking which token type the coupon is for)
    uint32 _couponsTokenType;

    // Events

    /// @notice Emitted when owner pubkey is changed
    /// @param oldPubkey Previous ephemeral public key
    /// @param newPubkey New ephemeral public key
    event OwnerChanged(uint256 oldPubkey, uint256 newPubkey);

    /// @notice Emitted when a single-outcome stake is confirmed by PMP
    /// @param stakeController PMP address (stake controller)
    /// @param outcome Outcome index used in stake
    /// @param amount Confirmed stake amount
    /// @param betType 0 - clean bet, 1 - debt bet, 2 - coupon bet
    event StakeConfirmed(address stakeController, uint32 outcome, uint128 amount, uint8 betType);

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
    /// @param eventId Event identifier
    /// @param tokenType Token type used by PMP
    /// @param pmpAddress Deterministic PMP address
    /// @param oracleEventLists OracleEventList addresses used by PMP
    /// @param oracleFee Fee per oracle (same order as oracleEventLists)
    event PMPDeployed(uint256 eventId, uint32 tokenType, address pmpAddress, address[] oracleEventLists, uint128[] oracleFee);

    /// @notice Emitted immediately when placeOrder is called (before OB confirmation)
    event OrderSubmitted(uint128 clientOrderId, uint32 outcomeId, bool isBuy, uint256 price, uint128 amount, uint8 flags, uint256 eventId, uint32 tokenType);

    /// @notice Emitted when an order is confirmed placed on OrderBook
    /// @param orderBook OrderBook address
    /// @param orderId Assigned order ID
    event OrderPlacedConfirmed(address orderBook, uint128 orderId);

    /// @notice Emitted when an order fill callback is received from OrderBook
    event OrderFilledConfirmed(address orderBook, uint128 orderId, uint32 outcomeId, uint128 filledAmount, uint256 clearingPrice, bool isBuy, uint128 feeAmount, bool isFinal);

    /// @notice Emitted when an order cancel callback is received from OrderBook
    event OrderCancelledConfirmed(address orderBook, uint128 orderId, uint32 outcomeId, bool isBuy, uint128 returnAmount);

    /// @notice Emitted when an outbound transfer is initiated
    event TransferInitiated(address dest, uint32 tokenType, uint128 amount);

    /// @notice Emitted when an inbound transfer is received and credited
    event TransferReceived(address from, uint32 tokenType, uint128 amount);

    /// @notice PrivateNote constructor
    /// @param value Initial token balance
    /// @param ephemeralPubkey Ephemeral public key for authorization
    /// @param tokenType Type of token
    /// @param pmpCode PMP contract code used for deterministic PMP derivation.
    /// @param orderBookCode OrderBook contract code used for deterministic OB derivation.
    /// @param oracleCodeHash Oracle contract code hash.
    /// @param oracleCodeDepth Oracle contract code depth.
    /// @param oracleEventListCodeHash OracleEventList contract code hash.
    /// @param oracleEventListCodeDepth OracleEventList contract code depth.
    constructor(uint128 value, uint256 ephemeralPubkey, uint32 tokenType, TvmCell pmpCode, TvmCell orderBookCode,
                uint256 oracleCodeHash, uint16 oracleCodeDepth, uint256 oracleEventListCodeHash, uint16 oracleEventListCodeDepth) {
        tvm.accept();
        require(msg.sender == ROOT_PN_ADDRESS, ERR_INVALID_SENDER);
        _pmpCode = pmpCode;
        _privateNoteCode = tvm.code();
        _oracleCodeHash = oracleCodeHash;
        _oracleCodeDepth = oracleCodeDepth;
        _oracleEventListCodeHash = oracleEventListCodeHash;
        _oracleEventListCodeDepth = oracleEventListCodeDepth;
        _orderBookCode = orderBookCode;
        _balance[tokenType] = value;
        _ephemeralPubkey = ephemeralPubkey;
        RootPN(ROOT_PN_ADDRESS).privateNoteDeployed{value: 0.1 vmshell, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}(_depositIdentifierHash, tokenType, value);
    }

    /// @notice Changes the owner public key
    /// @param newPubkey New public key
    function changeOwner(uint256 newPubkey) public accept saveMsg {
        require(msg.pubkey() == _ephemeralPubkey, ERR_INVALID_SENDER);
        // Refuse to demote ownership to pubkey=0 — every onlyOwnerPubkey-gated
        // PN method would then accept msg.pubkey()==0, i.e. any unsigned tx.
        require(newPubkey != 0, ERR_INVALID_PARAMS);
        ensureBalance();

        uint256 oldPubkey = _ephemeralPubkey;
        _ephemeralPubkey = newPubkey;
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_OWNER_CHANGED, bitCntAddress);
        emit OwnerChanged{dest: addrExtern}(oldPubkey, newPubkey);
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
    /// @param eventId Unique identifier of the PMP event.
    /// @param oracleFee Array of additional fees (in shell tokens) for each oracle.
    ///                  Must match the length of `names` and `index`.
    /// @param tokenType Token type used by the PMP contract.
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
    function deployPMP(uint256 eventId, uint128[] oracleFee, uint32 tokenType, string[] names, uint128[] index, uint128[] initialStakes) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        uint256 length = names.length;
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(length == oracleFee.length, ERR_INVALID_PARAMS);
        require(length == index.length, ERR_INVALID_PARAMS);
        require(initialStakes.length > 0, ERR_INVALID_PARAMS);
        require(_debt == 0, ERR_DEBT_NON_ZERO);

        // Validate initial stakes and compute total
        uint128 initialLot = lotSize(tokenType);
        uint128 initialTotal = 0;
        for (uint32 i = 0; i < initialStakes.length; i++) {
            require(initialStakes[i] >= minStakeValue(tokenType), ERR_LOW_VALUE);
            require(initialStakes[i] % initialLot == 0, ERR_AMOUNT_NOT_LOT_MULTIPLE);
            initialTotal += initialStakes[i];
        }
        require(_balance[tokenType] >= initialTotal, ERR_LOW_VALUE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);

        ensureBalance();

        mapping(uint256 => bool) forOracleHash;

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
            forOracleHash[tvm.hash(names[i])] = true;
        }
        mapping(uint32 => varuint32) dataCur;
        dataCur[CURRENCIES_ID_SHELL] = sumFee + NETWORK_FEE_AMOUNT;

        uint256 oracleListHash = tvm.hash(abi.encode(forOracleHash));
        TvmCell stateInit = DexLib.buildPMPStateInit(_privateNoteCode, _pmpCode, eventId, oracleListHash, tokenType);
        address pmpAddress = DexLib.computePMPAddress(_privateNoteCode, _pmpCode, eventId, oracleListHash, tokenType);

        // Deduct initial stakes from balance and set pending stake record
        _balance[tokenType] -= initialTotal;
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        // candidateAmount = initialTotal signals pending initial stake
        _stakes[hash] = StakeInfo({
            amount: new uint128[](initialStakes.length),
            debtAmount: new uint128[](initialStakes.length),
            couponsAmount: new uint128[](initialStakes.length),
            candidateAmount: initialTotal,
            candidateOutcome: 0,
            candidateBetType: BET_TYPE_CLEAN,
            oracleListHash: oracleListHash,
            tokenType: tokenType
        });
        _busy = pmpAddress;
        _lastHash = hash;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_PMP_DEPLOYED, bitCntAddress);
        emit PMPDeployed{dest: addrExtern}(eventId, tokenType, pmpAddress, oracleEventLists, oracleFee);

        new PMP{
            stateInit: stateInit,
            value: 50 vmshell,
            currencies: dataCur,
            flag: 1,
            bounce: true
        }(_depositIdentifierHash, tokenType, oracleEventLists, oracleFee, initialStakes, _orderBookCode);
    }

    /// @notice Called by PMP after initial stakes (passed with deployPMP) are accepted
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param amounts Per-outcome initial stake amounts confirmed by PMP
    function onInitialStakesAccepted(uint256 eventId, uint256 oracleListHash, uint32 tokenType, uint128[] amounts)
        public senderIs(_busy.get()) accept
    {
        ensureBalance();
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        StakeInfo stake = _stakes[hash];
        for (uint32 i = 0; i < amounts.length; i++) {
            stake.amount[i] = amounts[i];
        }
        stake.candidateAmount = 0;
        _stakes[hash] = stake;
        delete _busy;
    }

    /// @notice Called by PMP when initial stakes are invalid (outcome count mismatch → PMP cancelled)
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param refundTotal Total amount to refund to balance
    function onInitialStakesFailed(uint256 eventId, uint256 oracleListHash, uint32 tokenType, uint128 refundTotal)
        public senderIs(_busy.get()) accept
    {
        ensureBalance();
        _balance[tokenType] += refundTotal;
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
        delete _busy;
    }

    /// @notice PMP normalization-refund callback (creator-only).
    /// @dev Sent by PMP at freeze time when each clean pool is reduced to a
    ///      multiple of `min(_initialStakes)`. Decrements the creator's
    ///      stake.amount[k] by the refunded outcome-token amounts (so the
    ///      creator can no longer claim/sell/merge tokens already returned
    ///      as collateral) and credits `refundTotal` back to `_balance`.
    /// @param eventId PMP event ID (used to recompute PMP address for sender check)
    /// @param oracleListHash Oracle list hash for PMP address derivation
    /// @param tokenType Token type managed by this PMP
    /// @param refundAmounts Per-outcome refunded clean-token amounts
    /// @param refundTotal Total collateral credited back to _balance
    function onPmpCleanRefund(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128[] refundAmounts,
        uint128 refundTotal
    ) public senderIs(DexLib.computePMPAddress(_privateNoteCode, _pmpCode, eventId, oracleListHash, tokenType)) accept {
        ensureBalance();
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);
        StakeInfo stake = _stakes[hash];
        for (uint32 k = 0; k < uint32(refundAmounts.length) && k < uint32(stake.amount.length); k++) {
            uint128 r = refundAmounts[k];
            if (r == 0) continue;
            require(stake.amount[k] >= r, ERR_LOW_VALUE);
            stake.amount[k] -= r;
        }
        _stakes[hash] = stake;
        _balance[tokenType] += refundTotal;
    }

    /// @notice Deletes a stake record
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of Oracles
    /// @param tokenType Token type
    function deleteStake(uint256 eventId, uint256 oracleListHash, uint32 tokenType) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
    }

    /// @notice Cancels a stake on a PMP contract
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of Oracles
    /// @param tokenType Token type
    function cancelStake(uint256 eventId, uint256 oracleListHash, uint32 tokenType) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);
        address pmpAddress = DexLib.computePMPAddress(_privateNoteCode, _pmpCode, eventId, _stakes[hash].oracleListHash, tokenType);
        _busy = pmpAddress;
        _lastHash = hash;
        PMP(pmpAddress).cancelStake{
            value: 0.1 vmshell, 
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_stakes[hash].amount, _stakes[hash].debtAmount, _stakes[hash].couponsAmount,_depositIdentifierHash);
    }

    /// @notice Called by PMP after stake is cancelled
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of Oracles
    /// @param tokenType Token type
    /// @param value Amount refunded
    /// @param couponValue Coupon amount refunded
    function onStakeCancelled(uint256 eventId, uint256 oracleListHash, uint32 tokenType, uint128 value, uint128 couponValue) 
        public senderIs(_busy.get()) accept
    {
        ensureBalance();
        
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
        _balance[tokenType] += value;
        _couponsValue += couponValue;
        
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
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param collateral Amount of collateral to split
    function splitFullSet(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128 collateral
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(collateral > 0, ERR_LOW_VALUE);
        require(collateral % lotSize(tokenType) == 0, ERR_AMOUNT_NOT_LOT_MULTIPLE);
        require(_balance[tokenType] >= collateral, ERR_LOW_VALUE);
        require(_debt == 0, ERR_DEBT_NON_ZERO);

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);

        _balance[tokenType] -= collateral;

        address pmpAddress = DexLib.computePMPAddress(
            _privateNoteCode,
            _pmpCode,
            eventId,
            oracleListHash,
            tokenType
        );

        _busy = pmpAddress;
        _lastHash = hash;

        StakeInfo stake = _stakes[hash];
        stake.candidateAmount = collateral;
        stake.candidateBetType = BET_TYPE_CLEAN;
        stake.oracleListHash = oracleListHash;
        stake.tokenType = tokenType;
        _stakes[hash] = stake;

        PMP(pmpAddress).splitFullSet{
            value: 0.1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(collateral, _depositIdentifierHash);
    }

    /// @notice Called by PMP after split is accepted.
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param amounts Outcome token amounts received from split
    function onSplitAccepted(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128[] amounts,
        uint128 collateralUsed
    ) public senderIs(_busy.get()) accept {
        ensureBalance();

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);

        StakeInfo stake = _stakes[hash];
        // Initialize arrays if this is the first split (no prior staking)
        if (stake.amount.length == 0) {
            stake.amount = new uint128[](amounts.length);
            stake.debtAmount = new uint128[](amounts.length);
            stake.couponsAmount = new uint128[](amounts.length);
        }
        for (uint32 i = 0; i < amounts.length; i++) {
            stake.amount[i] += amounts[i];
        }
        // Refund unused collateral (F - F_use) back to free balance.
        if (stake.candidateAmount > collateralUsed) {
            _balance[tokenType] += stake.candidateAmount - collateralUsed;
        }
        stake.candidateAmount = 0;
        _stakes[hash] = stake;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_SPLIT_CONFIRMED, bitCntAddress);
        emit FullSetStakeConfirmed{dest: addrExtern}(_busy.get(), amounts);

        delete _busy;
    }

    /// @notice Merges proportional outcome tokens back into collateral via PMP.
    /// @dev Sends outcome token amounts to PMP for merge. PMP verifies
    ///      proportionality, checks solvency, and calls back onMergeAccepted.
    ///
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param amount Array of outcome token amounts to merge
    function mergeFullSet(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128[] amount
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(amount.length > 0, ERR_INVALID_PARAMS);
        require(_debt == 0, ERR_DEBT_NON_ZERO);


        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);

        StakeInfo stake = _stakes[hash];
        require(amount.length == stake.amount.length, ERR_INVALID_PARAMS);

        // Verify PN has enough outcome tokens to merge
        uint128 mergeLot = lotSize(tokenType);
        uint128 total = 0;
        for (uint32 i = 0; i < amount.length; i++) {
            require(stake.amount[i] >= amount[i], ERR_LOW_VALUE);
            require(amount[i] % mergeLot == 0, ERR_AMOUNT_NOT_LOT_MULTIPLE);
            total += amount[i];
        }

        stake.candidateAmount = total;
        stake.candidateBetType = BET_TYPE_MERGE;
        _stakes[hash] = stake;

        address pmpAddress = DexLib.computePMPAddress(
            _privateNoteCode,
            _pmpCode,
            eventId,
            oracleListHash,
            tokenType
        );

        _busy = pmpAddress;
        _lastHash = hash;

        PMP(pmpAddress).mergeFullSet{
            value: 0.1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(amount, _depositIdentifierHash);
    }

    /// @notice Called by PMP after merge is accepted.
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param collateral Collateral amount returned from merge
    /// @param amounts Per-outcome token amounts that were merged
    function onMergeAccepted(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128 collateral,
        uint128[] amounts
    ) public senderIs(_busy.get()) accept {
        ensureBalance();

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);

        StakeInfo stake = _stakes[hash];
        bool isEmpty = true;
        for (uint32 i = 0; i < amounts.length; i++) {
            stake.amount[i] -= amounts[i];
            if ((stake.amount[i] > 0) || (stake.debtAmount[i] > 0) || (stake.couponsAmount[i] > 0)) {
                isEmpty = false;
            }
        }
        stake.candidateAmount = 0;

        if (isEmpty) {
            delete _stakes[hash];
        } else {
            _stakes[hash] = stake;
        }

        _balance[tokenType] += collateral;

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
    /// @param eventId PMP event identifier.
    /// @param oracleListHash Hash of oracle configuration.
    /// @param tokenType Token type.
    /// @param outcome Outcome index.
    /// @param amount Stake amount.
    /// @param useCoupon Whether to use coupon for this stake (if true, amount will be taken from available coupons instead of balance)
    ///
    /// Requirements:
    /// - Wallet must not be busy.
    /// - Amount must be greater than zero.
    /// - Sufficient balance or coupons must exist.
    function setStake(uint256 eventId, uint256 oracleListHash, uint32 tokenType, uint32 outcome, uint128 amount, bool useCoupon)
        public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg
    {
        ensureBalance();
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(amount >= minStakeValue(tokenType), ERR_LOW_VALUE);
        require(amount % lotSize(tokenType) == 0, ERR_AMOUNT_NOT_LOT_MULTIPLE);
        if (useCoupon) {
            require(_couponsValue >= amount, ERR_LOW_VALUE);
            require(tokenType == _couponsTokenType, ERR_INVALID_TOKEN_TYPE);
        } else {
            require(_balance[tokenType] >= amount, ERR_LOW_VALUE);
        }
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        address pmpAddress = DexLib.computePMPAddress(_privateNoteCode, _pmpCode, eventId, oracleListHash, tokenType);
        uint8 betType = _debt > 0 && _debtTokenType == tokenType ? 1 : 0;
        betType = useCoupon ? 2 : betType;

        if (_stakes.exists(hash)) {
            StakeInfo stake = _stakes[hash];
            require(stake.candidateAmount == 0, ERR_STAKE_NOT_APPROVED);
            stake.candidateAmount = amount;
            stake.candidateOutcome = outcome;
            stake.candidateBetType = betType;
            _stakes[hash] = stake;
        } else {
            _stakes[hash] = StakeInfo({
                amount: new uint128[](0),
                debtAmount: new uint128[](0),
                couponsAmount: new uint128[](0),
                candidateAmount: amount,
                candidateOutcome: outcome,
                candidateBetType: betType,
                oracleListHash: oracleListHash,
                tokenType: tokenType
            });
        }
        
        _busy = pmpAddress;
        _lastHash = hash;
        if (useCoupon) {
            _couponsValue -= amount;
        } else {
            _balance[tokenType] -= amount;
        }
        PMP(pmpAddress).acceptStake{
            value: 0.1 vmshell, 
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(outcome, amount, _depositIdentifierHash, betType);
    }

    /// @notice Called by PMP after stake is accepted
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of Oracles
    /// @param tokenType Token type
    /// @param outcomeCount Number of outcomes configured in PMP for this event.
    /// @param betType 0 - clean bet, 1 - debt bet, 2 - coupon bet
    function onStakeAccepted(uint256 eventId, uint256 oracleListHash, uint32 tokenType, uint128 outcomeCount, uint8 betType) 
        public senderIs(_busy.get()) 
    {
        tvm.accept();
        ensureBalance();
        
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        StakeInfo stake = _stakes[hash];
        uint128 amount = stake.candidateAmount;

        // Initialize all arrays on first stake (regardless of type)
        // This ensures claim() always has properly sized arrays to pass to PMP
        if (stake.amount.length == 0) {
            stake.amount = new uint128[](outcomeCount);
            stake.debtAmount = new uint128[](outcomeCount);
            stake.couponsAmount = new uint128[](outcomeCount);
        }

        if (betType == BET_TYPE_COUPON) {
            stake.couponsAmount[stake.candidateOutcome] += amount;
        } else if (betType == BET_TYPE_DEBT) {
            stake.debtAmount[stake.candidateOutcome] += amount;
        } else {
            stake.amount[stake.candidateOutcome] += amount;
        }
        stake.candidateAmount = 0;
        _stakes[hash] = stake;
        
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_STAKE_CONFIRMED, bitCntAddress);
        emit StakeConfirmed{dest: addrExtern}(_busy.get(), stake.candidateOutcome, amount, betType);
        delete _busy;
    }

    /// @notice Claims winnings from PMP
    /// @dev Sends claim request to PMP. Wallet enters busy state
    ///      until `onClaimAccepted` callback is received.
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    function claim(uint256 eventId, uint256 oracleListHash, uint32 tokenType) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        StakeInfo stake = _stakes[hash];

        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(stake.candidateAmount == 0, ERR_STAKE_NOT_APPROVED);

        ensureBalance();
        
        address pmpaddress = DexLib.computePMPAddress(_privateNoteCode, _pmpCode, eventId, stake.oracleListHash, stake.tokenType);
        _busy = pmpaddress;
        _lastHash = hash;
        
        PMP(pmpaddress).claim{
            value: 0.1 vmshell, 
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(stake.amount, stake.debtAmount, stake.couponsAmount, _depositIdentifierHash);
    }

    /// @notice Called by PMP after claim is processed
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of Oracles
    /// @param tokenType Token type for payout and debt accounting.
    /// @param outcome Optional outcome (if resolved)
    /// @param payoutClean Payout amount for clean bets
    /// @param payoutDebt Debt payout amount
    /// @param payoutCoupon Coupon payout amount
    /// @param debtPaid Amount of debt repaid from this payout (formula 17)
    function onClaimAccepted(uint256 eventId, uint256 oracleListHash, uint32 tokenType, optional(uint32) outcome, uint128 payoutClean, uint128 payoutDebt, uint128 payoutCoupon, uint128 debtPaid)
        public senderIs(_busy.get()) 
    {        
        tvm.accept();
        ensureBalance();
        
        if (!outcome.hasValue()) {
            delete _busy;
            return;
        } 
        
        _balance[tokenType] += payoutClean + payoutDebt + payoutCoupon;

        // Formula 10: Increase debt from coupon profit
        if (payoutCoupon > 0) {
            _debt += payoutCoupon;
        }

        // Formula 18: Decrease debt by debtPaid
        if (_debt > debtPaid) {
            _debt -= debtPaid;
        } else {
            _debt = 0;
        }
        address addrExtern = address.makeAddrExtern(PRIVATENOTE_CLAIM_ACCEPTED, bitCntAddress);
        emit ClaimAccepted{dest: addrExtern}(_busy.get(), outcome, payoutClean + payoutDebt + payoutCoupon);
        
        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);
        delete _stakes[hash];
        delete _busy;
    }

    /// @notice Accepts creator fee transfer from PMP and credits local token balance.
    /// @param fee Creator fee amount transferred from PMP.
    /// @param tokenType Token type in which the fee is credited.
    /// @param eventId Event identifier of the source PMP.
    /// @param oracleListHash Oracle set hash of the source PMP.
    function acceptFee(uint128 fee, uint32 tokenType, uint256 eventId, uint256 oracleListHash) public senderIs(DexLib.computePMPAddress(_privateNoteCode, _pmpCode, eventId, oracleListHash, tokenType)) accept {
        ensureBalance();
        _balance[tokenType] += fee;
    }

    /// @notice Returns fixed coupon nominal for a token type.
    /// @param tokenType Token type used for coupon issuance.
    /// @return couponValue Coupon nominal value for the given token type (0 if unsupported).
    function getCouponValue(uint32 tokenType) private pure returns (uint128) {
        if (tokenType == CURRENCIES_ID_SHELL) {
            return SHELL_COUPON_VALUE;
        } else if (tokenType == CURRENCIES_ID) {
            return NACKL_COUPON_VALUE;
        } else if (tokenType == CURRENCIES_ID_USDC) {
            return USDC_COUPON_VALUE;
        } else {
            return 0;
        }
    }

    /// @notice Generates a free coupon for the specified token type
    /// @param tokenType Token type for which to generate coupon
    /// @dev Can only generate coupon when:
    ///      - all token balances < minStakeValue (i.e. too small to stake)
    ///      - debt == 0
    ///      - No withdrawals from this wallet have been performed.
    ///      - No active stakes exist.
    ///      - No coupon currently exists.
    function generateCoupon(uint32 tokenType) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(_debt == 0, ERR_HAS_DEBT);
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(!_hasTransferred, ERR_INVALID_STATE);
        require(_stakes.empty(), ERR_NOTE_BUSY);
        for ((uint32 tt, uint128 bal) : _balance) {
            require(bal < minStakeValue(tt), ERR_NON_ZERO_BALANCE);
        }        
        require(_couponsValue == 0, ERR_COUPON_ALREADY_EXISTS);
        _couponsValue = getCouponValue(tokenType);
        _couponsTokenType = tokenType;

        uint128 baseDebt = _couponsValue * 5 / 100;
        _debt = baseDebt;
        _debtTokenType = tokenType;
    }

    /// @notice Receives funds to the wallet
    receive() external {
        tvm.accept();
        ensureBalance();
    }

    /// @notice Handles bounced messages from PMP contracts and remote PrivateNotes.
    /// @dev Distinguishes between a transfer bounce (_pendingTransferAmount > 0)
    ///      and a PMP operation bounce (candidateAmount in _stakes[_lastHash]).
    /// @param body Bounced message body (kept for ABI compatibility; not decoded).
    onBounce(TvmSlice body) external {
        tvm.accept();
        ensureBalance();
        body;
        if (!_busy.hasValue() || msg.sender != _busy.get()) {
            return;
        }

        // --- Batch bounce: executePlaceBatch/executeCancelBatch/executeCancelAll ---
        if (_pendingBatchActive) {
            // Restore pre-batch balance and stake amounts.
            if (_pendingBatchBuyLock > 0) {
                _balance[_pendingBatchTokenType] += _pendingBatchBuyLock;
                if (_lockedInOrders[_pendingBatchTokenType] >= _pendingBatchBuyLock) {
                    _lockedInOrders[_pendingBatchTokenType] -= _pendingBatchBuyLock;
                } else {
                    _lockedInOrders[_pendingBatchTokenType] = 0;
                }
            }
            if (_pendingBatchSells.length > 0 && _stakes.exists(_pendingBatchStakeHash)) {
                StakeInfo s = _stakes[_pendingBatchStakeHash];
                for (uint32 i = 0; i < uint32(_pendingBatchSells.length); i++) {
                    PendingBatchSell ps = _pendingBatchSells[i];
                    s.amount[ps.outcomeId] += ps.amount;
                }
                _stakes[_pendingBatchStakeHash] = s;
            }
            _pendingBatchActive = false;
            _pendingBatchBuyLock = 0;
            _pendingBatchTokenType = 0;
            _pendingBatchStakeHash = 0;
            delete _pendingBatchSells;
            delete _busy;
            return;
        }

        // --- Single-order placeOrder buy bounce ---
        // placeOrder buy locks BOTH _balance and _lockedInOrders, so the
        // bounce must release BOTH. (Previously this path overloaded
        // _pendingTransferAmount and indiscriminately mutated
        // _lockedInOrders on every bounce, which corrupted state on a
        // bounced offerTransfer — fixed by splitting into a dedicated
        // _pendingPlaceBuyLock slot.)
        if (_pendingPlaceBuyLock > 0) {
            _balance[_pendingPlaceBuyTokenType] += _pendingPlaceBuyLock;
            if (_lockedInOrders[_pendingPlaceBuyTokenType] >= _pendingPlaceBuyLock) {
                _lockedInOrders[_pendingPlaceBuyTokenType] -= _pendingPlaceBuyLock;
            } else {
                _lockedInOrders[_pendingPlaceBuyTokenType] = 0;
            }
            _pendingPlaceBuyLock = 0;
            _pendingPlaceBuyTokenType = 0;
            delete _busy;
            return;
        }

        // --- initTransfer (offerTransfer) bounce ---
        // Restore _balance only. initTransfer never mutates _lockedInOrders
        // (transfers move user-owned tokens between PNs, not order
        // collateral), so onBounce must not touch it either.
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
        if (stake.candidateBetType == BET_TYPE_COUPON) {
            _couponsValue += stake.candidateAmount;
        } else if (stake.candidateBetType == BET_TYPE_OB_SELL) {
            // Sell order bounce: return outcome tokens to stake
            stake.amount[stake.candidateOutcome] += stake.candidateAmount;
        } else if (stake.candidateBetType == BET_TYPE_MERGE) {
            // Merge bounce: nothing was deducted from _balance, outcome tokens
            // are still in stake.amount — just clear candidate, no restoration needed.
        } else {
            _balance[stake.tokenType] += stake.candidateAmount;
        }

        stake.candidateAmount = 0;
        _stakes[_lastHash] = stake;

        // Delete stake record if no confirmed amounts remain
        // (covers both: regular stake with no history, and full-set
        //  stake that bounced on the very first attempt)
        bool allEmpty = true;
        for (uint32 i = 0; i < stake.amount.length; i++) {
            if (stake.amount[i] > 0) { allEmpty = false; break; }
        }
        for (uint32 i = 0; i < stake.debtAmount.length; i++) {
            if (stake.debtAmount[i] > 0) { allEmpty = false; break; }
        }
        for (uint32 i = 0; i < stake.couponsAmount.length; i++) {
            if (stake.couponsAmount[i] > 0) { allEmpty = false; break; }
        }
        if (allEmpty) {
            delete _stakes[_lastHash];
        }
    }

    // ── PrivateNote-to-PrivateNote transfer ──────────────────────────────────────

    /// @notice Initiates a token transfer to another PrivateNote.
    /// @dev Destination address is derived deterministically from destDepositHash.
    ///      Deducts amount from balance immediately and sets _busy = dest.
    ///      The receiving PrivateNote credits the tokens automatically.
    ///      If offerTransfer bounces, onBounce restores the balance.
    /// @param destDepositHash _depositIdentifierHash of the destination PrivateNote
    /// @param tokenType Token type to transfer
    /// @param amount Amount to transfer (must be >= minStakeValue)
    function initTransfer(uint256 destDepositHash, uint32 tokenType, uint128 amount)
        public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg
    {
        ensureBalance();
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(_stakes.empty(), ERR_NOTE_BUSY);
        require(amount >= minStakeValue(tokenType), ERR_LOW_VALUE);
        require(_balance[tokenType] >= amount, ERR_LOW_VALUE);
        require(_debt == 0, ERR_DEBT_NON_ZERO);
        require(_couponsValue == 0, ERR_COUPON_ACTIVE);
        require(destDepositHash != _depositIdentifierHash, ERR_INVALID_PARAMS);

        address dest = DexLib.computePrivateNoteAddress(_privateNoteCode, destDepositHash);

        _balance[tokenType] -= amount;
        _pendingTransferAmount = amount;
        _pendingTransferTokenType = tokenType;
        _hasTransferred = true;
        _busy = dest;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_TRANSFER_INITIATED, bitCntAddress);
        emit TransferInitiated{dest: addrExtern}(dest, tokenType, amount);

        PrivateNote(dest).offerTransfer{value: 0.1 vmshell, flag: 1, bounce: true, dest_dapp_id: ROOT_PN_DAPP_ID}(
            tokenType, amount, _depositIdentifierHash
        );
    }

    /// @notice Called by a remote PrivateNote to deliver a transfer.
    /// @dev Verifies the sender is a valid PrivateNote via deterministic address derivation.
    ///      Credits tokens immediately and notifies the sender.
    /// @param tokenType Token type being transferred
    /// @param amount Amount being transferred
    /// @param senderDepositHash _depositIdentifierHash of the sending PrivateNote
    function offerTransfer(uint32 tokenType, uint128 amount, uint256 senderDepositHash) public accept {
        ensureBalance();
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(
            msg.sender == DexLib.computePrivateNoteAddress(_privateNoteCode, senderDepositHash),
            ERR_INVALID_SENDER
        );
        _balance[tokenType] += amount;

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_TRANSFER_CONFIRMED, bitCntAddress);
        emit TransferReceived{dest: addrExtern}(msg.sender, tokenType, amount);

        PrivateNote(msg.sender).onTransferAccepted{value: 0.1 vmshell, flag: 1, bounce: false, dest_dapp_id: ROOT_PN_DAPP_ID}();
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
    function clearTransferBusy() public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(_pendingTransferAmount > 0, ERR_INVALID_STATE);
        _pendingTransferAmount = 0;
        delete _busy;
    }

    // ── Coupon management ─────────────────────────────────────────────────────────

    /// @notice Discards the current coupon without using it.
    /// @dev Allowed only when coupon exists and no active stakes are pending.
    ///      The debt created at coupon issuance remains and must be repaid normally.
    function discardCoupon() public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(_couponsValue > 0, ERR_NO_COUPON_AVAILABLE);
        _couponsValue = 0;
        _couponsTokenType = 0;
    }

    /// @notice Withdraws tokens to a specified wallet.
    /// @dev Inner action flag is hard-coded to 1 inside RootPN — accepting a
    ///      caller-supplied flag opens TVM flag 128 (CARRY_ALL_BALANCE) and 32
    ///      (DELETE_IF_EMPTY) abuse paths that drain or destroy RootPN.
    /// @param destWalletAddr Destination wallet address
    /// @param tokenType Token type to withdraw.
    function withdrawTokens(address destWalletAddr, uint32 tokenType) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(_stakes.empty(), ERR_NOTE_BUSY);
        require(_debt == 0, ERR_DEBT_NON_ZERO);
        RootPN(ROOT_PN_ADDRESS).withdrawTokens{value: 0.1 vmshell, bounce: false, flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID}(_balance[tokenType], tokenType, destWalletAddr, _depositIdentifierHash);
        _balance[tokenType] = 0;
        _hasWithdrawn = true;
	}

    /// @notice Reverts a withdraw operation (called by Vault)
    /// @param tokenType Type of token
    /// @param value Amount to revert
    function revertWithdraw(uint32 tokenType, uint128 value) public senderIs(ROOT_PN_ADDRESS) accept {
        ensureBalance();
        _balance[tokenType] += value;
        // Clear the withdrawn latch: RootPN.withdrawTokens only calls this
        // path when the withdraw did NOT happen (insufficient RootPN
        // liquidity). Without the reset the PN stays permanently pinned
        // in the `_hasWithdrawn=true` state, which blocks setStake,
        // split/merge, claim, generateCoupon, initTransfer, batch OB ops,
        // etc. — functionally the wallet is frozen despite having a
        // restored balance.
        _hasWithdrawn = false;
    }



    // ===== Order Book Functions =====

    /// @notice Places a limit order on the order book for a specific outcome.
    /// @dev For sell orders: locks outcome tokens (reduces stake.amount[outcomeId]).
    ///      For buy orders: locks collateral from balance.
    ///      Sets _busy until onOrderPlaced callback.
    ///
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param outcomeId Outcome to trade
    /// @param isBuy True for buy, false for sell
    /// @param price Limit price in basis points (10000 bps = 1 collateral; no upper bound; ignored for market orders)
    /// @param amount Amount of outcome tokens to trade
    /// @param flags Order flags: IOC=0x01, FOK=0x02, MARKET=0x04
    /// @param minAmount Minimum fill amount (0 = no minimum)
    /// @param epochId Epoch identifier used by dark order book matching.
    function placeOrder(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint32 outcomeId,
        bool isBuy,
        uint256 price,
        uint128 amount,
        uint8 flags,
        uint128 minAmount,
        uint64 epochId,
        uint128 clientOrderId
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(amount > 0, ERR_LOW_VALUE);
        require(_debt == 0, ERR_DEBT_NON_ZERO);

        // Amount quantisation in base currency (outcome-tokens).
        require(amount % lotSize(tokenType) == 0, ERR_AMOUNT_NOT_LOT_MULTIPLE);

        // Minimum order notional (value in quote currency) + tick size on price.
        uint128 minNotional = minOrderNotional(tokenType);
        if (flags & 0x04 != 0) {
            if (isBuy) {
                require(amount >= minNotional, ERR_ORDER_TOO_SMALL);
            }
        } else {
            require(price % TICK_SIZE == 0, ERR_PRICE_NOT_TICK_MULTIPLE);
            uint128 notional = uint128(
                (uint256(amount) * uint256(price)) / uint256(FULL_PERCENT)
            );
            require(notional >= minNotional, ERR_ORDER_TOO_SMALL);
        }

        {
            address addrExtern = address.makeAddrExtern(PRIVATENOTE_ORDER_SUBMITTED, bitCntAddress);
            emit OrderSubmitted{dest: addrExtern}(clientOrderId, outcomeId, isBuy, price, amount, flags, eventId, tokenType);
        }

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);

        if (isBuy) {
            // Buy: lock collateral + max fee reserve from balance.
            // The feeReserve is recomputed by OrderBook and sent back in onOrderPlaced.
            uint128 cost;
            if (flags & 0x04 != 0) {
                cost = amount; // market buy: lock full balance as collateral
            } else {
                cost = uint128((uint256(amount) * uint256(price)) / uint256(FULL_PERCENT));
            }
            uint128 maxFee = uint128(
                (uint256(cost) * uint256(TAKER_FEE_RATE)) / uint256(FEE_DENOMINATOR)
            );
            require(_balance[tokenType] >= cost + maxFee, ERR_LOW_VALUE);
            _balance[tokenType] -= (cost + maxFee);
            _lockedInOrders[tokenType] += (cost + maxFee);
            // Track locked collateral + fee reserve so onBounce can restore
            // BOTH _balance and _lockedInOrders. Separate slot from
            // _pendingTransferAmount (which belongs to initTransfer's
            // bounce path that does NOT touch _lockedInOrders).
            _pendingPlaceBuyLock = cost + maxFee;
            _pendingPlaceBuyTokenType = tokenType;
        } else {
            // Sell: lock outcome tokens
            require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);
            StakeInfo stake = _stakes[hash];
            require(outcomeId < uint32(stake.amount.length), ERR_INVALID_OUTCOME_ID);
            require(stake.amount[outcomeId] >= amount, ERR_LOW_VALUE);
            stake.amount[outcomeId] -= amount;
            // Track locked tokens so onBounce can restore them if execute() bounces
            stake.candidateAmount = amount;
            stake.candidateOutcome = outcomeId;
            stake.candidateBetType = BET_TYPE_OB_SELL;
            _stakes[hash] = stake;
        }

        address obAddress = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );

        _busy = obAddress;
        _lastHash = hash;

        OrderBook.PlaceParams[] orderArr;
        orderArr.push(OrderBook.PlaceParams(outcomeId, isBuy, flags, price, amount, minAmount, epochId, clientOrderId));
        uint128[] noCancels;
        OrderBook(obAddress).executeBatch{
            value: 1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_depositIdentifierHash, orderArr, noCancels);
    }

    /// @notice Called by OrderBook after order is placed.
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param orderId Assigned order ID
    /// @param feeReserve Max fee reserve for this order (OB-computed, authoritative).
    ///                   Non-zero for buy orders; zero for sells.
    /// @param lock Full buy-side lock = cost + feeReserve (authoritative OB
    ///             value). Stored in `_orderLocks[orderId]` so floor-accumulated
    ///             residuals can be refunded on final fill / cancel. Zero for
    ///             sells.
    function onOrderPlaced(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128 orderId,
        uint128 feeReserve,
        uint128 lock
    ) public accept {
        // Sender check: allow either active single-op OB (_busy) or any OB from this event
        // (because in batch mode _busy is cleared in onBatchComplete, not on first callback).
        address expectedOb = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );
        require(msg.sender == expectedOb, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();

        // Store fee reserve + total lock per (OB, orderId) (buys only;
        // sells have feeReserve == 0 and lock == 0). msg.sender == expectedOb
        // is verified above, so we can use it directly as the outer key.
        if (feeReserve > 0) {
            _orderFeeReserves[msg.sender][orderId] = feeReserve;
            _orderLocks[msg.sender][orderId] = lock;
        }

        // For single-place path only: clear pending place-buy lock, sell candidate, _busy.
        // Batch-place path leaves these alone; onBatchComplete performs final cleanup.
        if (!_pendingBatchActive) {
            _pendingPlaceBuyLock = 0;
            _pendingPlaceBuyTokenType = 0;
            TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
            uint256 hash = tvm.hash(data);
            if (_stakes.exists(hash)) {
                StakeInfo stake = _stakes[hash];
                if (stake.candidateAmount > 0 && stake.candidateBetType == BET_TYPE_OB_SELL) {
                    stake.candidateAmount = 0;
                    _stakes[hash] = stake;
                }
            }
            if (_busy.hasValue() && _busy.get() == msg.sender) {
                delete _busy;
            }
        }

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_ORDER_PLACED, bitCntAddress);
        emit OrderPlacedConfirmed{dest: addrExtern}(msg.sender, orderId);
    }

    /// @notice Called by OrderBook when a place submission was rejected (queue full or
    ///         invalid params). Restores any balance / outcome-token lock that was
    ///         taken at placement time.
    /// @dev Signature mirrors PlaceParams so PN can deterministically reconstruct
    ///      the original lock amount without consulting state.
    function onOrderRejected(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint32 outcomeId,
        bool isBuy,
        uint8 flags,
        uint256 price,
        uint128 amount
    ) public accept {
        ensureBalance();
        address expectedOb = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );
        require(msg.sender == expectedOb, ERR_INVALID_SENDER);
        tvm.accept();

        if (isBuy) {
            // Reconstruct original lock (cost + maxFee)
            uint128 cost;
            if (flags & 0x04 != 0) {
                cost = amount;
            } else {
                cost = uint128((uint256(amount) * uint256(price)) / uint256(FULL_PERCENT));
            }
            uint128 maxFee = uint128(
                (uint256(cost) * uint256(TAKER_FEE_RATE)) / uint256(FEE_DENOMINATOR)
            );
            uint128 lock = cost + maxFee;
            _balance[tokenType] += lock;
            if (_lockedInOrders[tokenType] >= lock) {
                _lockedInOrders[tokenType] -= lock;
            } else {
                _lockedInOrders[tokenType] = 0;
            }
        } else {
            // Sell: restore outcome-token lock in stake
            TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
            uint256 hash = tvm.hash(data);
            if (_stakes.exists(hash)) {
                StakeInfo stake = _stakes[hash];
                if (outcomeId < uint32(stake.amount.length)) {
                    stake.amount[outcomeId] += amount;
                    _stakes[hash] = stake;
                }
            }
        }
    }

    /// @notice Cancels an existing order on the order book.
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param orderId Order ID to cancel
    function cancelOrder(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128 orderId
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);


        address obAddress = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        _lastHash = tvm.hash(data);
        _busy = obAddress;

        OrderBook.PlaceParams[] noOrders;
        uint128[] cancelArr;
        cancelArr.push(orderId);
        OrderBook(obAddress).executeBatch{
            value: 1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_depositIdentifierHash, noOrders, cancelArr);
    }

    function cancelOrderByClient(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128 clientOrderId
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);

        address obAddress = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        _lastHash = tvm.hash(data);
        _busy = obAddress;

        OrderBook(obAddress).cancelByClientId{
            value: 1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_depositIdentifierHash, clientOrderId);
    }

    /// @notice Called by OrderBook after order is cancelled. Returns locked tokens.
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param orderId Cancelled order ID
    /// @param outcomeId Outcome of the cancelled order
    /// @param isBuy Whether it was a buy order
    /// @param amount Amount that was locked
    function onOrderCancelled(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128 orderId,
        uint32 outcomeId,
        bool isBuy,
        uint128 amount
    ) public accept {
        // Verify sender is the correct OrderBook (same pattern as onOrderFilled)
        address expectedOb = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );
        require(msg.sender == expectedOb, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();
        orderId; // suppress unused warning

        if (isBuy) {
            // Return the authoritative remaining lock for this order.
            // `_orderLocks[ob][orderId]` is kept in sync by
            // onOrderPlaced/Filled and already accounts for per-fill floor
            // residuals, so we refund it verbatim (rather than recomputing
            // `amount + feeReserve` from OB's floor-truncated `amount`,
            // which would leak sub-unit dust back into `_lockedInOrders`).
            // Keyed by (ob, orderId) — different OBs (one per PMP event)
            // have independent `_nextOrderId` sequences and would collide
            // under a flat mapping.
            uint128 returned = _orderLocks[msg.sender][orderId];
            // Defensive fallback: if for some reason `_orderLocks` is
            // missing (pre-upgrade orders), fall back to the old formula.
            if (returned == 0) {
                uint128 feeReserveFallback = _orderFeeReserves[msg.sender][orderId];
                returned = amount + feeReserveFallback;
            }
            _balance[tokenType] += returned;
            if (_lockedInOrders[tokenType] >= returned) {
                _lockedInOrders[tokenType] -= returned;
            } else {
                _lockedInOrders[tokenType] = 0;
            }
            if (_orderFeeReserves[msg.sender].exists(orderId)) {
                delete _orderFeeReserves[msg.sender][orderId];
            }
            if (_orderLocks[msg.sender].exists(orderId)) {
                delete _orderLocks[msg.sender][orderId];
            }
        } else {
            // Return outcome tokens to the stake. If the stake was already
            // deleted (e.g., user has claimed), the outcome tokens are
            // PMP-internal and are silently dropped — they have no
            // standalone value here. Unlike collateral above, we cannot
            // credit them to _balance (different unit, different semantics).
            TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
            uint256 hash = tvm.hash(data);
            if (_stakes.exists(hash)) {
                StakeInfo stake = _stakes[hash];
                stake.amount[outcomeId] += amount;
                _stakes[hash] = stake;
            }
        }

        // Clear _busy only if it still points to this OrderBook (explicit cancelOrder flow).
        // For IOC/FOK auto-cancels, _busy was already cleared by onOrderPlaced.
        // For batch operations, _busy is cleared in onBatchComplete.
        if (!_pendingBatchActive && _busy.hasValue() && _busy.get() == msg.sender) {
            delete _busy;
        }

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_ORDER_CANCELLED, bitCntAddress);
        emit OrderCancelledConfirmed{dest: addrExtern}(msg.sender, orderId, outcomeId, isBuy, amount);
    }

    /// @notice Called by OrderBook when an order is filled during epoch settlement.
    /// @param eventId PMP event ID
    /// @param oracleListHash Hash of oracle configuration
    /// @param tokenType Token type
    /// @param outcomeId Outcome that was traded
    /// @param filledAmount Amount of outcome tokens filled
    /// @param clearingPrice Clearing price in basis points
    /// @param isBuy Whether this was a buy fill
    /// @param refundAmount Collateral refund for buy orders (overpaid above clearing price)
    /// @param feeAmount Trading fee (maker or taker) calculated by OrderBook
    function onOrderFilled(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint32 outcomeId,
        uint128 filledAmount,
        uint256 clearingPrice,
        bool isBuy,
        uint128 refundAmount,
        uint128 feeAmount,
        uint128 orderId,
        bool isFinal
    ) public accept {
        ensureBalance();
        // Verify sender is the OrderBook for this event
        address expectedOb = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );
        require(msg.sender == expectedOb, ERR_INVALID_SENDER);

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
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
            // Fee is covered by per-order fee reserve (populated by OB in onOrderPlaced).
            // Each fill consumes its exact fee from the reserve. On the final
            // fill (isFinal=true) any unused reserve is refunded and the entry
            // deleted. Otherwise the reserve carries over for subsequent fills;
            // if the order is later cancelled, onOrderCancelled refunds what's
            // left. This keeps multi-fill orders charged exactly once per
            // actual fill (pre-fix: delete-on-first-fill let fills 2..N skip
            // payment).
            // All per-order state is keyed by (obAddress, orderId) to
            // avoid collisions with other PMP events' OBs (each OB has its
            // own `_nextOrderId` starting at 1). msg.sender is the verified
            // OB address.
            uint128 reserve = _orderFeeReserves[msg.sender][orderId];
            uint128 newReserve = reserve >= feeAmount ? reserve - feeAmount : 0;
            uint128 feeRefund = 0;
            if (isFinal) {
                // Order done: refund the unused reserve and clean up.
                feeRefund = newReserve;
                newReserve = 0;
            }
            if (newReserve == 0) {
                if (_orderFeeReserves[msg.sender].exists(orderId)) {
                    delete _orderFeeReserves[msg.sender][orderId];
                }
            } else {
                _orderFeeReserves[msg.sender][orderId] = newReserve;
            }
            // Credit the price-diff refund (for limit buys filled below cap
            // price) plus any final-fill fee refund.
            _balance[tokenType] += refundAmount + feeRefund;
            // Unlock: price-diff refund + final-fill fee refund + actual cost + this fill's fee.
            uint128 actualCost = uint128(
                (uint256(filledAmount) * uint256(clearingPrice)) / uint256(FULL_PERCENT)
            );
            uint128 consumed = refundAmount + feeRefund + actualCost + feeAmount;

            // Per-order lock tracker: `_orderLocks[ob][orderId]` is the
            // authoritative remaining lock for this order (set at
            // placement from OB's `cost + feeReserve`). Decrement by the
            // consumed amount; on `isFinal`, drain any floor-accumulated
            // residual back to `_balance` so `_lockedInOrders` empties
            // exactly per closed order instead of leaving cosmetic dust.
            uint128 orderLock = _orderLocks[msg.sender][orderId];
            uint128 applyDec = orderLock >= consumed ? consumed : orderLock;

            if (isFinal) {
                uint128 residual = orderLock > applyDec ? orderLock - applyDec : 0;
                _balance[tokenType] += residual;
                uint128 totalUnlock = applyDec + residual; // == orderLock
                if (_lockedInOrders[tokenType] >= totalUnlock) {
                    _lockedInOrders[tokenType] -= totalUnlock;
                } else {
                    _lockedInOrders[tokenType] = 0;
                }
                if (_orderLocks[msg.sender].exists(orderId)) {
                    delete _orderLocks[msg.sender][orderId];
                }
            } else {
                _orderLocks[msg.sender][orderId] = orderLock - applyDec;
                if (_lockedInOrders[tokenType] >= applyDec) {
                    _lockedInOrders[tokenType] -= applyDec;
                } else {
                    _lockedInOrders[tokenType] = 0;
                }
            }
        } else {
            // Sold outcome tokens: receive collateral minus fee
            uint128 proceeds = uint128(
                (uint256(filledAmount) * uint256(clearingPrice)) / uint256(FULL_PERCENT)
            );
            if (proceeds > feeAmount) {
                _balance[tokenType] += (proceeds - feeAmount);
            }
        }

        address addrExtern = address.makeAddrExtern(PRIVATENOTE_ORDER_FILLED, bitCntAddress);
        emit OrderFilledConfirmed{dest: addrExtern}(msg.sender, orderId, outcomeId, filledAmount, clearingPrice, isBuy, feeAmount, isFinal);
    }

    // ===== Batch order-book operations =====
    // MAX_BATCH_SIZE is inherited from Modifiers (shared with OrderBook).

    /// @notice Places a batch of orders atomically. All-or-nothing in WASM;
    ///         if any order is invalid, the whole batch bounces and state is restored.
    function placeBatch(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        OrderBook.PlaceParams[] orders
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_hasWithdrawn, ERR_INVALID_STATE);
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(_debt == 0, ERR_DEBT_NON_ZERO);
        require(orders.length > 0, ERR_EMPTY_BATCH);
        require(orders.length <= MAX_BATCH_SIZE, ERR_BATCH_TOO_LARGE);


        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        uint256 hash = tvm.hash(data);

        // Clear any leftover batch bounce-protection arrays (defensive).
        delete _pendingBatchSells;

        uint128 totalBuyLock = 0;
        bool hasSells = false;
        StakeInfo stake;
        if (_stakes.exists(hash)) {
            stake = _stakes[hash];
        }

        uint128 minNotional = minOrderNotional(tokenType);
        uint128 lot = lotSize(tokenType);
        for (uint32 i = 0; i < uint32(orders.length); i++) {
            OrderBook.PlaceParams p = orders[i];
            require(p.amount > 0, ERR_LOW_VALUE);
            require(p.amount % lot == 0, ERR_AMOUNT_NOT_LOT_MULTIPLE);
            if (p.flags & 0x04 != 0) {
                if (p.isBuy) {
                    require(p.amount >= minNotional, ERR_ORDER_TOO_SMALL);
                }
            } else {
                require(p.price % TICK_SIZE == 0, ERR_PRICE_NOT_TICK_MULTIPLE);
                uint128 notional = uint128(
                    (uint256(p.amount) * uint256(p.price)) / uint256(FULL_PERCENT)
                );
                require(notional >= minNotional, ERR_ORDER_TOO_SMALL);
            }

            if (p.isBuy) {
                uint128 cost;
                if (p.flags & 0x04 != 0) {
                    cost = p.amount; // market buy: lock full amount as collateral
                } else {
                    cost = uint128((uint256(p.amount) * p.price) / uint256(FULL_PERCENT));
                }
                uint128 maxFee = uint128(
                    (uint256(cost) * uint256(TAKER_FEE_RATE)) / uint256(FEE_DENOMINATOR)
                );
                totalBuyLock += cost + maxFee;
            } else {
                require(_stakes.exists(hash), ERR_STAKE_NOT_EXISTS);
                require(p.outcomeId < uint32(stake.amount.length), ERR_INVALID_OUTCOME_ID);
                require(stake.amount[p.outcomeId] >= p.amount, ERR_LOW_VALUE);
                stake.amount[p.outcomeId] -= p.amount;
                _pendingBatchSells.push(PendingBatchSell({
                    outcomeId: p.outcomeId, amount: p.amount
                }));
                hasSells = true;
            }
        }

        require(_balance[tokenType] >= totalBuyLock, ERR_LOW_VALUE);
        _balance[tokenType] -= totalBuyLock;
        _lockedInOrders[tokenType] += totalBuyLock;

        if (hasSells) {
            _stakes[hash] = stake;
        }

        _pendingBatchActive = true;
        _pendingBatchBuyLock = totalBuyLock;
        _pendingBatchTokenType = tokenType;
        _pendingBatchStakeHash = hash;

        address obAddress = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );
        _busy = obAddress;
        _lastHash = hash;

        uint128[] emptyIds;
        OrderBook(obAddress).executeBatch{
            value: 1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_depositIdentifierHash, orders, emptyIds);
    }

    /// @notice Cancels a batch of orders by ID atomically. All-or-nothing.
    function cancelBatch(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType,
        uint128[] orderIds
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);
        require(orderIds.length > 0, ERR_EMPTY_BATCH);
        require(orderIds.length <= MAX_BATCH_SIZE, ERR_BATCH_TOO_LARGE);


        address obAddress = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );

        TvmCell data = abi.encode(eventId, oracleListHash, tokenType);
        _lastHash = tvm.hash(data);
        _pendingBatchActive = true;
        _busy = obAddress;

        OrderBook.PlaceParams[] emptyOrders;
        OrderBook(obAddress).executeBatch{
            value: 1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_depositIdentifierHash, emptyOrders, orderIds);
    }

    /// @notice Enqueues a CANCEL_ALL request on the given OrderBook. The OB will
    ///         cancel up to MAX_MATCHES_PER_CALL orders per processing pass and
    ///         self-invoke until all owner orders are cleared.
    function cancelAllOrders(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType
    ) public onlyOwnerPubkey(_ephemeralPubkey) accept saveMsg {
        ensureBalance();
        require(!_busy.hasValue(), ERR_NOTE_BUSY);

        address obAddress = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );

        // Latch busy + batch-mode so per-order onOrderCancelled callbacks during
        // the cancellation pass don't accidentally clear _busy. onBatchComplete
        // releases the latch.
        _pendingBatchActive = true;
        _pendingBatchTokenType = tokenType;
        _busy = obAddress;

        OrderBook(obAddress).cancelAllOrders{
            value: 1 vmshell,
            flag: 1, dest_dapp_id: ROOT_PN_DAPP_ID
        }(_depositIdentifierHash);
    }

    /// @notice Sentinel callback sent by OrderBook after all effects of a batch
    ///         operation have been dispatched. Clears _busy and any pending
    ///         bounce-protection state. Only the OrderBook for this pair may call.
    function onBatchComplete(
        uint256 eventId,
        uint256 oracleListHash,
        uint32 tokenType
    ) public accept {
        address expectedOb = DexLib.computeOrderBookAddress(
            _privateNoteCode,
            _orderBookCode,
            eventId,
            oracleListHash,
            tokenType
        );
        require(msg.sender == expectedOb, ERR_INVALID_SENDER);
        tvm.accept();
        ensureBalance();

        // All effects dispatched — tx committed, bounce protection no longer needed.
        _pendingBatchActive = false;
        _pendingBatchBuyLock = 0;
        _pendingBatchTokenType = 0;
        _pendingBatchStakeHash = 0;
        delete _pendingBatchSells;

        if (_busy.hasValue() && _busy.get() == msg.sender) {
            delete _busy;
        }
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
        TvmCell salt = abi.encode(_privateNoteCode);
        TvmCell code = abi.setCodeSalt(_pmpCode, salt);
        return (code, tvm.hash(code));
    }

    /// @notice Returns all global variables
    /// @return depositIdentifierHash Deposit identifier hash
    /// @return ephemeralPubkey Ephemeral public key
    /// @return balance Current free token balance
    /// @return lockedInOrders Collateral locked in open buy orders
    /// @return pmpCodeHash Hash of PMP code
    /// @return privateNoteCodeHash Hash of PrivateNote code
    /// @return busyAddress Current busy PMP address (if any)
    function getDetails() external view returns (
        uint256 depositIdentifierHash,
        uint256 ephemeralPubkey,
        mapping(uint32 => uint128) balance,
        mapping(uint32 => uint128) lockedInOrders,
        uint256 pmpCodeHash,
        uint256 privateNoteCodeHash,
        optional(address) busyAddress,
        uint128 couponsValue,
        bool hasWithdrawn
    ) {
        return (
            _depositIdentifierHash,
            _ephemeralPubkey,
            _balance,
            _lockedInOrders,
            tvm.hash(_pmpCode),
            tvm.hash(_privateNoteCode),
            _busy,
            _couponsValue,
            _hasWithdrawn
        );
    }

    /// @notice Returns contract name
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "PrivateNote");
    }
}
