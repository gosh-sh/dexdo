pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./PrivateNote.sol";
import "./PMP.sol";
import "./libraries/DexLib.sol";

/// @title OrderBook - Dark Epoch-Based Order Book for Prediction Market Tokens
/// @notice Single-entry-point contract: all business logic lives in WASM.
///         Solidity passes _state blob to WASM, WASM returns new state + effects,
///         Solidity persists state and dispatches callbacks/events.
contract OrderBook is Modifiers {

    /// @notice Contract semantic version.
    string constant version = "1.0.2";

    /// @notice Event identifier associated with this order book.
    uint256 static _event_id;
    /// @notice Oracle list hash associated with this order book.
    uint256 static _oracle_list_hash;
    /// @notice Token type associated with this order book.
    uint32 static _token_type;

    /// @notice PrivateNote code used for deterministic wallet address resolution.
    TvmCell _PrivateNoteCode;

    // ===== State: opaque blob owned by WASM =====
    // Header: next_order_id(16) + num_orders(4) = 20 bytes
    // Per order (126 bytes):
    //   orderId(16) + depositHash(32) + outcomeId(4) + isBuy(1) + flags(1)
    //   + priceBps(32) + amount(16) + minAmount(16) + epochId(8)

    /// @notice Opaque WASM-owned state blob encoded in big-endian layout.
    bytes _state;

    // Flags per orderId (for maker/taker fee classification at settlement)
    mapping(uint128 => uint8) _orderFlags;

    // Accumulated trading fees
    uint128 _totalMakerFees;
    uint128 _totalTakerFees;

    // ===== WASM constants =====

    /// @notice Content hash of WASM order book binary.
    bytes constant _wasm_hash = hex"96dc993f08ef70a77974558a103be25e0b0ee4c70bc2ceed53b1b264d908e4fa";
    /// @notice WASM module identifier passed to `gosh.runwasm`.
    string constant WASM_OB_MODULE = "docs:orderbook/orderbook-engine@0.1.0";
    /// @notice WASM entry function name.
    string constant WASM_OB_FUNCTION = "execute";
    /// @notice Optional embedded WASM binary payload (empty when module is resolved externally).
    bytes constant WASM_BINARY = "";

    // Action types
    /// @notice Action id for placing an order.
    uint8 constant ACTION_PLACE  = 1;
    /// @notice Action id for canceling an order.
    uint8 constant ACTION_CANCEL = 2;

    // Effect types
    /// @notice Effect id emitted by WASM when an order is accepted.
    uint8 constant EFFECT_ORDER_PLACED    = 1;
    /// @notice Effect id emitted by WASM when an order is canceled.
    uint8 constant EFFECT_ORDER_CANCELLED = 2;
    /// @notice Effect id emitted by WASM when an order is filled.
    uint8 constant EFFECT_ORDER_FILLED    = 3;

    // Order flags (mirrors WASM constants)
    /// @notice Immediate-or-cancel order flag.
    uint8 constant FLAG_IOC       = 0x01;
    /// @notice Fill-or-kill order flag.
    uint8 constant FLAG_FOK       = 0x02;
    /// @notice Market order flag.
    uint8 constant FLAG_MARKET    = 0x04;
    /// @notice Post-only (maker) order flag.
    uint8 constant FLAG_POST_ONLY = 0x08;
    /// @notice Good-till-cancelled (maker) order flag.
    uint8 constant FLAG_GTC       = 0x10;

    // Taker flag mask: IOC | FOK | MARKET
    uint8 constant TAKER_FLAGS_MASK = 0x07;

    // Events
    /// @notice Emitted when a new order is accepted by the matching engine.
    /// @param orderId Assigned order identifier.
    /// @param outcomeId Outcome identifier traded by the order.
    /// @param isBuy True for buy orders, false for sell orders.
    /// @param flags Bitmask of order execution flags.
    /// @param priceBps Limit price in basis points.
    /// @param amount Requested order amount.
    event OrderPlaced(uint128 orderId, uint32 outcomeId, bool isBuy, uint8 flags, uint256 priceBps, uint128 amount);

    /// @notice Emitted when an order is canceled.
    /// @param orderId Canceled order identifier.
    event OrderCancelled(uint128 orderId);

    /// @notice Emitted when an order receives a fill.
    /// @param orderId Filled order identifier.
    /// @param filledAmount Filled amount.
    /// @param clearingPrice Clearing price used for this fill in basis points.
    /// @param feeAmount Fee charged for this fill.
    /// @param isTaker True if the order was a taker order.
    event OrderFilled(uint128 orderId, uint128 filledAmount, uint256 clearingPrice, uint128 feeAmount, bool isTaker);

    // ===== Constructor =====

    /// @notice Initializes an OrderBook instance bound to a specific PMP market.
    /// @param pmpSaltedCodeHash Hash of salted PMP code.
    /// @param pmpSaltedCodeDepth Depth of salted PMP code cell.
    constructor(
        uint256 pmpSaltedCodeHash,
        uint16 pmpSaltedCodeDepth
    ) {
        tvm.accept();
        ensureBalance();

        TvmCell salt = abi.codeSalt(tvm.code()).get();
        (TvmCell PrivateNoteCode) = abi.decode(salt, (TvmCell));
        _PrivateNoteCode = PrivateNoteCode;

        require(msg.sender == DexLib.computePMPAddressFromHash(
            pmpSaltedCodeHash, pmpSaltedCodeDepth,
            _event_id, _oracle_list_hash, _token_type
        ), ERR_INVALID_SENDER);

        // Initialize empty state blob: next_order_id=1, num_orders=0
        bytes initState;
        initState.append(uint128ToBytes(1));
        initState.append(uint32ToBytes(0));
        _state = initState;
    }

    /// @notice Ensures minimal native balance for contract operation.
    function ensureBalance() private pure {
        if (address(this).balance > MIN_BALANCE) return;
        gosh.mintshellq(MIN_BALANCE);
    }

    // ===== Byte helpers (big-endian) =====

    /// @notice Converts `uint8` to big-endian bytes.
    /// @param value Value to encode.
    /// @return Encoded bytes representation.
    function uint8ToBytes(uint8 value) private pure returns (bytes) {
        return bytes(bytes1(value));
    }

    /// @notice Converts `uint32` to big-endian bytes.
    /// @param value Value to encode.
    /// @return Encoded bytes representation.
    function uint32ToBytes(uint32 value) private pure returns (bytes) {
        return bytes(bytes4(value));
    }

    /// @notice Converts `uint64` to big-endian bytes.
    /// @param value Value to encode.
    /// @return Encoded bytes representation.
    function uint64ToBytes(uint64 value) private pure returns (bytes) {
        return bytes(bytes8(value));
    }

    /// @notice Converts `uint128` to big-endian bytes.
    /// @param value Value to encode.
    /// @return Encoded bytes representation.
    function uint128ToBytes(uint128 value) private pure returns (bytes) {
        return bytes(bytes16(value));
    }

    /// @notice Converts `uint256` to big-endian bytes.
    /// @param value Value to encode.
    /// @return Encoded bytes representation.
    function uint256ToBytes(uint256 value) private pure returns (bytes) {
        return bytes(bytes32(value));
    }

    // ===== Single entry point =====

    /// @notice Executes a single order book action via WASM and dispatches produced effects.
    /// @param actionType Action type (`ACTION_PLACE`, `ACTION_CANCEL`, `ACTION_SETTLE`).
    /// @param outcomeId Outcome identifier (used for place action).
    /// @param isBuy Buy/sell side for place action.
    /// @param priceBps Limit price in basis points for place action.
    /// @param amount Order amount for place action.
    /// @param orderId Order identifier for cancel action.
    /// @param deposit_identifier_hash PrivateNote deposit hash used for sender checks and callbacks.
    /// @param flags Execution flags bitmask for place action.
    /// @param minAmount Minimum acceptable fill amount for place action.
    /// @param epochId Epoch identifier for place/settle actions.
    function execute(
        uint8   actionType,
        uint32  outcomeId,
        bool    isBuy,
        uint256 priceBps,
        uint128 amount,
        uint128 orderId,
        uint256 deposit_identifier_hash,
        uint8   flags,
        uint128 minAmount,
        uint64  epochId
    ) public {
        // Sender verification
        require(actionType == ACTION_PLACE || actionType == ACTION_CANCEL, ERR_INVALID_PARAMS);
        address wallet = DexLib.computePrivateNoteAddress(
            _PrivateNoteCode,
            deposit_identifier_hash
        );
        require(msg.sender == wallet, ERR_INVALID_SENDER);

        if (actionType == ACTION_PLACE) {
            require(amount >= minOrderAmount(_token_type), ERR_ORDER_TOO_SMALL);
        }

        tvm.accept();
        ensureBalance();

        // ── Build WASM input: [header][action params][state blob] ──
        bytes dataForWasm;
        dataForWasm.append(uint8ToBytes(actionType));
        dataForWasm.append(uint256ToBytes(deposit_identifier_hash));

        if (actionType == ACTION_PLACE) {
            dataForWasm.append(uint32ToBytes(outcomeId));
            dataForWasm.append(uint8ToBytes(isBuy ? 1 : 0));
            dataForWasm.append(uint8ToBytes(flags));
            dataForWasm.append(uint256ToBytes(priceBps));
            dataForWasm.append(uint128ToBytes(amount));
            dataForWasm.append(uint128ToBytes(minAmount));
            dataForWasm.append(uint64ToBytes(epochId));
        } else if (actionType == ACTION_CANCEL) {
            dataForWasm.append(uint128ToBytes(orderId));
        }

        dataForWasm.append(_state);

        // ── Call WASM ──
        TvmCell finalData = abi.encode(dataForWasm);
        TvmCell result = gosh.runwasm(
            abi.encode(_wasm_hash),
            finalData,
            abi.encode(WASM_OB_FUNCTION),
            abi.encode(WASM_OB_MODULE),
            abi.encode(WASM_BINARY)
        );
        bytes data = abi.decode(result, (bytes));

        // ── Parse result: [status:u8][stateLen:u32][newState...][numEffects:u32][effects...] ──
        uint8 status = uint8(data[0]);
        require(status == 0, ERR_INVALID_PARAMS);

        uint32 stateLen = _readUint32(data, 1);
        _state = _sliceBytes(data, 5, stateLen);
        uint32 effectsOffset = 5 + stateLen;

        // ── Parse and dispatch effects ──
        uint32 numEffects = _readUint32(data, effectsOffset);
        uint32 off = effectsOffset + 4;

        for (uint32 e = 0; e < numEffects; e++) {
            uint8 effectType = uint8(data[off]);

            if (effectType == EFFECT_ORDER_PLACED) {
                // [1][orderId:16] = 17 bytes
                uint128 eOrderId = _readUint128(data, off + 1);
                _orderFlags[eOrderId] = flags;
                address addrExtern = address.makeAddrExtern(OB_ORDER_PLACED, bitCntAddress);
                emit OrderPlaced{dest: addrExtern}(eOrderId, outcomeId, isBuy, flags, priceBps, amount);
                address wallet = DexLib.computePrivateNoteAddress(
                    _PrivateNoteCode, deposit_identifier_hash
                );
                PrivateNote(wallet).onOrderPlaced{
                    value: 0.1 vmshell, flag: 1
                }(_event_id, _oracle_list_hash, _token_type, eOrderId);
                off += 17;

            } else if (effectType == EFFECT_ORDER_CANCELLED) {
                // [2][pnHash:32][orderId:16][outcomeId:4][isBuy:1][returnAmount:16] = 70 bytes
                uint256 ePnHash       = _readUint256(data, off + 1);
                uint128 eOrderId      = _readUint128(data, off + 33);
                uint32 eOutcomeId     = _readUint32(data, off + 49);
                bool eIsBuy           = uint8(data[off + 53]) == 1;
                uint128 eReturnAmount = _readUint128(data, off + 54);
                delete _orderFlags[eOrderId];
                address addrExtern = address.makeAddrExtern(OB_ORDER_CANCELLED, bitCntAddress);
                emit OrderCancelled{dest: addrExtern}(eOrderId);
                address wallet = DexLib.computePrivateNoteAddress(
                    _PrivateNoteCode, ePnHash
                );
                PrivateNote(wallet).onOrderCancelled{
                    value: 0.1 vmshell, flag: 1
                }(_event_id, _oracle_list_hash, _token_type, eOrderId, eOutcomeId, eIsBuy, eReturnAmount);
                off += 70;

            } else if (effectType == EFFECT_ORDER_FILLED) {
                // [3][pnHash:32][orderId:16][outcomeId:4][filledAmt:16][clearingPrice:32][isBuy:1][buyerRefund:16]
                // = 118 bytes
                uint256 ePnHash        = _readUint256(data, off + 1);
                uint128 eOrderId       = _readUint128(data, off + 33);
                uint32 eOutcomeId      = _readUint32(data, off + 49);
                uint128 eFilledAmount  = _readUint128(data, off + 53);
                uint256 eClearingPrice = _readUint256(data, off + 69);
                bool eIsBuy            = uint8(data[off + 101]) == 1;
                uint128 eBuyerRefund   = _readUint128(data, off + 102);

                // Calculate maker/taker fee
                uint8 oFlags = _orderFlags[eOrderId];
                bool isTaker = (oFlags & TAKER_FLAGS_MASK) != 0;
                uint128 feeRate = isTaker ? TAKER_FEE_RATE : MAKER_FEE_RATE;
                uint128 notional = uint128(
                    (uint256(eFilledAmount) * uint256(eClearingPrice)) / uint256(FULL_PERCENT)
                );
                uint128 feeAmount = uint128(
                    (uint256(notional) * uint256(feeRate)) / uint256(FEE_DENOMINATOR)
                );
                if (isTaker) {
                    _totalTakerFees += feeAmount;
                } else {
                    _totalMakerFees += feeAmount;
                }
                // Do NOT delete _orderFlags here: a taker order can cross multiple makers
                // and needs its flags on every fill. Flags are deleted on EFFECT_ORDER_CANCELLED.

                address addrExtern = address.makeAddrExtern(OB_ORDER_FILLED, bitCntAddress);
                emit OrderFilled{dest: addrExtern}(eOrderId, eFilledAmount, eClearingPrice, feeAmount, isTaker);
                address wallet = DexLib.computePrivateNoteAddress(
                    _PrivateNoteCode, ePnHash
                );
                PrivateNote(wallet).onOrderFilled{
                    value: 0.1 vmshell, flag: 1
                }(_event_id, _oracle_list_hash, _token_type, eOutcomeId, eFilledAmount, eClearingPrice, eIsBuy, eBuyerRefund, feeAmount, eOrderId);
                off += 118;

            }
        }
    }

    // ===== Read helpers (big-endian) =====

    /// @notice Reads a big-endian `uint32` from a byte array.
    /// @param data Source byte array.
    /// @param offset Start offset in bytes.
    /// @return Parsed `uint32` value.
    function _readUint32(bytes data, uint32 offset) private pure returns (uint32) {
        return uint32(uint8(data[offset])) << 24
             | uint32(uint8(data[offset + 1])) << 16
             | uint32(uint8(data[offset + 2])) << 8
             | uint32(uint8(data[offset + 3]));
    }

    /// @notice Reads a big-endian `uint64` from a byte array.
    /// @param data Source byte array.
    /// @param offset Start offset in bytes.
    /// @return Parsed `uint64` value.
    function _readUint64(bytes data, uint32 offset) private pure returns (uint64) {
        uint64 result = 0;
        for (uint32 i = 0; i < 8; i++) {
            result = (result << 8) | uint64(uint8(data[offset + i]));
        }
        return result;
    }

    /// @notice Reads a big-endian `uint128` from a byte array.
    /// @param data Source byte array.
    /// @param offset Start offset in bytes.
    /// @return Parsed `uint128` value.
    function _readUint128(bytes data, uint32 offset) private pure returns (uint128) {
        uint128 result = 0;
        for (uint32 i = 0; i < 16; i++) {
            result = (result << 8) | uint128(uint8(data[offset + i]));
        }
        return result;
    }

    /// @notice Reads a big-endian `uint256` from a byte array.
    /// @param data Source byte array.
    /// @param offset Start offset in bytes.
    /// @return Parsed `uint256` value.
    function _readUint256(bytes data, uint32 offset) private pure returns (uint256) {
        uint256 result = 0;
        for (uint32 i = 0; i < 32; i++) {
            result = (result << 8) | uint256(uint8(data[offset + i]));
        }
        return result;
    }

    /// @notice Returns a slice of bytes from `data`.
    /// @param data Source byte array.
    /// @param offset Start offset.
    /// @param length Number of bytes to copy.
    /// @return Copied byte slice.
    function _sliceBytes(bytes data, uint32 offset, uint32 length) private pure returns (bytes) {
        bytes result;
        for (uint32 i = 0; i < length; i++) {
            result.append(bytes(bytes1(data[offset + i])));
        }
        return result;
    }

    // ===== Getters (parse _state on demand) =====

    /// @notice Returns static market ids and dynamic order book counters.
    /// @return event_id Bound market event identifier.
    /// @return oracle_list_hash Bound oracle list hash.
    /// @return token_type Bound token type.
    /// @return nextOrderId Next order identifier that will be assigned.
    /// @return orderCount Number of active orders currently stored.
    function getDetails() external view returns (
        uint256 event_id,
        uint256 oracle_list_hash,
        uint32  token_type,
        uint128 nextOrderId,
        uint128 orderCount,
        uint128 totalMakerFees,
        uint128 totalTakerFees
    ) {
        uint128 nextOid   = _readUint128(_state, 0);
        uint32  numOrders = _readUint32(_state, 16);
        return (
            _event_id,
            _oracle_list_hash,
            _token_type,
            nextOid,
            uint128(numOrders),
            _totalMakerFees,
            _totalTakerFees
        );
    }

    /// @notice Returns a single order by id.
    /// @param orderId Target order identifier.
    /// @return deposit_identifier_hash Owner PrivateNote deposit hash.
    /// @return outcomeId Outcome identifier.
    /// @return isBuy Buy/sell side.
    /// @return flags Execution flags.
    /// @return priceBps Limit price in basis points.
    /// @return amount Remaining order amount.
    /// @return minAmount Minimum acceptable fill amount.
    /// @return epochId Epoch identifier assigned to the order.
    function getOrder(uint128 orderId) external view returns (
        uint256 deposit_identifier_hash,
        uint32  outcomeId,
        bool    isBuy,
        uint8   flags,
        uint256 priceBps,
        uint128 amount,
        uint128 minAmount,
        uint64  epochId
    ) {
        uint32 numOrders = _readUint32(_state, 16);
        uint32 off = 20;
        for (uint32 i = 0; i < numOrders; i++) {
            uint128 oid = _readUint128(_state, off);
            if (oid == orderId) {
                return (
                    _readUint256(_state, off + 16),
                    _readUint32(_state,  off + 48),
                    uint8(_state[off + 52]) == 1,
                    uint8(_state[off + 53]),
                    _readUint256(_state, off + 54),
                    _readUint128(_state, off + 86),
                    _readUint128(_state, off + 102),
                    _readUint64(_state,  off + 118)
                );
            }
            off += 126;
        }
        revert(ERR_ORDER_NOT_FOUND);
    }

    /// @notice Returns contract version identifier.
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "OrderBook");
    }
}
