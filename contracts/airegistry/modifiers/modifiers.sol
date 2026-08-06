pragma gosh-solidity >=0.76.1;

import "./errors.sol";

abstract contract AiRegistryModifiers is AiRegistryErrors {
    uint64 constant MIN_BALANCE = 100 vmshell;

    // ECC currency id used for payments and fee burn.
    uint32 constant SHELL_ECC_ID = 2;

    uint16 constant BPS_DENOMINATOR = 10_000;

    // ── Protocol-wide constants (were per-ДОБ params; fixed by design) ──────
    // Platform fee: 2.5% (spec §5.1 PLATFORM_FEE_BPS), buyer-side, BY-FACT (on
    // delivered ticks), net-of-rebate burned (§5.4). NOT charged upfront.
    uint16 constant PLATFORM_FEE_BPS = 250;       // 2.5%
    // Seller rebate (§5.3): rate = min(REBATE_MAX_BPS, REBATE_SLOPE_BPS * n) bps, paid only on a
    // clean (never-disputed) close. REBATE_MAX_BPS is strictly below PLATFORM_FEE_BPS, so the net
    // burn stays positive however many ticks a deal delivers.
    uint16 constant REBATE_MAX_BPS   = 200;       // 2.0% cap
    uint16 constant REBATE_SLOPE_BPS = 4;         // bps per tick (cap at 50 ticks)
    // Probe phase (spec §3.1.2): the buyer's window to try the service before anything can be
    // claimed. Deliberately short — a buyer who finds a dead endpoint should stop and burn, not
    // wait out a long silence.
    uint64  constant PROBE_WINDOW    = 180;        // probe acceptance window (s)
    uint64  constant DISPUTE_WINDOW  = 600;        // dispute -> resolution timeout (s)
    uint64  constant MATCH_OPEN_TIMEOUT = 600;     // funded-but-unopened cleanup (no-show, §2.1)

    // ── Subscription / consumption accounting ───────────────────────────────────────────────
    uint64  constant SUB_WEEK_LEN = 604_800;       // one subscription week (s)
    // A subscription is ALWAYS one month — four weekly settlement periods. The term is not a
    // parameter: a single fixed length means the order carries no duration to validate, both
    // sides of a match agree on the term by construction, and there is exactly one shape of
    // subscription on the book instead of one per week count.
    uint8   constant SUB_WEEKS    = 4;             // one month, in weekly periods

    // Physical floor on generation: the fastest model in existence produces TICK_SIZE (1M) tokens
    // in ~58.8 s, so ONE TICK PER MINUTE is the ceiling nothing can beat. A claim of `d` ticks is
    // rejected unless `d * MIN_SECONDS_PER_TICK <= elapsed` — the seller cannot assert more output
    // than any hardware could have produced in the time that passed. Waiting longer widens THIS
    // bound and nothing else: a claim is also capped at `MAX_CLAIM_DELTA` — one tick per call,
    // whatever the elapsed time — so silence never buys the right to assert a batch. At these
    // values the per-call cap is the binding one, and a caller written against the elapsed-time
    // bound alone will have its claim rejected. Revisit if a materially faster model ships.
    uint64  constant MIN_SECONDS_PER_TICK = 60;    // (s per tick)
    // Promotion window for the newest claim. Two claims may stay pending at once (the seller may
    // run up to two ticks ahead), so the wait is two tick-times: after it, an unchallenged claim
    // becomes final even though no further claim arrived. Without this the LAST claim of a deal
    // could never be promoted — there is no successor to promote it — and would be unpayable.
    uint64  constant CLAIM_PROMOTE_WINDOW = 2 * MIN_SECONDS_PER_TICK;   // 120 s
    // Floor on the gap between two claims. It may sit BELOW the promotion window: each claim
    // carries its own timestamp and is promoted on its own window, so a fast claim rate never
    // shortens the contest time of the claim before it. That keeps the claimable rate at the
    // physical ceiling — one tick per minute — instead of halving it to fit the window.
    uint64  constant MIN_CLAIM_INTERVAL = MIN_SECONDS_PER_TICK;         // 60 s
    // Ticks one week can physically deliver at that rate cap. A subscription may not buy more
    // volume than its own term could ever produce, so the whole-term ceiling is SUB_WEEKS of these
    // — derived from the cap rather than written down, so the two can never drift apart.
    uint128 constant SUB_TICKS_PER_WEEK = uint128(SUB_WEEK_LEN / MIN_SECONDS_PER_TICK);   // 10 080

    // Forwarded value for child -> parent registration messages.
    varuint16 constant REGISTER_FORWARD_VALUE = 5 vmshell;

    // External address constants for directed events (off-chain subscribers).
    uint constant bitCntAddress = 256;
    uint128 constant RootRegisteredEmit          = 700;
    uint128 constant TokenContractRegisteredEmit = 702;
    uint128 constant ContractDeployedEmit        = 703;
    uint128 constant ContractDestroyedEmit       = 709;
    uint128 constant ShellWithdrawnEmit          = 710;
    // Streaming deal (spec §3-4)
    uint128 constant StreamFundedEmit            = 720;
    uint128 constant StreamOpenedEmit            = 721;
    uint128 constant TickFinalizedEmit           = 722;
    uint128 constant StreamStoppedEmit           = 723;
    uint128 constant StreamDisputedEmit          = 724;
    uint128 constant DisputeResolvedEmit         = 725;
    // Probe / seller bond (spec §3.1.2 / §4.2)
    uint128 constant SellerBondFundedEmit        = 727;
    uint128 constant ProbeAcceptedEmit           = 728;
    uint128 constant ProbeBurnedEmit             = 729;
    /// @notice External event id for `TokenContract.TicksClaimed`.
    /// @dev    It used to share `TickFinalizedEmit` (722) with `TickFinalized`, and this is the
    ///         quietest of the three collisions: both bodies are `(uint128, uint128)`, so a
    ///         positional decode SUCCEEDS and hands back two plausible numbers that mean different
    ///         things — `TicksClaimed(trusted, claimed)` counts ticks, `TickFinalized(finalizedOwed,
    ///         deposit)` counts money. The other two collisions break loudly; this one only ever
    ///         produced a wrong figure in somebody's report.
    uint128 constant TicksClaimedEmit            = 730;
    // InferenceOrderBook (spec §2 + §8) — dedicated 1000+ range (separate from registry/streaming/oracle 700s)
    uint128 constant OfferPlacedEmit             = 1000;
    uint128 constant OfferCancelledEmit          = 1001;
    uint128 constant BuyUnmatchedEmit            = 1002;
    uint128 constant MatchedEmit                 = 1003;
    uint128 constant ExecutedEmit                = 1004;
    // (736-738 were InferenceOracle — folded into InferenceOrderBook's
    //  daily-VWAP/weekly-median reference price; standalone oracle removed.)
    // InferenceOrderBook §8 — continue the 1000+ range
    // (1005-1007 were the weekly-cycle subscription events; a subscription is an ordinary order
    //  now, so it reports through the same OfferPlaced / Matched / OrderCancelled events.)
    uint128 constant InferenceOBDeployedEmit     = 1008;
    uint128 constant OfferCancelRejectedEmit     = 1009;
    uint128 constant OrderExpiredEmit            = 1010;
    /// @notice External event id for `InferenceOrderBook.InferenceOrderRejected`.
    /// @dev    It shared `OfferCancelRejectedEmit` (1009) with `InferenceOrderCancelRejected`.
    ///         Two refusals, but of different things and with different bodies —
    ///         `(uint8, address, address, uint128)` against `(uint128, uint8, address)`. 1009 stays
    ///         with the cancel rejection, whose name it matches; the placement rejection gets 1011.
    uint128 constant OrderRejectedEmit           = 1011;

    modifier accept() {
        tvm.accept();
        _;
    }

    modifier onlyOwnerPubkey(uint256 ownerPubkey) {
        require(msg.pubkey() == ownerPubkey, ERR_NOT_OWNER);
        _;
    }

    modifier senderIs(address sender) {
        require(msg.sender == sender, ERR_INVALID_SENDER);
        _;
    }
}
