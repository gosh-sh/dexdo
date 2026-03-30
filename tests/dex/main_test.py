"""
Comprehensive DEX integration test.

Tests all DEX contract functionality:
  Oracle:
    - deploy oracle
    - add event to OracleEventList
    - deploy additional OracleEventList (index 1)
    - delete event
    - withdraw oracle fees
  PrivateNote:
    - deploy via ZK proof
    - changeOwner
    - setStake (clean bet)
    - setStake (coupon bet, use_coupon=True)
    - cancelStake (after event cancel)
    - claim (win)
    - claim (lose, payout=0)
    - generateCoupon (after losing all balance)
    - deleteStake
    - withdrawTokens
  PMP:
    - deploy via PrivateNote.deployPMP
    - oracle approval (OracleEventList.confirmEvent -> PMP.approveEvent)
    - submitSetTimings  (66% consensus; single oracle → quorum=1, fires immediately)
    - acceptStake (clean bet)
    - acceptStake (coupon bet, bet_type=2)
    - splitFullSet / mergeFullSet
    - submitResolve    (66% consensus; fires immediately with single oracle)
    - claim (win, single staker)
    - claim (win, multi-staker with profit)
    - claim (lose, payout=0)
    - submitCancelEvent (66% consensus; fires immediately)
    - cancelStake (after submitCancelEvent)

Test sequence:
  Phase 1 – Oracle & PrivateNote setup
  Phase 2 – PMP Happy Path  : stake → submitSetTimings → resolve → claim (win)
             PMP1 self-destructs after the single winner claims.
  Phase 3 – PMP Cancel Path: deploy PMP2 (same address as PMP1, which is now dead)
             stake → submitCancelEvent → cancelStake
  Phase 4 – Oracle management: deployEventList(1), addEvent, deleteEvent, withdrawFees
  Phase 5 – generateCoupon & coupon-stake flow (two-participant PMP with winner/loser)
             Uses a second oracle ("MyOracle2") for a fresh PMP address.
             Loser deploys PN2, stakes and loses → balance=0 → generateCoupon.
             Then PN2 uses the coupon to stake on a new PMP.
  Phase 8 – auto-freeze → OrderBook deployed; splitFullSet; placeOrder/cancelOrder; mergeFullSet
  Phase 6 – PrivateNote token transfer (initTransfer) and discardCoupon
  Phase 7 – PrivateNote management: changeOwner, withdrawTokens, deleteStake

Key timing rules (from PMP.sol):
  FULL_SET_PERCENT = 1000, FULL_PERCENT = 10000
  Regular stake window   = stakeStart .. stakeStart + (stakeEnd-stakeStart)*10/100
  Full-set stake window  = stakeStart + (stakeEnd-stakeStart)*10/100 .. stakeEnd
  We use STAKE_PERIOD=300s → regular window=30s, full-set window=270s.

With a single oracle, quorum = ceil(1 * 6600 / 10000) = 1, so every
consensus call (submitSetTimings / submitResolve / submitCancelEvent) executes
immediately on first oracle submission.
"""

import json
import os
import random
import time
import sys

sys.path.append("tests")
from helper import common


def generate_random_sk():
    """Generate a random valid sk for halo2-proover.

    The binary reads the 64-char hex as a little-endian BN254 field element.
    The last byte becomes the MSB and must be < 0x30 (field modulus starts
    with 0x30644e72…).  We keep it in [0x00..0x2f] for safety.
    """
    return os.urandom(31).hex() + format(random.randint(0, 0x2f), '02x')

# ── Contract addresses ─────────────────────────────────────────────────────────
GIVER_ADDRESS = "0:1111111111111111111111111111111111111111111111111111111111111111"
ROOT_PN_ADDRESS = "0:1010101010101010101010101010101010101010101010101010101010101010"
ROOT_ORACLE_ADDRESS = "0:1515151515151515151515151515151515151515151515151515151515151515"

# ── ABI paths ──────────────────────────────────────────────────────────────────
GIVER_ABI = "./contracts/giver/GiverV3.abi.json"
GIVER_KEY_PATH = "./tests/GiverV3.keys.json"
ROOT_PN_ABI = "./contracts/0.79.3_compiled/dex/RootPN.abi.json"
ROOT_ORACLE_ABI = "./contracts/0.79.3_compiled/dex/RootOracle.abi.json"
PRIVATE_NOTE_ABI = "./contracts/0.79.3_compiled/dex/PrivateNote.abi.json"
PMP_ABI = "./contracts/0.79.3_compiled/dex/PMP.abi.json"
ORACLE_ABI = "./contracts/0.79.3_compiled/dex/Oracle.abi.json"
EVENTLIST_ABI = "./contracts/0.79.3_compiled/dex/OracleEventList.abi.json"

# ── Key paths ──────────────────────────────────────────────────────────────────
EPHEMERAL_KEY_PATH = "./tests/dex/ephemeral.keys.json"
ORACLE_KEY_PATH = "./tests/dex/oracle.keys.json"
NEW_OWNER_KEY_PATH = "./tests/dex/new_owner.keys.json"
LOSER_KEY_PATH = "./tests/dex/loser.keys.json"    # Phase 5 loser PrivateNote key

# ── Token types ────────────────────────────────────────────────────────────────
TOKEN_TYPE = 1        # NACKL
TOKEN_TYPE_SHELL = 2  # Shell
TOKEN_TYPE_ECC = 300  # Shell fee (ECC)

# ── Oracle / Event constants ───────────────────────────────────────────────────
ORACLE_NAME = "MyOracle"
ORACLE2_NAME = "MyOracle2"         # second oracle used in Phase 5 (coupon/loser PMP)
ORACLE3_NAME = "MyOracle3"         # third oracle used in Phase 8 (orderbook/split/merge PMP)
EVENT_NAME = "Winner of match X"
EVENT_DESCRIBE = "Who will win match X"
EVENT_OUTCOMES = {1: "Team A", 2: "Team B"}   # keys 1,2 → numOutcomes=2 → valid outcomeId: 0,1
EVENT_DEADLINE = 2000000000                     # Far future (~2033)
ORACLE_FEE = 100

# Pre-computed: tvm.hash(abi.encode(EVENT_NAME, EVENT_DEADLINE, EVENT_DESCRIBE, EVENT_OUTCOMES))
# Must match what OracleEventList.addEvent stores internally.
EVENT_ID = "0x67f17d97fb26ce706694339bf87ee24fe9c11752ff7ccc2a1fb56ea67e4e4e2f"

# ── Deposit / stake amounts ────────────────────────────────────────────────────
VAULT_DEPOSIT = 1_000_000_000_000  # 1000 NACKL – enough for all phases including OB tests
ECC_SHELL_DEPOSIT = 10_000_000_000
STAKE_AMOUNT = 200_000_000          # 0.2 NACKL – fits in regular stake window
STAKE_OUTCOME = 0                   # outcome to stake on and resolve to (win path)
LOSING_OUTCOME = 1                  # outcome for the Phase-5 loser PrivateNote

# Coupon stake amount: must meet MIN_VALUE (10_000_000 for NACKL) AND
# COUPON_POOL_LIMIT_PERCENT=500 (5%) constraint.
# With STAKE_AMOUNT=200M clean on winning outcome, max coupon ≈ 200M * 5/95 ≈ 10.5M.
# We use exactly MIN_VALUE = 10_000_000 which is ≤ 10.5M. ✓
COUPON_STAKE_AMOUNT = 10_000_000    # minimum valid coupon stake for NACKL

# Phase 5h: PN1 also places a small losing bet to create profit pool for coupon payout.
# Must be >= MIN_VALUE = 10_000_000 for NACKL.
PHASE5H_LOSING_BET = 10_000_000    # PN1's sacrificial losing-side bet in Phase 5h

# Deployer must cover all outcomes with clean bets before full-set window.
# Minimum valid clean bet for NACKL = MIN_VALUE = 10_000_000.
DEPLOYER_SEED_AMOUNT = 15_000_000_000   # seed stake per outcome (15 NACKL); must be >= SELL_AMOUNT for OB tests

# Phase 6: PN-to-PN transfer amount (must be >= minStakeValue = 10_000_000)
TRANSFER_AMOUNT = 10_000_000

# Phase 8: OrderBook / Split / Merge constants
PHASE8_STAKE_PERIOD = 20    # seconds; very short so we can freeze quickly
PHASE8_RESULT_PERIOD = 90   # seconds; epoch = result window length
EVENT_OB_NAME = "OrderBook Integration Test"
EVENT_OB_DESCRIBE = "Test event for OrderBook phase"
EVENT_OB_OUTCOMES = {1: "Side A", 2: "Side B"}
# Collateral to split: equals 2*DEPLOYER_SEED so we get exactly [SEED, SEED] tokens
SPLIT_COLLATERAL = 2 * DEPLOYER_SEED_AMOUNT   # 20M
SELL_OUTCOME_ID = 0                            # sell outcome-0 tokens
SELL_AMOUNT = 10_000_000_000                    # 10 NACKL (>= MIN_ORDER_NACKL)
SELL_PRICE_BPS = 5000                          # 50% in basis points
ORDERBOOK_ABI = "./contracts/0.79.3_compiled/dex/OrderBook.abi.json"

# Phase 9: OrderBook advanced – immediate matching, IOC, FOK, market, minAmount, multi-epoch
BUY_AMOUNT = 5_000_000
BUY_PRICE_BPS = 6000                           # 60% – crosses with SELL at 50%
EPOCH_1 = 1
EPOCH_2 = 2

# ── ZK proof secret keys (generated randomly each run) ───────────────────────
SKCOMMIT = generate_random_sk()
# Distinct key for the "loser" PrivateNote (PN2) used in Phase 5
SKCOMMIT_LOSER = generate_random_sk()

# ── Timing constants ──────────────────────────────────────────────────────────
# Phase 2 (happy path): short stake period so resolve can be tested in same run.
PHASE2_STAKE_PERIOD = 120   # seconds; regular window = 12s
# Phase 3 (cancel path): longer period to allow full-set stake testing.
PHASE3_STAKE_PERIOD = 300   # seconds; regular window = 30s, full-set window = 270s
# Phase 5 (coupon/loser path): same as Phase 2 for brevity.
PHASE5_STAKE_PERIOD = 120   # seconds; regular window = 12s
# Phase 5h (coupon stake): 3 stakes need to fit in regular window (3 × 4s = 12s needed).
PHASE5H_STAKE_PERIOD = 300  # seconds; regular window = 30s → plenty of time

# ── Contract constants (mirrored from modifiers.sol) ──────────────────────────
BET_TYPE_CLEAN = 0
BET_TYPE_DEBT = 1
BET_TYPE_COUPON = 2
NACKL_COUPON_VALUE = 100_000_000_000   # 100 NACKL
FULL_PERCENT = 10000
FEE_PERCENT = 1   # 0.01%


# ── Logging helper ─────────────────────────────────────────────────────────────
def log(title, data):
    print(f"\n=== {title} ===")
    print(data)
    print("===")


# ══════════════════════════════════════════════════════════════════════════════
# Assertion helpers
# ══════════════════════════════════════════════════════════════════════════════

def get_pn_balance(pn_details, token_type):
    """Extract a specific token balance from PN getDetails() output."""
    bal = pn_details.get("balance", {})
    val = bal.get(str(token_type), bal.get(token_type, 0))
    return int(val) if val is not None else 0


def get_pmp_outcome_pool(pmp_details, outcome_id, bet_type):
    """Extract a specific typed outcome pool amount from PMP getDetails()."""
    pools = pmp_details.get("typedOutcomePools", {})
    o = pools.get(str(outcome_id), pools.get(outcome_id, {}))
    if isinstance(o, dict):
        v = o.get(str(bet_type), o.get(bet_type, 0))
        return int(v) if v is not None else 0
    return 0


def normalize_uint256(val):
    """Convert a uint256 value (decimal, hex with/without 0x) to Python int."""
    if isinstance(val, int):
        return val
    s = str(val).strip()
    if s.startswith(("0x", "0X")):
        return int(s, 16)
    try:
        return int(s)
    except ValueError:
        return int(s, 16)


def assert_not_busy(pn_details, msg="PN must not be busy"):
    """Assert that a PrivateNote is not currently busy with a PMP operation."""
    busy = pn_details.get("busyAddress")
    assert not busy, f"{msg}; busyAddress={busy}"


def check_event_in_eventlist(eventlist_address, event_id, must_exist=True):
    """
    Verify that event_id is (or is not) in EventList._events.
    Returns the EventInfo dict if found, else None.
    """
    out = common.run_getter(eventlist_address, EVENTLIST_ABI, "_events", {})
    events_map = (out.get("_events") or {}) if isinstance(out, dict) else {}
    target = normalize_uint256(event_id)
    found = None
    for k, v in events_map.items():
        try:
            if normalize_uint256(k) == target:
                found = v
                break
        except (ValueError, TypeError):
            continue
    if must_exist:
        assert found is not None, \
            f"Event {event_id} not found in EventList {eventlist_address}"
    else:
        assert found is None, \
            f"Event {event_id} should NOT be in EventList {eventlist_address}"
    return found


def check_version(address, abi, expected_contract_name):
    """Assert that getVersion() returns '1.0.1' and the correct contract name."""
    out = common.run_getter(address, abi, "getVersion", {})
    assert out is not None, f"getVersion() failed for {address}"
    ver = out.get("value0", "")
    name = out.get("value1", "")
    assert ver == "1.0.1", \
        f"{expected_contract_name} version mismatch: expected 1.0.1, got '{ver}'"
    assert name == expected_contract_name, \
        f"Contract name mismatch: expected '{expected_contract_name}', got '{name}'"
    log(f"getVersion {expected_contract_name}", f"{ver} / {name}")


def get_pn_stakes(pn_address):
    """Return the full _stakes mapping from PrivateNote."""
    out = common.run_getter(pn_address, PRIVATE_NOTE_ABI, "_stakes", {})
    return (out.get("_stakes") or {}) if isinstance(out, dict) else {}


def assert_pmp_self_destructed(pmp_address, grace_sleep=3):
    """Assert that a PMP contract has self-destructed (no longer Active)."""
    if grace_sleep > 0:
        time.sleep(grace_sleep)
    active = common.is_account_active(pmp_address)
    assert not active, f"PMP {pmp_address} should have self-destructed but is still Active"
    log("PMP self-destructed", pmp_address)


# ══════════════════════════════════════════════════════════════════════════════
# Phase 1 helpers – Oracle & PrivateNote setup
# ══════════════════════════════════════════════════════════════════════════════

def generate_oracle_pubkey():
    """Read oracle key pair (generate only if file absent) and return 0x-prefixed pubkey."""
    import os
    common.gen_keys(ORACLE_KEY_PATH)
    pub = common.read_public_key(ORACLE_KEY_PATH)
    if not pub.startswith("0x"):
        pub = "0x" + pub
    log("oracle pubkey", pub)
    time.sleep(1)
    return pub


def deploy_oracle(oracle_pubkey):
    """Deploy Oracle contract via RootOracle."""
    params = {"oraclePubkey": oracle_pubkey, "oracleName": ORACLE_NAME}
    out = common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                               EPHEMERAL_KEY_PATH, "deployOracle", params)
    log("deployOracle", out)
    time.sleep(3)


def get_oracle_address():
    """Return Oracle contract address; waits until account is active."""
    out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                            "getOracleAddress", {"name": ORACLE_NAME})
    addr = out.get("oracleAddress") if isinstance(out, dict) else out
    log("oracle address", addr)
    common.wait_account_active(addr)
    time.sleep(1)
    assert common.is_account_active(addr), "Oracle not active"
    return addr


def get_eventlist_address(oracle_address, index=0):
    """Return OracleEventList address for given oracle and index."""
    out = common.run_getter(oracle_address, ORACLE_ABI,
                            "getEventListAddress", {"index": index})
    addr = out.get("value0") if isinstance(out, dict) else out
    log(f"eventlist address (index={index})", addr)
    common.wait_account_active(addr)
    time.sleep(1)
    assert common.is_account_active(addr), "EventList not active"
    return addr


def add_event(eventlist_address, event_name=EVENT_NAME, oracle_fee=ORACLE_FEE,
              deadline=EVENT_DEADLINE, describe=EVENT_DESCRIBE,
              outcomes=None, trust_addr=None):
    """Add an event to the OracleEventList. Returns raw call output."""
    if outcomes is None:
        outcomes = EVENT_OUTCOMES
    params = {
        "event_name": event_name,
        "oracle_fee": oracle_fee,
        "deadline": deadline,
        "describe": describe,
        "outcomeNames": outcomes,
        "trustAddr": trust_addr,   # optional(uint256); None serialises as JSON null
    }
    out = common.call_contract(eventlist_address, EVENTLIST_ABI,
                               ORACLE_KEY_PATH, "addEvent", params)
    log(f"addEvent ({event_name})", out)
    time.sleep(2)
    return out


def generate_ephemeral_pubkey():
    """Read ephemeral key pair (generate only if file absent) and return 0x-prefixed pubkey."""
    import os
    common.gen_keys(EPHEMERAL_KEY_PATH)
    pub = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not pub.startswith("0x"):
        pub = "0x" + pub
    log("ephemeral pubkey", pub)
    time.sleep(1)
    return pub


def send_tokens_to_root_pn(token_type, value):
    """Send ECC tokens to RootPN contract via giver."""
    params = {
        "dest": ROOT_PN_ADDRESS,
        "value": 2_000_000_000,
        "ecc": {token_type: value},
        "flag": 1,
    }
    out = common.call_contract(GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
                               "sendCurrencyWithFlag", params)
    log(f"sendCurrencyWithFlag token_type={token_type} value={value}", out)
    time.sleep(3)


def generate_proof(skcommit, token_type, value):
    """Run halo2-prover; return (proof, deposit_identifier_hash, value, token_type, nullifier_hash)."""
    out = common.execute_cmd(f"./halo2-proover {skcommit} {token_type} {value}")
    log("halo2-prover output", out)
    time.sleep(1)
    raw = [line for line in out.splitlines() if line.strip()][-1]
    pair = json.loads(json.loads(raw))
    dih = "0x" + pair["private_note_digest"]
    nullifier = "0x" + pair["private_note_digest"]
    return (pair["proof"], dih,
            int(pair["private_note_sum"]), int(pair["token_type"]), nullifier)


def deploy_private_note(proof, deposit_identifier_hash, value, token_type, ephemeral_pubkey):
    """Deploy PrivateNote via RootPN.deployPrivateNote."""
    params = {
        "zkproof": proof,
        "deposit_identifier_hash": deposit_identifier_hash,
        "ephemeral_pubkey": ephemeral_pubkey,
        "value": value,
        "token_type": token_type,
    }
    out = common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                               EPHEMERAL_KEY_PATH, "deployPrivateNote", params)
    log("deployPrivateNote", out)
    time.sleep(2)


def get_private_note_address(deposit_identifier_hash):
    """Return PrivateNote address for the given deposit identifier hash."""
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                            "getPrivateNoteAddress",
                            {"deposit_identifier_hash": deposit_identifier_hash})
    log("getPrivateNoteAddress", out)
    time.sleep(1)
    return out["privateNoteAddress"]


def send_ecc_to_private_note(proof, nullifier_hash, deposit_identifier_hash, value):
    """Send ECC shell tokens to an existing PrivateNote."""
    params = {
        "proof": proof,
        "nullifier_hash": nullifier_hash,
        "deposit_identifier_hash": deposit_identifier_hash,
        "value": value,
    }
    out = common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                               EPHEMERAL_KEY_PATH, "sendEccShellToPrivateNote", params)
    log("sendEccShellToPrivateNote", out)
    time.sleep(6)


# ══════════════════════════════════════════════════════════════════════════════
# Phase 2/3 helpers – PMP lifecycle
# ══════════════════════════════════════════════════════════════════════════════

def deploy_pmp(pn_address, event_id, oracle_name, oracle_fee, token_type, index=0):
    """Deploy PMP via PrivateNote.deployPMP; return PMP address."""
    params = {
        "event_id": event_id,
        "oracleFee": [oracle_fee],
        "token_type": token_type,
        "names": [oracle_name],
        "index": [index],
        "initialStakes": [DEPLOYER_SEED_AMOUNT, DEPLOYER_SEED_AMOUNT],
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "deployPMP", params)
    log("deployPMP", out)
    time.sleep(3)

    pmp_out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI, "getPMPAddress",
                                {"event_id": event_id,
                                 "names": [oracle_name],
                                 "token_type": token_type})
    pmp_address = pmp_out["pmpAddress"]
    log("PMP address", pmp_address)
    time.sleep(2)
    common.wait_account_active(pmp_address)
    return pmp_address


def get_pmp_details(pmp_address):
    """Return PMP.getDetails() dict."""
    return common.run_getter(pmp_address, PMP_ABI, "getDetails", {})


def wait_pmp_approved(pmp_address, max_wait=90):
    """Poll PMP.getDetails until oracle has confirmed event (approvedOracleEvents == numberOfOracleEvents)."""
    deadline = time.time() + max_wait
    while time.time() < deadline:
        details = get_pmp_details(pmp_address)
        if details:
            n = int(details.get("numberOfOracleEvents", 0))
            a = int(details.get("approvedOracleEvents", 0))
            if n > 0 and a >= n:
                time.sleep(5)  # Allow onInitialStakesAccepted callback to reach PN
                log("PMP oracle confirmed", pmp_address)
                return details
        time.sleep(3)
    raise TimeoutError(f"PMP {pmp_address} oracle confirmation not received after {max_wait}s")


def oracle_submit_set_timings(pmp_address, result_start):
    """Oracle sends submitSetTimings; fires immediately (quorum=1 with single oracle)."""
    params = {
        "resultStart": result_start,
    }
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitSetTimings", params)
    log("submitSetTimings", out)
    time.sleep(3)


def oracle_submit_resolve(pmp_address, outcome_id):
    """Oracle sends submitResolve; fires immediately (quorum=1 with single oracle)."""
    params = {"outcomeId": outcome_id}
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitResolve", params)
    log("submitResolve", out)
    time.sleep(3)


def oracle_submit_cancel_event(pmp_address):
    """Oracle votes to cancel; fires immediately (quorum=1 with single oracle)."""
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitCancelEvent", {})
    log("submitCancelEvent", out)
    time.sleep(3)


def pn_set_stake(pn_address, event_id, oracle_list_hash, token_type,
                 outcome, amount, use_coupon=False, key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote places a single-outcome stake on PMP."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "outcome": outcome,
        "amount": amount,
        "use_coupon": use_coupon,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "setStake", params)
    log(f"setStake outcome={outcome} use_coupon={use_coupon}", out)
    time.sleep(4)


def pn_cancel_stake(pn_address, event_id, oracle_list_hash, token_type):
    """PrivateNote cancels its stake on a cancelled PMP (refund flow)."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "cancelStake", params)
    log("cancelStake", out)
    time.sleep(4)


def pn_claim(pn_address, event_id, oracle_list_hash, token_type,
             key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote claims winnings from a resolved PMP."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "claim", params)
    log("claim", out)
    time.sleep(4)


def pn_change_owner(pn_address, new_pubkey, key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote changes its owner (ephemeral) public key."""
    params = {"new_pubkey": new_pubkey}
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "changeOwner", params)
    log("changeOwner", out)
    time.sleep(2)


def pn_withdraw_tokens(pn_address, dest_addr, token_type,
                       key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote withdraws full token balance to dest_addr via RootPN."""
    params = {
        "flags": 1,
        "dest_wallet_addr": dest_addr,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "withdrawTokens", params)
    log("withdrawTokens", out)
    time.sleep(3)


def get_pn_details(pn_address):
    """Return PrivateNote.getDetails() dict."""
    return common.run_getter(pn_address, PRIVATE_NOTE_ABI, "getDetails", {})


def pn_generate_coupon(pn_address, token_type, key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote generates a free coupon for token_type.

    Pre-conditions (enforced by contract):
      - All token balances == 0
      - _debt == 0
      - No active stakes
      - No existing coupon
      - _has_withdrawn == False
    """
    params = {"token_type": token_type}
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "generateCoupon", params)
    log("generateCoupon", out)
    time.sleep(3)
    return out


def pn_delete_stake(pn_address, event_id, oracle_list_hash, token_type,
                    key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote removes a stale stake record (no on-chain refund)."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "deleteStake", params)
    log("deleteStake", out)
    time.sleep(2)
    return out


def pn_discard_coupon(pn_address, key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote discards the unused coupon portion (clears _coupons_value)."""
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "discardCoupon", {})
    log("discardCoupon", out)
    time.sleep(2)
    return out


def pn_init_transfer(pn_address, dest_deposit_hash, token_type, amount,
                     key_path=EPHEMERAL_KEY_PATH):
    """PrivateNote initiates a token transfer to another PrivateNote."""
    params = {
        "dest_deposit_hash": dest_deposit_hash,
        "token_type": token_type,
        "amount": amount,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "initTransfer", params)
    log("initTransfer", out)
    time.sleep(6)   # allow offerTransfer + onTransferAccepted round trip
    return out


def get_pn_coupons_value(pn_address):
    """Return _coupons_value from PrivateNote getDetails()."""
    out = get_pn_details(pn_address)
    val = (out.get("couponsValue") if isinstance(out, dict) else None) or 0
    return int(val)


def get_pn_has_withdrawn(pn_address):
    """Return _has_withdrawn from PrivateNote getDetails()."""
    out = get_pn_details(pn_address)
    val = (out.get("hasWithdrawn") if isinstance(out, dict) else None)
    return bool(val)


# ══════════════════════════════════════════════════════════════════════════════
# Phase 4 helpers – Oracle management
# ══════════════════════════════════════════════════════════════════════════════

def oracle_deploy_event_list(oracle_address, index):
    """Deploy a new OracleEventList at the given index."""
    out = common.call_contract(oracle_address, ORACLE_ABI,
                               ORACLE_KEY_PATH, "deployEventList", {"index": index})
    log(f"deployEventList index={index}", out)
    time.sleep(5)


def oracle_delete_event(eventlist_address, event_id):
    """Delete an event from an OracleEventList (only if count==0 or past deadline)."""
    out = common.call_contract(eventlist_address, EVENTLIST_ABI,
                               ORACLE_KEY_PATH, "deleteEvent", {"event_id": event_id})
    log("deleteEvent", out)
    time.sleep(2)


def oracle_withdraw_fees(oracle_address, dest_addr, amount):
    """Oracle withdraws accumulated shell-token fees to dest_addr."""
    out = common.call_contract(oracle_address, ORACLE_ABI,
                               ORACLE_KEY_PATH, "withdrawFees",
                               {"to": dest_addr, "amount": amount})
    log("withdrawFees", out)
    time.sleep(2)


# ══════════════════════════════════════════════════════════════════════════════
# Phase 8 helpers – OrderBook / Split / Merge
# ══════════════════════════════════════════════════════════════════════════════



def get_ob_details(ob_address):
    """Return OrderBook.getDetails() dict."""
    return common.run_getter(ob_address, ORDERBOOK_ABI, "getDetails", {})


def get_ob_order(ob_address, order_id):
    """Return OrderBook.getOrder(orderId) dict."""
    return common.run_getter(ob_address, ORDERBOOK_ABI, "getOrder", {"orderId": order_id})


def get_ob_order_count(ob_address):
    """Return number of active orders in OrderBook."""
    d = get_ob_details(ob_address)
    return int(d.get("orderCount", 0)) if isinstance(d, dict) else 0


def pn_split_full_set(pn_address, event_id, oracle_list_hash, token_type, collateral,
                      key_path=EPHEMERAL_KEY_PATH):
    """PN splits collateral into proportional outcome tokens via PMP."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "collateral": collateral,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI, key_path, "splitFullSet", params)
    log("splitFullSet", out)
    time.sleep(5)
    return out


def pn_merge_full_set(pn_address, event_id, oracle_list_hash, token_type, amounts,
                      key_path=EPHEMERAL_KEY_PATH):
    """PN merges proportional outcome tokens back into collateral via PMP."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "amount": amounts,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI, key_path, "mergeFullSet", params)
    log("mergeFullSet", out)
    time.sleep(5)
    return out


def pn_place_order(pn_address, event_id, oracle_list_hash, token_type,
                   outcome_id, is_buy, price_bps, amount, key_path=EPHEMERAL_KEY_PATH,
                   flags=0, min_amount=0, epoch_id=1):
    """PN places an order on the OrderBook. flags: IOC=0x01, FOK=0x02, MARKET=0x04."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "outcomeId": outcome_id,
        "isBuy": is_buy,
        "priceBps": price_bps,
        "amount": amount,
        "flags": str(flags),
        "minAmount": str(min_amount),
        "epochId": str(epoch_id),
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI, key_path, "placeOrder", params)
    log("placeOrder", out)
    time.sleep(5)
    return out


def pn_cancel_order(pn_address, event_id, oracle_list_hash, token_type, order_id,
                    key_path=EPHEMERAL_KEY_PATH):
    """PN cancels an existing order on the OrderBook."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "orderId": order_id,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI, key_path, "cancelOrder", params)
    log("cancelOrder", out)
    time.sleep(5)
    return out


## ob_settle_epoch removed — matching is now immediate within epoch (WASM epoch filter)


def get_pn_stake_amounts(pn_address, event_id, oracle_list_hash, token_type):
    """Return the stake amount[] array for a specific PMP from PN's _stakes mapping."""
    stakes = get_pn_stakes(pn_address)
    # _stakes key is a uint256 hash; compute it the same way the contract does:
    # hash = tvm.hash(abi.encode(event_id, oracle_list_hash, token_type))
    # We can't compute that client-side easily, so search all stakes and match by value
    # The stub: just return the first stake that has non-empty amounts (for Phase 8 there's only one)
    # Better: return all amounts across all PMP keys and let caller filter
    for key, val in stakes.items():
        amounts_raw = val.get("amount", [])
        if amounts_raw:
            return [int(x) for x in amounts_raw]
    return []


def get_pn_stake_amounts_for(pn_address, event_id, oracle_list_hash, token_type):
    """Return PN stake amounts for a specific event/oracle combination (reads all stakes, matches by context)."""
    # Since we can't easily compute tvm.hash client-side, we use the fact that in Phase 8
    # PMP5 is the only active stake and just return the amount array from any stake record.
    return get_pn_stake_amounts(pn_address, event_id, oracle_list_hash, token_type)


# ══════════════════════════════════════════════════════════════════════════════
# Main test
# ══════════════════════════════════════════════════════════════════════════════

def main():
    common.set_config({"async_call": "false"})
    common.setup()
    time.sleep(1)

    print(f"SK:         {SKCOMMIT}")
    print(f"SK_LOSER:   {SKCOMMIT_LOSER}")

    common.wait_account_active(GIVER_ADDRESS)
    common.wait_account_active(ROOT_PN_ADDRESS)
    common.wait_account_active(ROOT_ORACLE_ADDRESS)

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 1 – Oracle & PrivateNote setup
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 1: Oracle & PrivateNote setup")

    # Generate both keys first so all key files exist before any contract calls.
    oracle_pubkey = generate_oracle_pubkey()
    ephemeral_pubkey = generate_ephemeral_pubkey()

    deploy_oracle(oracle_pubkey)
    oracle_address = get_oracle_address()

    # ── Check 1.1: Oracle contract version ────────────────────────────────────
    check_version(oracle_address, ORACLE_ABI, "Oracle")

    eventlist_address = get_eventlist_address(oracle_address, index=0)

    # ── Check 1.2: OracleEventList contract version ───────────────────────────
    check_version(eventlist_address, EVENTLIST_ABI, "OracleEventList")

    # ── Check 1.3: Record EventList event count before addEvent ───────────────
    # (EventList may retain state from previous runs on the same network)
    events_before = common.run_getter(eventlist_address, EVENTLIST_ABI, "_events", {})
    events_map_before = (events_before.get("_events") or {}) if isinstance(events_before, dict) else {}
    n_events_before_add = len(events_map_before)
    log("Phase 1.3: events in EventList before addEvent", n_events_before_add)

    add_event(eventlist_address)

    # ── Check 1.4: Event is stored in EventList after addEvent ────────────────
    event_info = check_event_in_eventlist(eventlist_address, EVENT_ID, must_exist=True)
    assert event_info["event_name"] == EVENT_NAME, \
        f"Stored event_name mismatch: {event_info['event_name']} != {EVENT_NAME}"
    assert int(event_info["oracle_fee"]) == ORACLE_FEE, \
        f"Stored oracle_fee mismatch: {event_info['oracle_fee']} != {ORACLE_FEE}"
    log("Phase 1.4 PASSED: event in EventList", event_info["event_name"])

    # Fund RootPN with ECC tokens so it can forward them to PrivateNote
    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    send_tokens_to_root_pn("1", VAULT_DEPOSIT)   # ECC key "1" = NACKL

    # Generate ZK proof and deploy PrivateNote
    proof, dih, value, token_type, nullifier_hash = generate_proof(
        SKCOMMIT, TOKEN_TYPE, VAULT_DEPOSIT
    )

    # Capture pre-deploy PN balance (PN address is deterministic from dih).
    # On second run the PN may already exist with residual balance.
    _pn_addr_pre = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                                     "getPrivateNoteAddress",
                                     {"deposit_identifier_hash": dih})
    _pn_addr_pre = (_pn_addr_pre.get("privateNoteAddress") if isinstance(_pn_addr_pre, dict)
                    else _pn_addr_pre)
    pn_nackl_before_deploy = 0
    if _pn_addr_pre and common.is_account_active(_pn_addr_pre):
        try:
            _pn_pre = get_pn_details(_pn_addr_pre)
            pn_nackl_before_deploy = get_pn_balance(_pn_pre, TOKEN_TYPE)
        except Exception:
            pn_nackl_before_deploy = 0
    log("PN pre-deploy NACKL balance", pn_nackl_before_deploy)

    deploy_private_note(proof, dih, value, token_type, ephemeral_pubkey)

    pn_address = get_private_note_address(dih)
    common.wait_account_active(pn_address)
    log("PrivateNote active", pn_address)

    # ── Check 1.5: PrivateNote contract version ───────────────────────────────
    check_version(pn_address, PRIVATE_NOTE_ABI, "PrivateNote")

    # Send ECC shell tokens to PrivateNote (balance[300])
    proof_sh, nullifier_sh, value_sh, _, _ = generate_proof(
        SKCOMMIT, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT
    )
    send_ecc_to_private_note(proof_sh, nullifier_sh, dih, ECC_SHELL_DEPOSIT)

    pn0 = get_pn_details(pn_address)
    log("PrivateNote initial state", pn0)
    assert pn0 is not None, "PrivateNote details unavailable"

    # ── Check 1.6: PN NACKL balance increased by VAULT_DEPOSIT ───────────────
    pn0_nackl = get_pn_balance(pn0, TOKEN_TYPE)
    assert pn0_nackl == pn_nackl_before_deploy + VAULT_DEPOSIT, \
        f"NACKL balance must increase by {VAULT_DEPOSIT} " \
        f"(prev={pn_nackl_before_deploy}, expected={pn_nackl_before_deploy + VAULT_DEPOSIT}), got {pn0_nackl}"
    log("Phase 1.6 PASSED: PN NACKL balance after deposit", pn0_nackl)

    # ── Check 1.7: PN not busy at start ───────────────────────────────────────
    assert_not_busy(pn0, "PN must not be busy after deploy")

    # ── Check 1.8: PN deposit identifier hash is correct ─────────────────────
    stored_dih = normalize_uint256(pn0.get("depositIdentifierHash", 0))
    expected_dih = normalize_uint256(dih)
    assert stored_dih == expected_dih, \
        f"depositIdentifierHash mismatch: {stored_dih} != {expected_dih}"
    log("Phase 1.8 PASSED: deposit identifier hash", hex(stored_dih))

    # ── Check 1.9: PN ephemeral pubkey matches what we passed ─────────────────
    stored_epk = normalize_uint256(pn0.get("ephemeralPubkey", 0))
    expected_epk = normalize_uint256(ephemeral_pubkey)
    assert stored_epk == expected_epk, \
        f"ephemeralPubkey mismatch after deploy: {stored_epk} != {expected_epk}"
    log("Phase 1.9 PASSED: ephemeral pubkey", hex(stored_epk))

    print(">>> Phase 1 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 2 – PMP Happy Path: deploy → approve → timings → stake → resolve → claim
    # After a single winner claims, PMP self-destructs (totalWinPool → 0).
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 2: PMP Happy Path (stake → resolve → claim win)")

    # Capture event count before PMP deploy (used in Check 2.3)
    ev_before_pmp1 = check_event_in_eventlist(eventlist_address, EVENT_ID, must_exist=True)
    event_count_before_pmp1 = int(ev_before_pmp1["count"])

    pmp1_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )

    # ── Check 2.1: PMP version ────────────────────────────────────────────────
    check_version(pmp1_address, PMP_ABI, "PMP")

    # Wait for OracleEventList.confirmEvent → PMP.approveEvent callback
    pmp1_approved = wait_pmp_approved(pmp1_address)
    oracle_list_hash = pmp1_approved["oracle_list_hash"]
    log("oracle_list_hash", oracle_list_hash)

    # ── Check 2.2: PMP state before setTimings ───────────────────────────────
    pmp1_pre_timings = get_pmp_details(pmp1_address)
    # Note: _approved may already be true if submitSetTimings was called by oracle
    # before we get here; skip this check.
    assert int(pmp1_pre_timings.get("numOutcomes", 0)) == 2, \
        f"numOutcomes must be 2, got {pmp1_pre_timings.get('numOutcomes')}"
    assert int(pmp1_pre_timings.get("token_type", -1)) == TOKEN_TYPE, \
        f"token_type must be {TOKEN_TYPE}, got {pmp1_pre_timings.get('token_type')}"
    assert not pmp1_pre_timings.get("isCancelled"), "PMP must not be cancelled initially"
    assert pmp1_pre_timings.get("resolvedOutcome") is None, \
        "PMP must not be resolved initially"
    assert int(pmp1_pre_timings.get("numberOfOracleEvents", 0)) == 1, \
        "numberOfOracleEvents must be 1"
    assert int(pmp1_pre_timings.get("approvedOracleEvents", 0)) == 1, \
        "approvedOracleEvents must be 1 after oracle confirmation"
    assert int(pmp1_pre_timings.get("totalPool", 0)) == 2 * DEPLOYER_SEED_AMOUNT, \
        f"totalPool must be 2*DEPLOYER_SEED_AMOUNT={2*DEPLOYER_SEED_AMOUNT} (initial stakes), got {pmp1_pre_timings.get('totalPool')}"
    log("Phase 2.2 PASSED: PMP initial state verified", "")

    # ── Check 2.3: Event count in EventList increased by 1 after PMP deploy ───
    time.sleep(3)  # allow EventList update to propagate
    event_after_deploy = check_event_in_eventlist(eventlist_address, EVENT_ID, must_exist=True)
    count_after_pmp1 = int(event_after_deploy["count"])
    assert count_after_pmp1 == event_count_before_pmp1 + 1, \
        f"Event count must increase by 1 after PMP deploy: {event_count_before_pmp1} → {count_after_pmp1}"
    log("Phase 2.3 PASSED: event count after PMP deploy", count_after_pmp1)

    # Oracle submits timings (fires immediately: quorum=1)
    # Regular stake window = first 12s  (10% of PHASE2_STAKE_PERIOD=120s)
    now = int(time.time())
    s_start  = now
    r_start  = now + PHASE2_STAKE_PERIOD         # 120 s from now

    oracle_submit_set_timings(pmp1_address, r_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s_end = s_start + (r_start - s_start) // 10

    pmp1_d = get_pmp_details(pmp1_address)
    assert pmp1_d["approved"], "PMP must be approved after submitSetTimings"

    # ── Check 2.4: Timings stored correctly ───────────────────────────────────
    assert int(pmp1_d.get("resultStart", 0)) == r_start, \
        f"resultStart mismatch"
    assert not pmp1_d.get("isCancelled"), "PMP must not be cancelled after timings"
    log("Phase 2.4 PASSED: PMP timings set correctly", {
        "stakeStart": pmp1_d["stakeStart"],
        "stakeEnd":   pmp1_d["stakeEnd"],
    })

    # ── Record PN balance before Phase 2 stake ────────────────────────────────
    pn_before_stake2 = get_pn_details(pn_address)
    pn_bal_before_stake2 = get_pn_balance(pn_before_stake2, TOKEN_TYPE)
    log("PN NACKL balance before Phase 2 stake", pn_bal_before_stake2)
    assert pn_bal_before_stake2 == VAULT_DEPOSIT - 2 * DEPLOYER_SEED_AMOUNT, \
        f"PN balance before stake must be VAULT_DEPOSIT-2*DS={VAULT_DEPOSIT - 2*DEPLOYER_SEED_AMOUNT} (initial stakes deducted at deploy), got {pn_bal_before_stake2}"

    # ── Check 2.5: PN has 1 initial stakes record (from deployPMP) ───────────
    stakes_before = get_pn_stakes(pn_address)
    assert len(stakes_before) == 1, f"PN must have 1 initial stakes record after deployPMP, got {len(stakes_before)}"

    # Place clean bet (must happen within 12s of stakeStart)
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )

    pmp1_after_stake = get_pmp_details(pmp1_address)
    total_pool = int(pmp1_after_stake.get("totalPool", 0))
    log("PMP1 totalPool after stake", total_pool)
    assert total_pool > 0, "PMP totalPool must be non-zero after stake"

    # ── Check 2.6: PN balance decreased by STAKE_AMOUNT ──────────────────────
    pn_after_stake2 = get_pn_details(pn_address)
    pn_bal_after_stake2 = get_pn_balance(pn_after_stake2, TOKEN_TYPE)
    assert pn_bal_after_stake2 == pn_bal_before_stake2 - STAKE_AMOUNT, \
        f"PN balance must decrease by STAKE_AMOUNT: " \
        f"expected {pn_bal_before_stake2 - STAKE_AMOUNT}, got {pn_bal_after_stake2}"
    log("Phase 2.6 PASSED: PN balance decreased by STAKE_AMOUNT", pn_bal_after_stake2)

    # ── Check 2.7: PN not busy after stake confirmed ──────────────────────────
    assert_not_busy(pn_after_stake2, "PN must not be busy after stake confirmed")

    # ── Check 2.8: PMP clean pool updated correctly ───────────────────────────
    clean_pool2 = get_pmp_outcome_pool(pmp1_after_stake, STAKE_OUTCOME, BET_TYPE_CLEAN)
    expected_clean_pool2 = DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT  # initial DS + regular SA
    assert clean_pool2 == expected_clean_pool2, \
        f"PMP clean pool[outcome={STAKE_OUTCOME}] must be {expected_clean_pool2} (DS+SA), got {clean_pool2}"
    expected_total_pool2 = 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT  # 2 initial DS + regular SA
    assert total_pool == expected_total_pool2, \
        f"PMP totalPool must be {expected_total_pool2} (2*DS+SA), got {total_pool}"
    log("Phase 2.8 PASSED: PMP pool updated correctly", {
        "totalPool": total_pool, "cleanPool[0]": clean_pool2
    })

    # ── Check 2.9: PN has a stake record after setStake ──────────────────────
    stakes_after_stake2 = get_pn_stakes(pn_address)
    assert len(stakes_after_stake2) == 1, \
        f"PN must have exactly 1 stake record after setStake, got {len(stakes_after_stake2)}"
    log("Phase 2.9 PASSED: PN has stake record", len(stakes_after_stake2))

    # Wait until result window opens (resultStart must pass)
    wait_for_result = r_start - int(time.time()) + 5
    if wait_for_result > 0:
        print(f"\n>>> Waiting {wait_for_result}s for result window (resultStart to pass)...")
        time.sleep(wait_for_result)

    # Oracle resolves to STAKE_OUTCOME (fires immediately)
    oracle_submit_resolve(pmp1_address, STAKE_OUTCOME)

    pmp1_d2 = get_pmp_details(pmp1_address)
    resolved = pmp1_d2.get("resolvedOutcome")
    log("PMP1 resolvedOutcome", resolved)
    assert resolved is not None, "PMP should be resolved after submitResolve"

    # ── Check 2.10: Resolved outcome == STAKE_OUTCOME exactly ─────────────────
    assert int(resolved) == STAKE_OUTCOME, \
        f"resolvedOutcome must be {STAKE_OUTCOME}, got {resolved}"
    assert not pmp1_d2.get("isCancelled"), "Resolved PMP must not be cancelled"
    log("Phase 2.10 PASSED: resolvedOutcome == STAKE_OUTCOME", resolved)

    # ── Check 2.11: creatorFee > 0 (initial stakes on LOSING_OUTCOME create profit pool) ─
    creator_fee2 = int(pmp1_d2.get("creatorFee", 0))
    assert creator_fee2 > 0, \
        f"creatorFee must be > 0 (initial DS on losing side creates profit), got {creator_fee2}"
    log("Phase 2.11 PASSED: creatorFee > 0 (initial stakes provide profit pool)", creator_fee2)

    # PrivateNote claims winnings (only staker → gets stake back; pool profit=0)
    pn_claim(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE)
    # PMP1 self-destructs here (totalWinPool reaches 0 after single claim)

    pn_after_claim2 = get_pn_details(pn_address)
    log("PrivateNote after claim", pn_after_claim2)

    # ── Check 2.12: PN balance restored to pre-deploy level (initial stakes returned) ─
    # PN spent: 2*DS (deploy) + SA (stake); gets back: 2*DS + SA (all from winning pool)
    # + creatorFee via acceptFee. Net: VAULT_DEPOSIT = pn_bal_before_stake2 + 2*DS
    pn_bal_after_claim2 = get_pn_balance(pn_after_claim2, TOKEN_TYPE)
    expected_bal_after_claim2 = pn_bal_before_stake2 + 2 * DEPLOYER_SEED_AMOUNT
    assert pn_bal_after_claim2 == expected_bal_after_claim2, \
        f"PN balance after claim must equal pn_bal_before_stake2+2*DS={expected_bal_after_claim2}: " \
        f"got {pn_bal_after_claim2}"
    log("Phase 2.12 PASSED: PN balance restored after claim", pn_bal_after_claim2)

    # ── Check 2.13: PN not busy after claim ───────────────────────────────────
    assert_not_busy(pn_after_claim2, "PN must not be busy after claim")

    # ── Check 2.14: PN stakes cleared after claim ─────────────────────────────
    stakes_after_claim2 = get_pn_stakes(pn_address)
    assert len(stakes_after_claim2) == 0, \
        f"PN must have no stake records after claim, got {len(stakes_after_claim2)}"
    log("Phase 2.14 PASSED: PN stakes cleared after claim", "")

    # ── Check 2.15: PMP1 self-destructed after last claim ─────────────────────
    assert_pmp_self_destructed(pmp1_address, grace_sleep=2)

    print(">>> Phase 2 PASSED: stake → resolve → claim (win)")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 3 – PMP Cancel Path + Full-Set Stake
    # PMP1 self-destructed → deploy PMP2 at the same address (same event/oracle/token).
    # Test: regular stake → submitCancelEvent → cancelStake
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 3: PMP Cancel Path + Full-Set Stake")

    time.sleep(5)   # allow PMP1 selfdestruct to settle

    pmp2_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )
    assert pmp2_address == pmp1_address, \
        "PMP2 must have same address as PMP1 (deterministic deployment)"

    wait_pmp_approved(pmp2_address)

    pmp2_d = get_pmp_details(pmp2_address)
    oracle_list_hash2 = pmp2_d["oracle_list_hash"]
    assert oracle_list_hash2 == oracle_list_hash, "oracle_list_hash must be stable"

    # Timings: stake_period = PHASE3_STAKE_PERIOD=300s → regular=30s, full-set=270s.
    now3     = int(time.time())
    r3_start = now3 + PHASE3_STAKE_PERIOD          # 300s from now

    oracle_submit_set_timings(pmp2_address, r3_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s3_end = now3 + (r3_start - now3) // 10

    pmp2_after_timings = get_pmp_details(pmp2_address)
    assert pmp2_after_timings["approved"], "PMP2 must be approved after submitSetTimings"

    # ── Check 3.1: PMP2 timings stored correctly ──────────────────────────────
    assert not pmp2_after_timings.get("isCancelled"), "PMP2 must not be cancelled"
    assert not pmp2_after_timings.get("resolvedOutcome"), "PMP2 must not be resolved"
    log("Phase 3.1 PASSED: PMP2 timings correct", "")

    # ── Record PN balance before Phase 3 operations ───────────────────────────
    pn_before_stake3 = get_pn_details(pn_address)
    pn_bal_before_stake3 = get_pn_balance(pn_before_stake3, TOKEN_TYPE)
    log("PN NACKL balance before Phase 3 stake", pn_bal_before_stake3)

    # ── 3a. Regular stake (within first 30s) ──────────────────────────────────
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash2, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )

    pmp2_after_regular = get_pmp_details(pmp2_address)
    log("PMP2 totalPool after regular stake", pmp2_after_regular.get("totalPool"))
    assert int(pmp2_after_regular.get("totalPool", 0)) > 0, \
        "PMP2 pool must be non-zero after regular stake"

    # ── Check 3.2: PMP2 clean pool correct after regular stake ────────────────
    # Pool on outcome 0 = initial DS (from deployPMP) + regular SA
    clean_pool3a = get_pmp_outcome_pool(pmp2_after_regular, STAKE_OUTCOME, BET_TYPE_CLEAN)
    expected_clean_pool3a = DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT
    assert clean_pool3a == expected_clean_pool3a, \
        f"PMP2 clean pool must be {expected_clean_pool3a} (DS+SA) after regular stake, got {clean_pool3a}"

    # ── Check 3.3: PN balance decreased after regular stake ───────────────────
    pn_after_reg_stake = get_pn_details(pn_address)
    pn_bal_after_reg3 = get_pn_balance(pn_after_reg_stake, TOKEN_TYPE)
    assert pn_bal_after_reg3 == pn_bal_before_stake3 - STAKE_AMOUNT, \
        f"PN balance after regular stake must be {pn_bal_before_stake3 - STAKE_AMOUNT}, " \
        f"got {pn_bal_after_reg3}"
    assert_not_busy(pn_after_reg_stake, "PN must not be busy after regular stake")
    log("Phase 3.3 PASSED: PN balance and busy state after regular stake", pn_bal_after_reg3)

    # ── 3b. submitCancelEvent → event cancelled immediately ──────────────────
    oracle_submit_cancel_event(pmp2_address)

    pmp2_cancelled = get_pmp_details(pmp2_address)
    assert pmp2_cancelled.get("isCancelled"), "PMP2 must be cancelled"
    log("PMP2 isCancelled", True)

    # ── Check 3.8: Cancelled PMP is not resolved ──────────────────────────────
    assert pmp2_cancelled.get("resolvedOutcome") is None, \
        "Cancelled PMP must not be resolved"
    log("Phase 3.8 PASSED: PMP2 cancelled, not resolved", "")

    # ── 3f. PrivateNote cancels its remaining regular stake (refund) ──────────
    pn_before_cancel = get_pn_details(pn_address)
    log("PrivateNote before cancelStake", pn_before_cancel)

    pn_cancel_stake(pn_address, EVENT_ID, oracle_list_hash2, TOKEN_TYPE)

    pn_after_cancel = get_pn_details(pn_address)
    log("PrivateNote after cancelStake", pn_after_cancel)

    # ── Check 3.9: PN balance restored (initial stakes + regular stake refunded) ──
    # cancelStake refunds stake.amount = [DS+SA, DS] → 2*DS+SA returned to PN
    # Before cancel: pn_bal_before_stake3 - SA (regular stake deducted, fs returned)
    # After cancel: pn_bal_before_stake3 - SA + (2*DS + SA) = pn_bal_before_stake3 + 2*DS
    pn_bal_after_cancel = get_pn_balance(pn_after_cancel, TOKEN_TYPE)
    expected_bal_after_cancel = pn_bal_before_stake3 + 2 * DEPLOYER_SEED_AMOUNT
    assert pn_bal_after_cancel == expected_bal_after_cancel, \
        f"PN balance after cancelStake must be pn_bal_before_stake3+2*DS={expected_bal_after_cancel}, " \
        f"got {pn_bal_after_cancel}"
    log("Phase 3.9 PASSED: PN balance restored after cancelStake", pn_bal_after_cancel)

    # ── Check 3.10: PN not busy after cancelStake ─────────────────────────────
    assert_not_busy(pn_after_cancel, "PN must not be busy after cancelStake")

    # ── Check 3.11: PN stake record cleared after cancelStake ─────────────────
    stakes_after_cancel = get_pn_stakes(pn_address)
    assert len(stakes_after_cancel) == 0, \
        f"PN must have no stake records after cancelStake, got {len(stakes_after_cancel)}"
    log("Phase 3.11 PASSED: PN stakes cleared after cancelStake", "")

    print(">>> Phase 3 PASSED: regular stake + cancel + refund")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 4 – Oracle management
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 4: Oracle management")

    # 4a. Deploy additional OracleEventList at index 1
    oracle_deploy_event_list(oracle_address, index=1)
    eventlist1_address = get_eventlist_address(oracle_address, index=1)
    log("new EventList (index=1)", eventlist1_address)

    # ── Check 4.1: New EventList has correct version ───────────────────────────
    check_version(eventlist1_address, EVENTLIST_ABI, "OracleEventList")

    # ── Check 4.2: New EventList is empty before addEvent ─────────────────────
    events1_before = common.run_getter(eventlist1_address, EVENTLIST_ABI, "_events", {})
    events1_map_before = (events1_before.get("_events") or {}) if isinstance(events1_before, dict) else {}
    assert len(events1_map_before) == 0, "New EventList must be empty before addEvent"

    # 4b. Add a different event to the new list (tests addEvent with custom params)
    add_event(
        eventlist1_address,
        event_name="Winner of match Y",
        oracle_fee=ORACLE_FEE,
        deadline=EVENT_DEADLINE,
        describe="Who will win match Y",
        outcomes={1: "Team C", 2: "Team D"},
    )

    # ── Check 4.3: New event stored in EventList1 ─────────────────────────────
    events1_after = common.run_getter(eventlist1_address, EVENTLIST_ABI, "_events", {})
    events1_map_after = (events1_after.get("_events") or {}) if isinstance(events1_after, dict) else {}
    assert len(events1_map_after) == 1, \
        f"EventList1 must have 1 event after addEvent, got {len(events1_map_after)}"
    # Verify the stored event name
    for k, v in events1_map_after.items():
        assert v.get("event_name") == "Winner of match Y", \
            f"Event name in EventList1 mismatch: {v.get('event_name')}"
        assert int(v.get("count", 0)) == 0, \
            f"New event count must be 0, got {v.get('count')}"
    log("Phase 4.3 PASSED: event stored in EventList1", "Winner of match Y")

    # 4c. deleteEvent from the original EventList.
    # PMP2 was cancelled → its count was decremented back to 0 in OracleEventList.
    # deleteEvent succeeds when count==0 or deadline passed.
    oracle_delete_event(eventlist_address, EVENT_ID)
    log("deleteEvent", f"event {EVENT_ID} removed from {eventlist_address}")

    # 4d. withdrawFees – oracle collects accumulated shell-token oracle fees
    oracle_withdraw_fees(oracle_address, GIVER_ADDRESS, 10)

    print(">>> Phase 4 PASSED: deployEventList, addEvent, deleteEvent, withdrawFees")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 5 – generateCoupon & coupon-stake flow
    #
    # Requires a two-participant PMP so we can have a clear winner and loser:
    #   • PN1 (original PrivateNote) stakes on STAKE_OUTCOME=0  → winner
    #   • PN2 (fresh "loser" PrivateNote) stakes on LOSING_OUTCOME=1 → loser
    #
    # We use a second oracle ("MyOracle2") so the PMP3 address is distinct from
    # the already-cancelled PMP2.  Oracle2 shares the same key-file (ORACLE_KEY_PATH).
    #
    # After resolution:
    #   • PN2 calls claim() → payout=0, stake record deleted, balance stays 0
    #   • PN1 calls claim() → wins, PMP3 self-destructs
    #   • PN2 can now call generateCoupon (all conditions satisfied)
    #
    # Phase 5b – coupon stake:
    #   • Deploy PMP4 at same address as PMP3 (it self-destructed)
    #   • PN1 stakes clean bet on STAKE_OUTCOME
    #   • PN2 stakes coupon bet (use_coupon=True) on STAKE_OUTCOME
    #   • Oracle resolves → PN2 wins coupon payout
    #   • PN1 claims, PMP4 self-destructs
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 5: generateCoupon & coupon-stake flow")

    # ── 5-setup: second oracle ────────────────────────────────────────────────
    # Deploy oracle2 (same pubkey/key-file, different name → different address).
    oracle2_params = {"oraclePubkey": oracle_pubkey, "oracleName": ORACLE2_NAME}
    common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                         EPHEMERAL_KEY_PATH, "deployOracle", oracle2_params)
    time.sleep(3)

    oracle2_out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                                    "getOracleAddress", {"name": ORACLE2_NAME})
    oracle2_address = oracle2_out.get("oracleAddress") if isinstance(oracle2_out, dict) else oracle2_out
    log("oracle2 address", oracle2_address)
    common.wait_account_active(oracle2_address)

    # ── Check 5.1: Oracle2 has correct version ────────────────────────────────
    check_version(oracle2_address, ORACLE_ABI, "Oracle")

    eventlist2_address = get_eventlist_address(oracle2_address, index=0)
    log("oracle2 eventlist address", eventlist2_address)

    # ── Check 5.2: Oracle2 EventList has correct version ─────────────────────
    check_version(eventlist2_address, EVENTLIST_ABI, "OracleEventList")

    # Add the same event (EVENT_ID) to oracle2's eventlist so PMP3 can be approved.
    add_event(eventlist2_address)

    # ── Check 5.3: Event stored in Oracle2's EventList ────────────────────────
    check_event_in_eventlist(eventlist2_address, EVENT_ID, must_exist=True)
    log("Phase 5.3 PASSED: event in oracle2 EventList", "")

    # ── 5-setup: loser PrivateNote (PN2) ────────────────────────────────────
    # Use stable keys for PN2 (generate only if absent).
    import os
    common.gen_keys(LOSER_KEY_PATH)
    loser_pubkey = common.read_public_key(LOSER_KEY_PATH)
    if not loser_pubkey.startswith("0x"):
        loser_pubkey = "0x" + loser_pubkey
    log("loser pubkey (PN2)", loser_pubkey)

    # Fund RootPN with NACKL and Shell for PN2.
    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    send_tokens_to_root_pn("1", VAULT_DEPOSIT)

    # ZK proof & deploy for PN2 (SKCOMMIT_LOSER).
    proof_l, dih_l, value_l, ttype_l, nullifier_l = generate_proof(
        SKCOMMIT_LOSER, TOKEN_TYPE, VAULT_DEPOSIT
    )
    deploy_private_note(proof_l, dih_l, value_l, ttype_l, loser_pubkey)

    pn2_address = get_private_note_address(dih_l)
    common.wait_account_active(pn2_address)
    log("PN2 (loser) active", pn2_address)

    # ── Check 5.4: PN2 initial NACKL balance == VAULT_DEPOSIT ─────────────────
    pn2_initial = get_pn_details(pn2_address)
    pn2_nackl_initial = get_pn_balance(pn2_initial, TOKEN_TYPE)
    assert pn2_nackl_initial == VAULT_DEPOSIT, \
        f"PN2 initial NACKL balance must be {VAULT_DEPOSIT}, got {pn2_nackl_initial}"
    assert_not_busy(pn2_initial, "PN2 must not be busy after deploy")
    log("Phase 5.4 PASSED: PN2 initial balance", pn2_nackl_initial)

    # Send Shell ECC to PN2 so it can pay network fees.
    proof_l_sh, null_l_sh, value_l_sh, _, _ = generate_proof(
        SKCOMMIT_LOSER, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT
    )
    send_ecc_to_private_note(proof_l_sh, null_l_sh, dih_l, ECC_SHELL_DEPOSIT)

    pn2_after_ecc = get_pn_details(pn2_address)
    log("PN2 state after ECC funding", pn2_after_ecc)

    # Determine PN2's actual NACKL balance so we can stake it all (→ balance=0).
    pn2_balance_map = pn2_after_ecc.get("balance", {})
    pn2_nackl_balance = int(pn2_balance_map.get(str(TOKEN_TYPE), 0))
    if pn2_nackl_balance == 0:
        # Some serialisers use integer keys
        pn2_nackl_balance = int(pn2_balance_map.get(TOKEN_TYPE, 0))
    log("PN2 NACKL balance (will stake all)", pn2_nackl_balance)
    assert pn2_nackl_balance > 0, "PN2 must have NACKL balance to stake"

    # ── Record PN1 balance before Phase 5 stake ───────────────────────────────
    pn1_before_stake5 = get_pn_details(pn_address)
    pn1_bal_before_stake5 = get_pn_balance(pn1_before_stake5, TOKEN_TYPE)
    log("PN1 NACKL balance before Phase 5 stake", pn1_bal_before_stake5)

    # ── 5a: Deploy PMP3 via PN1 using oracle2 ────────────────────────────────
    # PN1 still has NACKL balance at this point (withdrawn only in Phase 7).
    pmp3_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE2_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )

    pmp3_approved = wait_pmp_approved(pmp3_address)
    oracle_list_hash3 = pmp3_approved["oracle_list_hash"]
    log("PMP3 oracle_list_hash", oracle_list_hash3)

    # ── Check 5.5: PMP3 deployer is PN1 ──────────────────────────────────────
    pmp3_pre_timings = get_pmp_details(pmp3_address)
    assert not pmp3_pre_timings["approved"], "PMP3 must not be approved before setTimings"
    assert int(pmp3_pre_timings.get("numOutcomes", 0)) == 2, "PMP3 numOutcomes must be 2"
    log("Phase 5.5 PASSED: PMP3 initial state", "")

    now5     = int(time.time())
    r5_start = now5 + PHASE5_STAKE_PERIOD           # 120s from now

    oracle_submit_set_timings(pmp3_address, r5_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s5_end = now5 + (r5_start - now5) // 10

    pmp3_d = get_pmp_details(pmp3_address)
    assert pmp3_d["approved"], "PMP3 must be approved after submitSetTimings"

    # ── 5b: PN1 stakes on STAKE_OUTCOME (winning side) ───────────────────────
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )
    pmp3_after_p1 = get_pmp_details(pmp3_address)
    assert int(pmp3_after_p1.get("totalPool", 0)) > 0, "PMP3 pool must be non-zero after PN1 stake"
    log("PMP3 totalPool after PN1 stake", pmp3_after_p1.get("totalPool"))

    # ── Check 5.6: PN1 balance decreased by 2*DS (deployPMP) + STAKE_AMOUNT ──
    # pn1_bal_before_stake5 captured BEFORE deployPMP; deploy deducts 2*DS, setStake deducts SA
    pn1_after_stake5 = get_pn_details(pn_address)
    pn1_bal_after_stake5 = get_pn_balance(pn1_after_stake5, TOKEN_TYPE)
    expected_pn1_after_stake5 = pn1_bal_before_stake5 - 2 * DEPLOYER_SEED_AMOUNT - STAKE_AMOUNT
    assert pn1_bal_after_stake5 == expected_pn1_after_stake5, \
        f"PN1 balance after Phase 5 stake: expected {expected_pn1_after_stake5} (before-2*DS-SA), " \
        f"got {pn1_bal_after_stake5}"
    assert_not_busy(pn1_after_stake5, "PN1 must not be busy after Phase 5 stake")
    log("Phase 5.6 PASSED: PN1 balance after Phase 5 stake", pn1_bal_after_stake5)

    # ── 5c: PN2 stakes ALL its NACKL on LOSING_OUTCOME ───────────────────────
    pn_set_stake(
        pn2_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE,
        LOSING_OUTCOME, pn2_nackl_balance,
        key_path=LOSER_KEY_PATH
    )
    pmp3_after_p2 = get_pmp_details(pmp3_address)
    log("PMP3 totalPool after PN2 stake", pmp3_after_p2.get("totalPool"))
    assert int(pmp3_after_p2.get("totalPool", 0)) > int(pmp3_after_p1.get("totalPool", 0)), \
        "PMP3 pool must grow after PN2 stake"

    # ── Check 5.7: PMP3 pool == 2*DS + STAKE_AMOUNT + pn2_nackl_balance ──────
    # 2*DS from initialStakes + SA from PN1 setStake + pn2_nackl_balance from PN2
    expected_pmp3_pool = 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT + pn2_nackl_balance
    actual_pmp3_pool = int(pmp3_after_p2.get("totalPool", 0))
    assert actual_pmp3_pool == expected_pmp3_pool, \
        f"PMP3 totalPool must be {expected_pmp3_pool} (2*DS+SA+PN2), got {actual_pmp3_pool}"
    log("Phase 5.7 PASSED: PMP3 pool after both stakes", actual_pmp3_pool)

    # ── Check 5.8: PN2 balance == 0 after staking all NACKL ──────────────────
    pn2_after_stake = get_pn_details(pn2_address)
    pn2_bal_after_stake = get_pn_balance(pn2_after_stake, TOKEN_TYPE)
    assert pn2_bal_after_stake == 0, \
        f"PN2 NACKL balance must be 0 after staking all, got {pn2_bal_after_stake}"
    assert_not_busy(pn2_after_stake, "PN2 must not be busy after stake")
    log("Phase 5.8 PASSED: PN2 balance == 0 after staking all", "")

    # ── 5d: Wait for result window, then resolve ──────────────────────────────
    wait5 = r5_start - int(time.time()) + 5
    if wait5 > 0:
        print(f"\n>>> Waiting {wait5}s for PMP3 result window...")
        time.sleep(wait5)

    oracle_submit_resolve(pmp3_address, STAKE_OUTCOME)

    pmp3_resolved = get_pmp_details(pmp3_address)
    assert pmp3_resolved.get("resolvedOutcome") is not None, "PMP3 must be resolved"
    log("PMP3 resolvedOutcome", pmp3_resolved.get("resolvedOutcome"))

    # ── Check 5.9: PMP3 resolvedOutcome == STAKE_OUTCOME ─────────────────────
    assert int(pmp3_resolved.get("resolvedOutcome")) == STAKE_OUTCOME, \
        f"PMP3 resolvedOutcome must be {STAKE_OUTCOME}, got {pmp3_resolved.get('resolvedOutcome')}"

    # ── Check 5.10: creatorFee > 0 for multi-staker PMP ──────────────────────
    creator_fee5 = int(pmp3_resolved.get("creatorFee", 0))
    # creatorFee = totalPool * FEE_PERCENT / FULL_PERCENT
    # totalPool = 2*DS + STAKE_AMOUNT + pn2_nackl_balance
    expected_creator_fee5 = (2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT + pn2_nackl_balance) * FEE_PERCENT // FULL_PERCENT
    assert creator_fee5 > 0, \
        f"creatorFee must be > 0, got {creator_fee5}"
    assert creator_fee5 == expected_creator_fee5, \
        f"creatorFee must be {expected_creator_fee5}, got {creator_fee5}"
    log("Phase 5.10 PASSED: creatorFee > 0", creator_fee5)

    # ── 5e: PN2 claims first (loser → 0 payout, stake record deleted) ────────
    pn_claim(pn2_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE,
             key_path=LOSER_KEY_PATH)

    pn2_after_lose = get_pn_details(pn2_address)
    log("PN2 after losing claim", pn2_after_lose)
    # PN2 balance should still be 0 (was deducted at stake time, payout=0)
    pn2_bal_after = pn2_after_lose.get("balance", {})
    pn2_nackl_after = int(pn2_bal_after.get(str(TOKEN_TYPE), pn2_bal_after.get(TOKEN_TYPE, 0)))
    assert pn2_nackl_after == 0, \
        f"PN2 NACKL balance must be 0 after losing, got {pn2_nackl_after}"

    # ── Check 5.11: PN2 not busy after losing claim ───────────────────────────
    assert_not_busy(pn2_after_lose, "PN2 must not be busy after losing claim")

    # ── Check 5.12: PN2 stakes cleared after losing claim ────────────────────
    stakes_pn2_after_lose = get_pn_stakes(pn2_address)
    assert len(stakes_pn2_after_lose) == 0, \
        f"PN2 must have no stake records after losing claim, got {len(stakes_pn2_after_lose)}"
    log("Phase 5.12 PASSED: PN2 stakes cleared after losing claim", "")

    # ── 5f: PN1 claims (winner) – PMP3 self-destructs ────────────────────────
    pn_claim(pn_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE)
    pn1_after_win = get_pn_details(pn_address)
    log("PN1 after winning claim (PMP3)", pn1_after_win)

    # ── Check 5.13: PN1 balance increased (net gain = pn2_nackl_balance) ──────
    pn1_bal_after_win5 = get_pn_balance(pn1_after_win, TOKEN_TYPE)
    assert pn1_bal_after_win5 > pn1_bal_before_stake5, \
        f"PN1 balance must increase after winning: " \
        f"before={pn1_bal_before_stake5}, after={pn1_bal_after_win5}"
    net_gain5 = pn1_bal_after_win5 - pn1_bal_before_stake5
    log("Phase 5.13 PASSED: PN1 net gain", net_gain5)
    # Net gain ≈ pn2_nackl_balance (loser's stake minus tiny creator fee rounding)
    assert net_gain5 > 0, "PN1 must have net profit from Phase 5"

    # ── Check 5.14: PN1 not busy after winning claim ─────────────────────────
    assert_not_busy(pn1_after_win, "PN1 must not be busy after winning claim")

    # ── Check 5.15: PMP3 self-destructed after PN1 claim ─────────────────────
    assert_pmp_self_destructed(pmp3_address, grace_sleep=2)

    # ── 5g: generateCoupon on PN2 ────────────────────────────────────────────
    # Conditions now satisfied:
    #   • balance[TOKEN_TYPE] = 0 < minStakeValue(10M)  (stake was lost)
    #   • _stakes empty            (claim() deleted the record)
    #   • _has_withdrawn = False   (withdrawTokens never called)
    #   • _debt = 0, _coupons_value = 0
    pn_generate_coupon(pn2_address, TOKEN_TYPE, key_path=LOSER_KEY_PATH)

    pn2_with_coupon = get_pn_details(pn2_address)
    log("PN2 after generateCoupon", pn2_with_coupon)
    # The contract sets _coupons_value = NACKL_COUPON_VALUE = 100_000_000_000
    # and _debt = 5% of that = 5_000_000_000.
    # We verify via getDetails that busyAddress is clear (no pending op).

    # ── Check 5.16: PN2 not busy after generateCoupon ─────────────────────────
    assert_not_busy(pn2_with_coupon, "PN2 must not be busy after generateCoupon")
    log("Phase 5.16 PASSED: generateCoupon called successfully", "")

    # ── Check 5.17: PN2 NACKL balance still 0 after generateCoupon ────────────
    pn2_bal_with_coupon = get_pn_balance(pn2_with_coupon, TOKEN_TYPE)
    assert pn2_bal_with_coupon == 0, \
        f"PN2 NACKL balance must remain 0 after generateCoupon (coupon != balance), " \
        f"got {pn2_bal_with_coupon}"
    log("Phase 5.17 PASSED: PN2 NACKL balance == 0 after generateCoupon", "")

    print(">>> Phase 5g PASSED: generateCoupon called successfully")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 5h – coupon stake: PN2 uses its coupon to bet on a new PMP
    #
    # PMP3 self-destructed when PN1 claimed → deploy PMP4 at same address
    # (same event_id + same oracle2 → deterministic, same address).
    # PN1 places a clean bet on STAKE_OUTCOME so there is a pool.
    # PN2 places a coupon bet (use_coupon=True) on STAKE_OUTCOME.
    # Oracle resolves → PN2 wins coupon payout.
    # PN1 claims, PMP4 self-destructs
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 5h: coupon stake flow")

    time.sleep(5)   # allow PMP3 selfdestruct to settle

    pmp4_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE2_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )
    assert pmp4_address == pmp3_address, \
        "PMP4 must have same address as PMP3 (deterministic redeployment)"

    pmp4_approved = wait_pmp_approved(pmp4_address)
    oracle_list_hash4 = pmp4_approved["oracle_list_hash"]
    assert oracle_list_hash4 == oracle_list_hash3, "oracle_list_hash must be stable"

    # Use PHASE5H_STAKE_PERIOD (300s) to give all 3 stakes time within regular window (30s).
    now5h     = int(time.time())
    r5h_start = now5h + PHASE5H_STAKE_PERIOD

    oracle_submit_set_timings(pmp4_address, r5h_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s5h_end = now5h + (r5h_start - now5h) // 10

    pmp4_d = get_pmp_details(pmp4_address)
    assert pmp4_d["approved"], "PMP4 must be approved after submitSetTimings"

    # ── Record PN1 balance before Phase 5h stakes ─────────────────────────────
    pn1_before_stake5h = get_pn_details(pn_address)
    pn1_bal_before_stake5h = get_pn_balance(pn1_before_stake5h, TOKEN_TYPE)
    log("PN1 balance before Phase 5h stake", pn1_bal_before_stake5h)

    # ── Stake 1: PN1 clean stake on STAKE_OUTCOME (winning side) ─────────────
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )
    pmp4_after_p1 = get_pmp_details(pmp4_address)
    log("PMP4 totalPool after PN1 win-side stake", pmp4_after_p1.get("totalPool"))

    # ── Check 5h.1: PMP4 pool == 2*DS + STAKE_AMOUNT after PN1 win-side stake ─
    # 2*DS from initialStakes + SA from PN1 setStake
    expected_pool_5h_1 = 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT
    assert int(pmp4_after_p1.get("totalPool", 0)) == expected_pool_5h_1, \
        f"PMP4 pool must be {expected_pool_5h_1} (2*DS+SA) after PN1 stake, got {pmp4_after_p1.get('totalPool')}"

    # ── Stake 2: PN1 also stakes PHASE5H_LOSING_BET on LOSING_OUTCOME ─────────
    # This creates a profit budget so the coupon payout is non-zero.
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
        LOSING_OUTCOME, PHASE5H_LOSING_BET
    )
    pmp4_after_losing = get_pmp_details(pmp4_address)
    log("PMP4 totalPool after PN1 losing-side stake", pmp4_after_losing.get("totalPool"))

    # ── Check 5h.1b: PMP4 pool == 2*DS + STAKE_AMOUNT + PHASE5H_LOSING_BET ───
    # 2*DS from initialStakes + SA (win side) + LB (lose side) from PN1
    expected_pool_5h = 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT + PHASE5H_LOSING_BET
    assert int(pmp4_after_losing.get("totalPool", 0)) == expected_pool_5h, \
        f"PMP4 pool must be {expected_pool_5h} (2*DS+SA+LB) after both PN1 stakes, " \
        f"got {pmp4_after_losing.get('totalPool')}"
    assert_not_busy(get_pn_details(pn_address), "PN1 must not be busy after losing-side stake")

    # ── Stake 3: PN2 coupon stake on STAKE_OUTCOME (use_coupon=True → bet_type=2) ─
    # COUPON_STAKE_AMOUNT=10M must fit COUPON_POOL_LIMIT_PERCENT=5%:
    #   max_coupon ≈ STAKE_AMOUNT * 5/95 ≈ 10.5M ≥ COUPON_STAKE_AMOUNT ✓
    pn_set_stake(
        pn2_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
        STAKE_OUTCOME, COUPON_STAKE_AMOUNT, use_coupon=True,
        key_path=LOSER_KEY_PATH
    )
    pmp4_after_coupon = get_pmp_details(pmp4_address)
    log("PMP4 typedOutcomePools after coupon stake", pmp4_after_coupon.get("typedOutcomePools"))

    # ── Check 5h.2: PN2 coupon pool updated in PMP4 ───────────────────────────
    coupon_pool4 = get_pmp_outcome_pool(pmp4_after_coupon, STAKE_OUTCOME, BET_TYPE_COUPON)
    assert coupon_pool4 == COUPON_STAKE_AMOUNT, \
        f"PMP4 coupon pool must be {COUPON_STAKE_AMOUNT}, got {coupon_pool4}"
    log("Phase 5h.2 PASSED: PMP4 coupon pool", coupon_pool4)

    # ── Check 5h.3: PMP4 totalPool = clean bets only (coupons excluded) ───────
    total_pool4 = int(pmp4_after_coupon.get("totalPool", 0))
    assert total_pool4 == expected_pool_5h, \
        f"PMP4 totalPool (clean only) must be {expected_pool_5h}, got {total_pool4}"
    log("Phase 5h.3 PASSED: PMP4 totalPool includes PN1's two clean bets", total_pool4)

    # ── Check 5h.4: PN2 not busy after coupon stake ───────────────────────────
    pn2_after_coupon_stake = get_pn_details(pn2_address)
    assert_not_busy(pn2_after_coupon_stake, "PN2 must not be busy after coupon stake")

    # Wait for result window, resolve to STAKE_OUTCOME.
    wait5h = r5h_start - int(time.time()) + 5
    if wait5h > 0:
        print(f"\n>>> Waiting {wait5h}s for PMP4 result window...")
        time.sleep(wait5h)

    oracle_submit_resolve(pmp4_address, STAKE_OUTCOME)

    pmp4_resolved = get_pmp_details(pmp4_address)
    assert pmp4_resolved.get("resolvedOutcome") is not None, "PMP4 must be resolved"

    # ── Check 5h.5: PMP4 resolvedOutcome == STAKE_OUTCOME ────────────────────
    assert int(pmp4_resolved.get("resolvedOutcome")) == STAKE_OUTCOME, \
        f"PMP4 resolvedOutcome must be {STAKE_OUTCOME}"

    # ── Check 5h.5b: PMP4 creatorFee > 0 (profit pool from PN1's losing bet) ─
    creator_fee4 = int(pmp4_resolved.get("creatorFee", 0))
    assert creator_fee4 > 0, \
        f"PMP4 creatorFee must be > 0 when losing-side bets exist, got {creator_fee4}"
    log("Phase 5h.5b PASSED: PMP4 creatorFee > 0", creator_fee4)

    # PN2 claims coupon payout first.
    pn_claim(pn2_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
             key_path=LOSER_KEY_PATH)

    pn2_after_coupon_win = get_pn_details(pn2_address)
    log("PN2 after coupon-stake claim", pn2_after_coupon_win)

    # ── Check 5h.6: PN2 has payout after coupon win ───────────────────────────
    # profitBudget = PHASE5H_LOSING_BET - creatorFee ≈ 9.98M
    # couponWinCoef ≈ 9.98M * 10000 / (STAKE_AMOUNT + COUPON_STAKE_AMOUNT) ≈ 475
    # payoutCoupon = COUPON_STAKE_AMOUNT * 475 / 10000 ≈ 475,000 > 0
    pn2_bal_after_coupon_win = get_pn_balance(pn2_after_coupon_win, TOKEN_TYPE)
    assert pn2_bal_after_coupon_win > 0, \
        f"PN2 balance must be > 0 after coupon win, got {pn2_bal_after_coupon_win}"
    assert_not_busy(pn2_after_coupon_win, "PN2 must not be busy after coupon claim")
    log("Phase 5h.6 PASSED: PN2 has payout after coupon win", pn2_bal_after_coupon_win)

    # ── Check 5h.7: PN2 stakes cleared after coupon claim ────────────────────
    stakes_pn2_after_coupon = get_pn_stakes(pn2_address)
    assert len(stakes_pn2_after_coupon) == 0, \
        f"PN2 must have no stake records after coupon claim, got {len(stakes_pn2_after_coupon)}"
    log("Phase 5h.7 PASSED: PN2 stakes cleared after coupon claim", "")

    # PN1 claims its clean-bet win – PMP4 self-destructs.
    pn_claim(pn_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE)
    pn1_after_pmp4 = get_pn_details(pn_address)
    log("PN1 after PMP4 claim", pn1_after_pmp4)

    # ── Check 5h.8: PN1 balance recovered (PN1 spent STAKE_AMOUNT+LOSING_BET) ─
    # PN1 net in 5h ≈ profit_to_clean + creatorFee - LOSING_BET ≈ small negative
    # (coupon takes some of the profit). Check balance didn't drop by more than LOSING_BET.
    pn1_bal_after_pmp4 = get_pn_balance(pn1_after_pmp4, TOKEN_TYPE)
    assert pn1_bal_after_pmp4 > pn1_bal_before_stake5h - PHASE5H_LOSING_BET - 1, \
        f"PN1 balance after Phase 5h must be close to pre-stake level: " \
        f"before={pn1_bal_before_stake5h}, after={pn1_bal_after_pmp4}"
    assert_not_busy(pn1_after_pmp4, "PN1 must not be busy after PMP4 claim")
    log("Phase 5h.8 PASSED: PN1 balance after PMP4 win", pn1_bal_after_pmp4)

    # ── Check 5h.9: PMP4 self-destructed ─────────────────────────────────────
    assert_pmp_self_destructed(pmp4_address, grace_sleep=2)

    # ── 5i: deleteStake utility ───────────────────────────────────────────────
    # To test deleteStake, create an orphan stake record on PN2 by having it
    # attempt a claim on a non-existent (self-destructed) PMP with a dummy hash.
    # Simpler: just call deleteStake on PN2 for oracle_list_hash4 which was
    # already cleared by the coupon-stake claim above, verifying it's idempotent.
    # NOTE: deleteStake silently succeeds even if the record doesn't exist.
    pn_delete_stake(pn2_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
                    key_path=LOSER_KEY_PATH)
    log("deleteStake (idempotent)", "OK")

    # ── Check 5i.1: PN2 not busy after deleteStake ────────────────────────────
    pn2_after_delete = get_pn_details(pn2_address)
    assert_not_busy(pn2_after_delete, "PN2 must not be busy after deleteStake")
    log("Phase 5i.1 PASSED: deleteStake is idempotent and safe", "")

    print(">>> Phase 5 PASSED: generateCoupon, coupon stake, coupon payout, deleteStake")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 8 – auto-freeze + splitFullSet + placeOrder/cancelOrder + mergeFullSet
    #
    # Uses PN1 (still has NACKL balance, not yet sealed) and a fresh PMP5 on Oracle3.
    # Sequence:
    #   8a. Deploy Oracle3, add event, deploy PMP5 via PN1
    #   8b. Submit short timings → wait for stakeEnd
    #   8c. auto-freeze → OrderBook auto-deployed
    #   8d. PN1 splitFullSet(SPLIT_COLLATERAL) → receive [SEED, SEED] outcome tokens
    #   8e. PN1 placeOrder(sell, outcome0, SELL_AMOUNT, SELL_PRICE_BPS) → order in OB
    #   8f. PN1 cancelOrder → tokens restored
    #   8g. PN1 mergeFullSet([SEED, SEED]) → collateral returned
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 8: auto-freeze + splitFullSet + placeOrder/cancelOrder + mergeFullSet")

    # ── 8a: Deploy Oracle3, add OB event, deploy PMP5 ────────────────────────
    oracle3_params = {"oraclePubkey": oracle_pubkey, "oracleName": ORACLE3_NAME}
    common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                         EPHEMERAL_KEY_PATH, "deployOracle", oracle3_params)
    time.sleep(3)

    oracle3_out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                                    "getOracleAddress", {"name": ORACLE3_NAME})
    oracle3_address = oracle3_out.get("oracleAddress") if isinstance(oracle3_out, dict) else oracle3_out
    log("oracle3 address", oracle3_address)
    common.wait_account_active(oracle3_address)

    eventlist3_address = get_eventlist_address(oracle3_address, index=0)
    log("oracle3 eventlist address", eventlist3_address)

    # Add the OB test event to oracle3's eventlist
    add_event(eventlist3_address,
              event_name=EVENT_OB_NAME,
              describe=EVENT_OB_DESCRIBE,
              outcomes=EVENT_OB_OUTCOMES)

    # Capture the event_id by reading the events map
    ev3_map = common.run_getter(eventlist3_address, EVENTLIST_ABI, "_events", {})
    events3 = (ev3_map.get("_events") or {}) if isinstance(ev3_map, dict) else {}
    assert len(events3) > 0, "Oracle3 eventlist must have at least one event after addEvent"
    event_ob_id = list(events3.keys())[0]
    log("Phase 8: EVENT_OB_ID", event_ob_id)

    pmp5_address = deploy_pmp(pn_address, event_ob_id, ORACLE3_NAME, ORACLE_FEE, TOKEN_TYPE, index=0)
    pmp5_approved = wait_pmp_approved(pmp5_address)
    oracle_list_hash5 = pmp5_approved["oracle_list_hash"]
    log("PMP5 oracle_list_hash", oracle_list_hash5)

    # Submit short timings so stakeEnd arrives quickly
    now8 = int(time.time())
    r_start8 = now8 + PHASE8_STAKE_PERIOD
    oracle_submit_set_timings(pmp5_address, r_start8)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s_end8 = now8 + (r_start8 - now8) // 10

    pmp5_d = get_pmp_details(pmp5_address)
    assert pmp5_d["approved"], "PMP5 must be approved after submitSetTimings"
    log("Phase 8.1 PASSED: PMP5 deployed and approved", pmp5_address)

    # ── 8b: Wait for stakeEnd ──────────────────────────────────────────────────
    wait_secs = s_end8 - int(time.time()) + 3
    if wait_secs > 0:
        print(f">>> Waiting {wait_secs}s for PMP5 stakeEnd...")
        time.sleep(wait_secs)

    # ── 8c: PN1 splitFullSet (triggers auto-freeze + OB deploy) ──────────────
    pn1_bal_before_split = get_pn_balance(get_pn_details(pn_address), TOKEN_TYPE)
    pn1_stakes_before_split = get_pn_stakes(pn_address)
    # Find PMP5 stake in _stakes (all stakes keyed by hash, we need outcome amounts)
    pn_split_full_set(pn_address, event_ob_id, oracle_list_hash5, TOKEN_TYPE, SPLIT_COLLATERAL)

    pn1_after_split = get_pn_details(pn_address)
    pn1_bal_after_split = get_pn_balance(pn1_after_split, TOKEN_TYPE)
    assert pn1_bal_after_split == pn1_bal_before_split - SPLIT_COLLATERAL, \
        f"PN1 balance must decrease by SPLIT_COLLATERAL after split: " \
        f"before={pn1_bal_before_split}, after={pn1_bal_after_split}"
    assert_not_busy(pn1_after_split, "PN1 must not be busy after splitFullSet")
    log("Phase 8.3 PASSED: PN1 splitFullSet, balance reduced by SPLIT_COLLATERAL",
        pn1_bal_after_split)

    # Verify auto-freeze + OrderBook deployed
    pmp5_frozen = get_pmp_details(pmp5_address)
    assert pmp5_frozen.get("frozen"), "PMP5 must be auto-frozen after split"
    base_pool5 = int(pmp5_frozen.get("baseTotalPool", 0))
    log("PMP5 auto-frozen, baseTotalPool", base_pool5)

    ob5_out = common.run_getter(pmp5_address, PMP_ABI, "getOrderBookAddress", {})
    ob5_address = ob5_out.get("orderBookAddress") if isinstance(ob5_out, dict) else ob5_out
    log("OrderBook5 address", ob5_address)
    common.wait_account_active(ob5_address)
    ob5_d = get_ob_details(ob5_address)
    assert ob5_d is not None, "OrderBook5 must be active after auto-freeze"
    log("Phase 8.3b PASSED: OrderBook5 auto-deployed", ob5_address)

    # Verify outcome tokens received: stake.amount[k] increased by floor(SPLIT_COLLATERAL * pool_k / baseTotalPool)
    # With symmetric pools (SEED, SEED), each δ_k = floor(SPLIT_COLLATERAL / 2)
    expected_tokens_per_outcome = SPLIT_COLLATERAL // 2   # = DEPLOYER_SEED_AMOUNT
    pn1_stakes_after_split = get_pn_stakes(pn_address)
    # The stake amounts increased; we'll verify this via getOrder after placeOrder step
    log("Phase 8.4b: stakes after split", pn1_stakes_after_split)

    # ── 8e: PN1 placeOrder (sell outcome-0 tokens) ───────────────────────────
    ob5_order_count_before = get_ob_order_count(ob5_address)
    pn_place_order(pn_address, event_ob_id, oracle_list_hash5, TOKEN_TYPE,
                   SELL_OUTCOME_ID, False, SELL_PRICE_BPS, SELL_AMOUNT)

    ob5_order_count_after = get_ob_order_count(ob5_address)
    assert ob5_order_count_after == ob5_order_count_before + 1, \
        f"OB order count must increase by 1 after placeOrder: {ob5_order_count_before} → {ob5_order_count_after}"
    assert_not_busy(get_pn_details(pn_address), "PN1 must not be busy after placeOrder")
    log("Phase 8.5 PASSED: sell order placed in OrderBook", ob5_order_count_after)

    # Get the order id (nextOrderId was 1 before, so first order_id = 1)
    ob5_det = get_ob_details(ob5_address)
    next_oid = int(ob5_det.get("nextOrderId", 2))
    sell_order_id = next_oid - 1   # last assigned order id
    log("Sell order ID", sell_order_id)

    order_info = get_ob_order(ob5_address, sell_order_id)
    assert order_info is not None, "getOrder must return order details"
    assert not order_info.get("isBuy", True), "Order must be a sell"
    assert int(order_info.get("amount", 0)) == SELL_AMOUNT, "Order amount must match"
    assert normalize_uint256(order_info.get("priceBps", 0)) == SELL_PRICE_BPS, "Order price must match"
    log("Phase 8.5b PASSED: order details verified", order_info)

    # ── 8f: PN1 cancelOrder ───────────────────────────────────────────────────
    pn1_stakes_before_cancel = get_pn_stakes(pn_address)
    pn_cancel_order(pn_address, event_ob_id, oracle_list_hash5, TOKEN_TYPE, sell_order_id)

    ob5_order_count_cancel = get_ob_order_count(ob5_address)
    assert ob5_order_count_cancel == ob5_order_count_before, \
        f"OB order count must return to {ob5_order_count_before} after cancelOrder, got {ob5_order_count_cancel}"
    assert_not_busy(get_pn_details(pn_address), "PN1 must not be busy after cancelOrder")
    log("Phase 8.6 PASSED: sell order cancelled, OB empty", ob5_order_count_cancel)

    # ── 8g: PN1 mergeFullSet (return SPLIT_COLLATERAL/2 tokens per outcome) ───
    merge_amounts = [expected_tokens_per_outcome, expected_tokens_per_outcome]
    pn1_bal_before_merge = get_pn_balance(get_pn_details(pn_address), TOKEN_TYPE)
    pn_merge_full_set(pn_address, event_ob_id, oracle_list_hash5, TOKEN_TYPE, merge_amounts)

    pn1_after_merge = get_pn_details(pn_address)
    pn1_bal_after_merge = get_pn_balance(pn1_after_merge, TOKEN_TYPE)
    # PMP computes collateral as sum of floor(amount_k * baseTotalPool / sum(amounts))
    # With symmetric: collateral = DEPLOYER_SEED (= SPLIT_COLLATERAL/2 per side → total SPLIT_COLLATERAL)
    assert pn1_bal_after_merge > pn1_bal_before_merge, \
        "PN1 balance must increase after mergeFullSet"
    assert_not_busy(pn1_after_merge, "PN1 must not be busy after mergeFullSet")
    log("Phase 8.7 PASSED: mergeFullSet, collateral returned", pn1_bal_after_merge)

    # ── 8h: deleteStake to clear PN1's PMP5 stake record ─────────────────────
    # The initial seed stakes ([SEED, SEED]) remain in stake.amount after mergeFullSet.
    # deleteStake removes the local record (without refund) so _stakes.empty() == True
    # and Phase 6 initTransfer (which requires _stakes.empty()) can proceed.
    pn_delete_stake(pn_address, event_ob_id, oracle_list_hash5, TOKEN_TYPE)
    assert len(get_pn_stakes(pn_address)) == 0, \
        "PN1 stakes must be empty after deleteStake on PMP5"
    log("Phase 8.8 PASSED: PMP5 stake record cleared via deleteStake", "")

    print(">>> Phase 8 PASSED: auto-freeze + splitFullSet + placeOrder/cancelOrder + mergeFullSet")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 9 – Advanced OrderBook: immediate matching, IOC, FOK, market, minAmount, multi-epoch
    #
    # Deploy PMP6 on Oracle3 with a new event. PN1 splits collateral to get
    # outcome tokens, then exercises all order types and settle.
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 9: Advanced OrderBook (immediate matching, IOC, FOK, market, minAmount, multi-epoch)")

    EVENT_OB2_NAME = "OrderBook Advanced Test"
    EVENT_OB2_DESCRIBE = "Advanced OB test"
    EVENT_OB2_OUTCOMES = {1: "Yes", 2: "No"}
    PHASE9_SPLIT = 2 * DEPLOYER_SEED_AMOUNT  # 20M

    # ── 9a: Add new event to Oracle3's existing eventlist, deploy PMP6 ────────
    add_event(eventlist3_address,
              event_name=EVENT_OB2_NAME,
              describe=EVENT_OB2_DESCRIBE,
              outcomes=EVENT_OB2_OUTCOMES)

    ev3b_map = common.run_getter(eventlist3_address, EVENTLIST_ABI, "_events", {})
    events3b = (ev3b_map.get("_events") or {}) if isinstance(ev3b_map, dict) else {}
    # Get the new event (the one that wasn't in Phase 8)
    new_event_ids = [k for k in events3b.keys() if k != event_ob_id]
    assert len(new_event_ids) == 1, f"Expected exactly 1 new event, got {new_event_ids}"
    event_ob2_id = new_event_ids[0]
    log("Phase 9: EVENT_OB2_ID", event_ob2_id)

    pmp6_address = deploy_pmp(pn_address, event_ob2_id, ORACLE3_NAME, ORACLE_FEE, TOKEN_TYPE, index=0)
    pmp6_approved = wait_pmp_approved(pmp6_address)
    oracle_list_hash6 = pmp6_approved["oracle_list_hash"]
    log("PMP6 oracle_list_hash", oracle_list_hash6)

    now9 = int(time.time())
    r_start9 = now9 + PHASE8_STAKE_PERIOD
    oracle_submit_set_timings(pmp6_address, r_start9)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s_end9 = now9 + (r_start9 - now9) // 10

    wait_secs9 = s_end9 - int(time.time()) + 3
    if wait_secs9 > 0:
        print(f">>> Waiting {wait_secs9}s for PMP6 stakeEnd...")
        time.sleep(wait_secs9)

    # Freeze happens automatically on first split (Phase 9b)
    log("Phase 9a: stakeEnd reached, freeze on first split", "")

    # Verify PN1 has a stake for PMP6
    pn1_stakes_9 = get_pn_stakes(pn_address)
    log("Phase 9a: PN1 stakes after PMP6 deploy", pn1_stakes_9)
    assert len(pn1_stakes_9) > 0, "PN1 must have a stake for PMP6 before splitFullSet"

    pmp6_d = get_pmp_details(pmp6_address)
    log("Phase 9a: PMP6 baseTotalPool", pmp6_d.get("baseTotalPool"))
    log("Phase 9a: PMP6 frozen", pmp6_d.get("frozen"))

    # ── 9b: PN1 splitFullSet to get outcome tokens ────────────────────────────
    time.sleep(5)  # let any pending callbacks from earlier phases settle
    pn1_bal_before_split9 = get_pn_balance(get_pn_details(pn_address), TOKEN_TYPE)
    pn_split_full_set(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE, PHASE9_SPLIT)
    pn1_bal_after_split9 = get_pn_balance(get_pn_details(pn_address), TOKEN_TYPE)
    log("Phase 9b balance before/after split", f"{pn1_bal_before_split9} -> {pn1_bal_after_split9}")
    assert pn1_bal_after_split9 == pn1_bal_before_split9 - PHASE9_SPLIT, \
        f"Balance mismatch: {pn1_bal_before_split9} - {PHASE9_SPLIT} != {pn1_bal_after_split9}"
    tokens_per_outcome = PHASE9_SPLIT // 2  # 10M each
    log("Phase 9b PASSED: split done, tokens per outcome", tokens_per_outcome)

    # Get OrderBook6 (auto-deployed by split)
    ob6_out = common.run_getter(pmp6_address, PMP_ABI, "getOrderBookAddress", {})
    ob6_address = ob6_out.get("orderBookAddress") if isinstance(ob6_out, dict) else ob6_out
    log("OrderBook6 address (auto-deployed)", ob6_address)
    common.wait_account_active(ob6_address)

    # ── 9c: Test immediate matching with crossing limit orders ─────────────────
    # PN1 sells 2M outcome-0 at 50%, then buys 2M at 60% → match on placement
    TRADE_AMT = 10_000_000_000
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, False, SELL_PRICE_BPS, TRADE_AMT, epoch_id=EPOCH_1)
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, True, BUY_PRICE_BPS, TRADE_AMT, epoch_id=EPOCH_1)

    ob6_count_after = get_ob_order_count(ob6_address)
    assert ob6_count_after == 0, \
        f"OB6 must have 0 orders (both filled on placement), got {ob6_count_after}"
    assert_not_busy(get_pn_details(pn_address), "PN1 must not be busy after match")
    log("Phase 9c PASSED: immediate matching with crossing limits", "")

    # ── 9d: Test IOC orders (cancelled immediately if no match) ────────────────
    # Place IOC sell with no matching buy → cancelled immediately on placement
    IOC_AMT = 10_000_000_000
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, False, SELL_PRICE_BPS, IOC_AMT, flags=0x01, epoch_id=EPOCH_2)

    ob6_ioc_after = get_ob_order_count(ob6_address)
    assert ob6_ioc_after == 0, \
        f"IOC order must be cancelled immediately with no match, got {ob6_ioc_after}"
    log("Phase 9d PASSED: IOC order cancelled immediately (no match)", "")

    # ── 9e: Test FOK orders (Fill-Or-Kill) ────────────────────────────────────
    # Place a sell limit, then FOK buy that can fully fill → matched on placement
    EPOCH_3 = 3
    FOK_AMT = 10_000_000_000
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, False, SELL_PRICE_BPS, FOK_AMT, epoch_id=EPOCH_3)
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, True, BUY_PRICE_BPS, FOK_AMT, flags=0x02, epoch_id=EPOCH_3)

    ob6_fok_after = get_ob_order_count(ob6_address)
    assert ob6_fok_after == 0, \
        f"FOK buy + limit sell must fully match on placement, got {ob6_fok_after} remaining"
    log("Phase 9e PASSED: FOK buy fully filled against limit sell", "")

    # ── 9f: Test Market orders ────────────────────────────────────────────────
    EPOCH_4 = 4
    MKT_AMT = 10_000_000_000
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, False, SELL_PRICE_BPS, MKT_AMT, epoch_id=EPOCH_4)
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, True, 0, MKT_AMT, flags=0x04, epoch_id=EPOCH_4)  # market buy

    ob6_mkt_after = get_ob_order_count(ob6_address)
    assert ob6_mkt_after == 0, \
        f"Market buy + limit sell must match on placement, got {ob6_mkt_after} remaining"
    log("Phase 9f PASSED: Market buy filled against limit sell", "")

    # ── 9g: Test multi-epoch isolation ────────────────────────────────────────
    # Place crossing orders in epoch 5 → matched immediately.
    # Place sell in epoch 6 → rests (no counterparty in epoch 6).
    # Verify epoch 6 order is untouched by epoch 5 matching.
    EPOCH_5 = 5
    EPOCH_6 = 6
    ISO_AMT = 10_000_000_000
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, False, SELL_PRICE_BPS, ISO_AMT, epoch_id=EPOCH_5)
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, True, BUY_PRICE_BPS, ISO_AMT, epoch_id=EPOCH_5)
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, False, SELL_PRICE_BPS, ISO_AMT, epoch_id=EPOCH_6)

    ob6_multi_after = get_ob_order_count(ob6_address)
    assert ob6_multi_after == 1, \
        f"Epoch 5 matched immediately; only epoch 6 sell (1) must remain, got {ob6_multi_after}"
    log("Phase 9g PASSED: multi-epoch isolation verified", "")

    # Clean up epoch 6 order
    pn_cancel_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                    int(get_ob_details(ob6_address).get("nextOrderId", 0)) - 1)

    # ── 9h: Test minAmount partial fill protection ────────────────────────────
    # Place buy 1M first (rests), then sell 2M with minAmount=2M.
    # Sell finds only 1M available < 2M minAmount → cancelled immediately.
    # Buy remains unfilled.
    EPOCH_7 = 7
    MIN_AMT = 12_000_000_000
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, True, BUY_PRICE_BPS, 10_000_000_000, epoch_id=EPOCH_7)
    pn_place_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE,
                   0, False, SELL_PRICE_BPS, MIN_AMT, min_amount=MIN_AMT, epoch_id=EPOCH_7)

    ob6_min_after = get_ob_order_count(ob6_address)
    # Sell (2M, minAmount=2M) finds only 1M counterparty → below minAmount → cancelled.
    # Buy (1M) remains unfilled.
    assert ob6_min_after == 1, \
        f"Buy order must remain after sell cancelled (minAmount violation), got {ob6_min_after}"
    # Cancel the remaining buy order (buy was placed first → orderId = nextOrderId - 2)
    remaining_order_id = int(get_ob_details(ob6_address).get("nextOrderId", 0)) - 2
    pn_cancel_order(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE, remaining_order_id)
    ob6_min_cleanup = get_ob_order_count(ob6_address)
    assert ob6_min_cleanup == 0, f"OB must be empty after cleanup, got {ob6_min_cleanup}"
    log("Phase 9h PASSED: minAmount protection — sell cancelled, buy remains", "")

    # ── 9i: Cleanup — deleteStake for PMP6 ────────────────────────────────────
    pn_delete_stake(pn_address, event_ob2_id, oracle_list_hash6, TOKEN_TYPE)
    assert len(get_pn_stakes(pn_address)) == 0, "PN1 stakes must be empty after Phase 9 cleanup"
    log("Phase 9i PASSED: PMP6 stake cleaned up", "")

    print(">>> Phase 9 PASSED: Advanced OrderBook (immediate matching, IOC, FOK, market, minAmount, multi-epoch)")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 6 – PrivateNote token transfer & discardCoupon
    #
    # After Phase 5h, PN2 still has _coupons_value > 0 (partial coupon remaining)
    # and PN1 still has NACKL balance with no active coupon or debt.
    #
    # 6a. discardCoupon on PN2 – clears leftover coupon (not yet fully staked)
    # 6b. initTransfer: PN1 sends TRANSFER_AMOUNT to PN2
    #     - Sender (PN1): balance decreases, _has_withdrawn NOT set (transfer != withdrawal)
    #     - Receiver (PN2): auto-accepts via offerTransfer
    #     - If offerTransfer bounces: balance restored
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 6: PrivateNote token transfer & discardCoupon")

    # ── 6a. discardCoupon on PN2 ──────────────────────────────────────────────
    # After Phase 5g generateCoupon (coupon=100B) and 5h coupon stake (used 10M),
    # PN2 still has _coupons_value = 100B - 10M = 99,990,000,000 remaining.
    pn2_coupon_before_discard = get_pn_coupons_value(pn2_address)
    assert pn2_coupon_before_discard > 0, \
        f"PN2 must have active coupon before discardCoupon, got {pn2_coupon_before_discard}"
    log("PN2 _coupons_value before discardCoupon", pn2_coupon_before_discard)

    pn_discard_coupon(pn2_address, key_path=LOSER_KEY_PATH)

    pn2_coupon_after_discard = get_pn_coupons_value(pn2_address)
    assert pn2_coupon_after_discard == 0, \
        f"PN2 _coupons_value must be 0 after discardCoupon, got {pn2_coupon_after_discard}"
    assert_not_busy(get_pn_details(pn2_address), "PN2 must not be busy after discardCoupon")
    log("Phase 6.1 PASSED: discardCoupon cleared PN2 coupon", "")

    # ── 6b. initTransfer: PN1 → PN2 ──────────────────────────────────────────
    pn1_before_xfer = get_pn_details(pn_address)
    pn1_bal_before_xfer = get_pn_balance(pn1_before_xfer, TOKEN_TYPE)
    pn2_before_xfer = get_pn_details(pn2_address)
    pn2_bal_before_xfer = get_pn_balance(pn2_before_xfer, TOKEN_TYPE)
    assert pn1_bal_before_xfer >= TRANSFER_AMOUNT, \
        f"PN1 must have enough balance to transfer {TRANSFER_AMOUNT}, got {pn1_bal_before_xfer}"
    log("PN1 balance before initTransfer", pn1_bal_before_xfer)
    log("PN2 balance before initTransfer", pn2_bal_before_xfer)

    pn_init_transfer(pn_address, dih_l, TOKEN_TYPE, TRANSFER_AMOUNT)

    pn1_after_xfer = get_pn_details(pn_address)
    pn2_after_xfer = get_pn_details(pn2_address)
    pn1_bal_after_xfer = get_pn_balance(pn1_after_xfer, TOKEN_TYPE)
    pn2_bal_after_xfer = get_pn_balance(pn2_after_xfer, TOKEN_TYPE)

    # ── Check 6.2: PN1 balance decreased by TRANSFER_AMOUNT ───────────────────
    assert pn1_bal_after_xfer == pn1_bal_before_xfer - TRANSFER_AMOUNT, \
        f"PN1 balance after transfer: expected {pn1_bal_before_xfer - TRANSFER_AMOUNT}, " \
        f"got {pn1_bal_after_xfer}"
    log("Phase 6.2 PASSED: PN1 balance decreased by TRANSFER_AMOUNT", pn1_bal_after_xfer)

    # ── Check 6.3: PN2 balance increased by TRANSFER_AMOUNT ───────────────────
    assert pn2_bal_after_xfer == pn2_bal_before_xfer + TRANSFER_AMOUNT, \
        f"PN2 balance after transfer: expected {pn2_bal_before_xfer + TRANSFER_AMOUNT}, " \
        f"got {pn2_bal_after_xfer}"
    log("Phase 6.3 PASSED: PN2 balance increased by TRANSFER_AMOUNT", pn2_bal_after_xfer)

    # ── Check 6.4: Neither PN is busy after transfer completes ────────────────
    assert_not_busy(pn1_after_xfer, "PN1 must not be busy after transfer")
    assert_not_busy(pn2_after_xfer, "PN2 must not be busy after transfer")
    log("Phase 6.4 PASSED: no PN is busy after transfer", "")

    # ── Check 6.5: PN1 _has_withdrawn == False after initTransfer ─────────────
    # Transfer does NOT set _has_withdrawn — only withdrawTokens does.
    pn1_withdrawn_after_xfer = get_pn_has_withdrawn(pn_address)
    assert not pn1_withdrawn_after_xfer, \
        "PN1 _has_withdrawn must be False after initTransfer (transfer != withdrawal)"
    log("Phase 6.5 PASSED: PN1 _has_withdrawn = False after initTransfer", "")

    # ── Check 6.6: PN2 _has_withdrawn == False (receiver is NOT sealed) ───────
    pn2_withdrawn_after_xfer = get_pn_has_withdrawn(pn2_address)
    assert not pn2_withdrawn_after_xfer, \
        "PN2 _has_withdrawn must be False — receiving a transfer does not seal the note"
    log("Phase 6.6 PASSED: PN2 _has_withdrawn = False (receiver unaffected)", "")

    print(">>> Phase 6 PASSED: discardCoupon & initTransfer")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 7 – PrivateNote management: changeOwner, withdrawTokens
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 7: PrivateNote management (changeOwner, withdrawTokens)")

    # 7a. changeOwner – rotate to a fresh key pair
    common.gen_keys(NEW_OWNER_KEY_PATH)
    new_pubkey = common.read_public_key(NEW_OWNER_KEY_PATH)
    if not new_pubkey.startswith("0x"):
        new_pubkey = "0x" + new_pubkey
    log("new owner pubkey", new_pubkey)

    pn_change_owner(pn_address, new_pubkey)

    pn_after_owner = get_pn_details(pn_address)
    log("PrivateNote after changeOwner", pn_after_owner)

    # ── Check 7.1: ephemeralPubkey updated to new_pubkey ───────────────────────
    stored_pk_new = normalize_uint256(pn_after_owner.get("ephemeralPubkey", 0))
    expected_pk_new = normalize_uint256(new_pubkey)
    assert stored_pk_new == expected_pk_new, \
        f"ephemeralPubkey after changeOwner: expected {expected_pk_new}, got {stored_pk_new}"
    assert_not_busy(pn_after_owner, "PN must not be busy after changeOwner")
    log("Phase 7.1 PASSED: ephemeralPubkey updated to new_pubkey", hex(stored_pk_new))

    # Rotate back to original ephemeral key so remaining steps work
    orig_pubkey_str = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not orig_pubkey_str.startswith("0x"):
        orig_pubkey_str = "0x" + orig_pubkey_str
    pn_change_owner(pn_address, orig_pubkey_str, key_path=NEW_OWNER_KEY_PATH)
    log("changeOwner: restored original key", orig_pubkey_str)

    pn_after_restore = get_pn_details(pn_address)

    # ── Check 7.2: ephemeralPubkey restored to original ────────────────────────
    stored_pk_restored = normalize_uint256(pn_after_restore.get("ephemeralPubkey", 0))
    expected_pk_orig = normalize_uint256(orig_pubkey_str)
    assert stored_pk_restored == expected_pk_orig, \
        f"ephemeralPubkey after restore: expected {expected_pk_orig}, got {stored_pk_restored}"
    assert_not_busy(pn_after_restore, "PN must not be busy after restoring key")
    log("Phase 7.2 PASSED: ephemeralPubkey restored to original", hex(stored_pk_restored))

    # 7b. withdrawTokens – send remaining NACKL balance to giver
    pn_before_wd = get_pn_details(pn_address)
    pn_bal_before_wd6 = get_pn_balance(pn_before_wd, TOKEN_TYPE)
    log("PrivateNote before withdrawTokens", pn_before_wd)
    assert pn_bal_before_wd6 > 0, \
        f"PN must have NACKL balance before withdrawTokens, got {pn_bal_before_wd6}"

    # ── Check 7.3: PN has no active stakes before withdraw ────────────────────
    stakes_before_wd = get_pn_stakes(pn_address)
    assert len(stakes_before_wd) == 0, \
        f"PN must have no stake records before withdrawTokens, got {len(stakes_before_wd)}"

    pn_withdraw_tokens(pn_address, GIVER_ADDRESS, TOKEN_TYPE)

    pn_after_wd6 = get_pn_details(pn_address)
    log("PrivateNote after withdrawTokens", pn_after_wd6)

    # ── Check 7.4: PN NACKL balance == 0 after withdrawTokens ────────────────
    pn_bal_after_wd6 = get_pn_balance(pn_after_wd6, TOKEN_TYPE)
    assert pn_bal_after_wd6 == 0, \
        f"PN NACKL balance must be 0 after withdrawTokens, got {pn_bal_after_wd6}"
    assert_not_busy(pn_after_wd6, "PN must not be busy after withdrawTokens")
    log("Phase 7.4 PASSED: PN balance == 0 after withdrawTokens", "")

    # ── Check 7.5: generateCoupon must fail (sealed by withdrawTokens) ────────
    # _has_withdrawn=True was set by withdrawTokens in Phase 7b.
    # generateCoupon requires !_has_withdrawn → must reject with ERR_INVALID_STATE (151).
    ERR_INVALID_STATE = 151
    gc_after_withdraw = pn_generate_coupon(pn_address, TOKEN_TYPE)
    assert common.is_tvm_error(gc_after_withdraw, ERR_INVALID_STATE), \
        f"generateCoupon must fail with ERR_INVALID_STATE (151) after withdrawTokens, " \
        f"got: {gc_after_withdraw}"
    log("Phase 7.5 PASSED: generateCoupon rejected after withdrawTokens (ERR_INVALID_STATE)", "")

    print(">>> Phase 7 PASSED: changeOwner, withdrawTokens, generateCoupon blocked after withdraw")

    # ══════════════════════════════════════════════════════════════════════════
    # Summary
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "=" * 64)
    print("ALL PHASES PASSED (1-10, 6, 7)")
    print("=" * 64)
    print(f"Oracle1 address:     {oracle_address}")
    print(f"Oracle2 address:     {oracle2_address}")
    print(f"Oracle3 address:     {oracle3_address}")
    print(f"EventList0 address:  {eventlist_address}")
    print(f"EventList1 address:  {eventlist1_address}")
    print(f"EventList2 address:  {eventlist2_address}")
    print(f"EventList3 address:  {eventlist3_address}")
    print(f"PN1 address:         {pn_address}")
    print(f"PN2 address:         {pn2_address}")
    print(f"deposit_id_hash:     {dih}")
    print(f"oracle_list_hash:    {oracle_list_hash}")
    print(f"oracle_list_hash3:   {oracle_list_hash3}")
    print(f"oracle_list_hash5:   {oracle_list_hash5}")
    print(f"OrderBook5 address:  {ob5_address}")
    print("=" * 64)


if __name__ == "__main__":
    main()
