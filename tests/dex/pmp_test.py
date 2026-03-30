"""
PMP combination test — verifies balance/debt/coupon for all stake type combinations.

Based on pmp_test_spec.md: 267 combinations of stake types on winning (A) and losing (B) sides.
Tests 1-6 are fully specified with hardcoded expected values.

Each test:
  1. Creates named PNs with specified initial state (balance, debt, coupon_value)
  2. Creates PMP, places stakes
  3. Resolves PMP with outcome 0 (side A always wins)
  4. Claims for all participants
  5. Verifies (balance, debt, coupon_value) for each named PN

Tests run in parallel: each test uses its own event_id so PMP addresses never collide.
"""

import json
import os
import random
import threading
import time
import sys

sys.path.append("tests")
from helper import common

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
EPHEMERAL_KEY_PATH = "./tests/dex/pmp_test_ephemeral.keys.json"
ORACLE_KEY_PATH = "./tests/dex/pmp_test_oracle.keys.json"

# ── Token types ────────────────────────────────────────────────────────────────
TOKEN_TYPE = 1        # NACKL
TOKEN_TYPE_SHELL = 2  # Shell
TOKEN_TYPE_ECC = 300  # Shell fee (ECC)

# ── Event constants ────────────────────────────────────────────────────────────
EVENT_DESCRIBE = "Who will win match X"
EVENT_OUTCOMES = {1: "Team A", 2: "Team B"}
EVENT_DEADLINE = 2000000000
ORACLE_FEE = 100

# 6 unique event names — one per parallel test
EVENT_NAMES = [f"PMPTest_{i}" for i in range(1, 7)]
# Populated by main() after setup_oracle
EVENT_IDS = []

# ── Contract constants ─────────────────────────────────────────────────────────
FULL_PERCENT = 10000
MIN_NACKL = 10_000_000
ECC_SHELL_DEPOSIT = 10_000_000_000
NACKL_COUPON_VALUE = 100_000_000_000
NACKL_COUPON_DEBT = 5_000_000_000
STAKE_PERIOD = 300  # 5 min (stakeEnd = 30s, enough for 3+ stakes at ~7s each)

# ── Results tracking ──────────────────────────────────────────────────────────
RESULTS = []
results_lock = threading.Lock()
RESULTS_FILE = "tests/dex/pmp_test_results.txt"

# ── Global state ──────────────────────────────────────────────────────────────
# Named PNs: index -> (address, dih, sk)
# Each test uses unique indexes (T1:1, T2:2, T3:3-4, T4:5-6, T5:7-10, T6:11-13)
NAMED_PNS = {}


def generate_random_sk():
    return os.urandom(31).hex() + format(random.randint(0, 0x2f), '02x')


def wait_active(address, timeout=90):
    """Wait for account to become Active with extended timeout (for parallel load)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        out = common.execute_cli_cmd(f"account {address}")
        if isinstance(out, dict) and out.get("acc_type") == "Active":
            return
        time.sleep(1)
    raise TimeoutError(f"Account {address} not active after {timeout}s")


def log(title, data):
    print(f"\n=== {title} ===")
    print(data)
    print("===")


def record_result(name, passed, details=""):
    status = "PASS" if passed else "FAIL"
    with results_lock:
        RESULTS.append({"name": name, "passed": passed, "details": details})
    print(f"\n  [{status}] {name}")
    if details:
        print(f"         {details}")


# ══════════════════════════════════════════════════════════════════════════════
# Contract interaction helpers
# ══════════════════════════════════════════════════════════════════════════════

def get_pn_balance(pn_details, token_type):
    bal = pn_details.get("balance", {})
    val = bal.get(str(token_type), bal.get(token_type, 0))
    return int(val) if val is not None else 0


def send_tokens_to_root_pn(token_type, value):
    params = {
        "dest": ROOT_PN_ADDRESS,
        "value": 2_000_000_000,
        "ecc": {token_type: value},
        "flag": 1,
    }
    common.call_contract(GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
                         "sendCurrencyWithFlag", params)
    time.sleep(3)


def generate_proof(skcommit, token_type, value):
    out = common.execute_cmd(f"./halo2-proover {skcommit} {token_type} {value}")
    time.sleep(1)
    raw = [line for line in out.splitlines() if line.strip()][-1]
    pair = json.loads(json.loads(raw))
    dih = "0x" + pair["private_note_digest"]
    nullifier = "0x" + pair["private_note_digest"]
    return (pair["proof"], dih,
            int(pair["private_note_sum"]), int(pair["token_type"]), nullifier)


def deploy_private_note(proof, deposit_identifier_hash, value, token_type, ephemeral_pubkey):
    params = {
        "zkproof": proof,
        "deposit_identifier_hash": deposit_identifier_hash,
        "ephemeral_pubkey": ephemeral_pubkey,
        "value": value,
        "token_type": token_type,
    }
    common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                         EPHEMERAL_KEY_PATH, "deployPrivateNote", params)
    time.sleep(2)


def get_private_note_address(deposit_identifier_hash):
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                            "getPrivateNoteAddress",
                            {"deposit_identifier_hash": deposit_identifier_hash})
    time.sleep(1)
    return out["privateNoteAddress"]


def send_ecc_to_private_note(proof, nullifier_hash, deposit_identifier_hash, value):
    params = {
        "proof": proof,
        "nullifier_hash": nullifier_hash,
        "deposit_identifier_hash": deposit_identifier_hash,
        "value": value,
    }
    common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                         EPHEMERAL_KEY_PATH, "sendEccShellToPrivateNote", params)
    time.sleep(6)


def create_pn(nackl_balance, label="PN"):
    """Deploy a new PrivateNote with given NACKL balance. Returns (address, dih, sk)."""
    sk = generate_random_sk()
    print(f"  Creating {label} (balance={nackl_balance})")

    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    send_tokens_to_root_pn("1", nackl_balance)

    proof, dih, value, ttype, nullifier = generate_proof(sk, TOKEN_TYPE, nackl_balance)

    ephemeral_pubkey = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not ephemeral_pubkey.startswith("0x"):
        ephemeral_pubkey = "0x" + ephemeral_pubkey

    deploy_private_note(proof, dih, value, ttype, ephemeral_pubkey)
    pn_address = get_private_note_address(dih)
    wait_active(pn_address)

    proof_sh, null_sh, val_sh, _, _ = generate_proof(sk, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT)
    send_ecc_to_private_note(proof_sh, null_sh, dih, ECC_SHELL_DEPOSIT)

    return pn_address, dih, sk


def get_pn_details(pn_address):
    return common.run_getter(pn_address, PRIVATE_NOTE_ABI, "getDetails", {})


def read_pn_debt(pn_address):
    data = common.execute_cli_cmd(
        f"decode account data --abi {PRIVATE_NOTE_ABI} --addr {pn_address}"
    )
    return int((data or {}).get("_debt", 0))


def read_pn_coupons(pn_address):
    data = common.execute_cli_cmd(
        f"decode account data --abi {PRIVATE_NOTE_ABI} --addr {pn_address}"
    )
    return int((data or {}).get("_coupons_value", 0))


def get_pmp_details(pmp_address):
    return common.run_getter(pmp_address, PMP_ABI, "getDetails", {})


def deploy_pmp(pn_address, event_id, oracle_name, oracle_fee, token_type,
               index=0, initial_stakes=None):
    if initial_stakes is None:
        initial_stakes = [MIN_NACKL, MIN_NACKL]
    params = {
        "event_id": event_id,
        "oracleFee": [oracle_fee],
        "token_type": token_type,
        "names": [oracle_name],
        "index": [index],
        "initialStakes": initial_stakes,
    }
    common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                         EPHEMERAL_KEY_PATH, "deployPMP", params)
    time.sleep(3)

    pmp_out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI, "getPMPAddress",
                                {"event_id": event_id,
                                 "names": [oracle_name],
                                 "token_type": token_type})
    pmp_address = pmp_out["pmpAddress"]
    time.sleep(2)
    wait_active(pmp_address)
    return pmp_address


def wait_pmp_approved(pmp_address, max_wait=90):
    deadline = time.time() + max_wait
    while time.time() < deadline:
        details = get_pmp_details(pmp_address)
        if details:
            n = int(details.get("numberOfOracleEvents", 0))
            a = int(details.get("approvedOracleEvents", 0))
            if n > 0 and a >= n:
                time.sleep(5)
                return details
        time.sleep(3)
    raise TimeoutError(f"PMP {pmp_address} oracle confirmation timeout")


def oracle_submit_set_timings(pmp_address, result_start):
    params = {"resultStart": result_start}
    common.call_contract(pmp_address, PMP_ABI,
                         ORACLE_KEY_PATH, "submitSetTimings", params)
    time.sleep(3)


def oracle_submit_resolve(pmp_address, outcome_id):
    params = {"outcomeId": outcome_id}
    common.call_contract(pmp_address, PMP_ABI,
                         ORACLE_KEY_PATH, "submitResolve", params)
    time.sleep(3)


def setup_oracle(oracle_name):
    """Deploy oracle, add 6 events (one per test), return (oracle_addr, el_addr)."""
    oracle_pubkey = common.read_public_key(ORACLE_KEY_PATH)
    if not oracle_pubkey.startswith("0x"):
        oracle_pubkey = "0x" + oracle_pubkey

    params = {"oraclePubkey": oracle_pubkey, "oracleName": oracle_name}
    common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                         EPHEMERAL_KEY_PATH, "deployOracle", params)
    time.sleep(3)

    out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                            "getOracleAddress", {"name": oracle_name})
    oracle_addr = out.get("oracleAddress") if isinstance(out, dict) else out
    wait_active(oracle_addr)

    el_out = common.run_getter(oracle_addr, ORACLE_ABI,
                                "getEventListAddress", {"index": 0})
    el_addr = el_out.get("value0") if isinstance(el_out, dict) else el_out
    wait_active(el_addr)

    # Add one event per test (6 total) so each test gets a unique event_id
    for event_name in EVENT_NAMES:
        add_params = {
            "event_name": event_name,
            "oracle_fee": ORACLE_FEE,
            "deadline": EVENT_DEADLINE,
            "describe": EVENT_DESCRIBE,
            "outcomeNames": EVENT_OUTCOMES,
            "trustAddr": None,
        }
        common.call_contract(el_addr, EVENTLIST_ABI, ORACLE_KEY_PATH, "addEvent", add_params)
        time.sleep(2)

    return oracle_addr, el_addr


def get_event_id_by_name(el_addr, event_name):
    """Retrieve event_id (hex string) from EventList by matching event_name."""
    out = common.run_getter(el_addr, EVENTLIST_ABI, "_events", {})
    events = (out.get("_events") or {}) if isinstance(out, dict) else {}
    for eid, info in events.items():
        if isinstance(info, dict) and info.get("event_name") == event_name:
            # eid may be decimal or hex string
            eid_int = int(eid, 16) if isinstance(eid, str) and eid.startswith("0x") else int(eid)
            return "0x" + hex(eid_int)[2:].zfill(64)
    raise ValueError(f"Event '{event_name}' not found in EventList {el_addr}")


def pn_set_stake(pn_address, event_id, olh, token_type,
                 outcome, amount, use_coupon=False):
    params = {
        "event_id": event_id,
        "oracle_list_hash": olh,
        "token_type": token_type,
        "outcome": outcome,
        "amount": amount,
        "use_coupon": use_coupon,
    }
    common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                         EPHEMERAL_KEY_PATH, "setStake", params)
    time.sleep(4)


def pn_claim(pn_address, event_id, olh, token_type):
    params = {
        "event_id": event_id,
        "oracle_list_hash": olh,
        "token_type": token_type,
    }
    common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                         EPHEMERAL_KEY_PATH, "claim", params)
    time.sleep(4)


def pn_generate_coupon(pn_address, token_type):
    params = {"token_type": token_type}
    common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                         EPHEMERAL_KEY_PATH, "generateCoupon", params)
    time.sleep(3)


def pn_discard_coupon(pn_address):
    common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                         EPHEMERAL_KEY_PATH, "discardCoupon", {})
    time.sleep(2)


def pn_init_transfer(pn_address, dest_dih, token_type, amount):
    params = {
        "dest_deposit_hash": dest_dih,
        "token_type": token_type,
        "amount": amount,
    }
    common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                         EPHEMERAL_KEY_PATH, "initTransfer", params)
    time.sleep(6)


# ══════════════════════════════════════════════════════════════════════════════
# Spec operations (DSL from pmp_test_spec.md)
# ══════════════════════════════════════════════════════════════════════════════

ORACLE_NAME = None  # set in main()


def DNI(index=None):
    """Deploy PN with 100e9 NACKL. If index is not None, register as named."""
    addr, dih, sk = create_pn(100_000_000_000, f"PN_{index}" if index else "PN_anon")
    if index is not None:
        NAMED_PNS[index] = (addr, dih, sk)
    return addr, dih, sk


def DNII(index=None):
    """Deploy PN with 1000e9 NACKL."""
    addr, dih, sk = create_pn(1_000_000_000_000, f"PN_{index}" if index else "PN_anon")
    if index is not None:
        NAMED_PNS[index] = (addr, dih, sk)
    return addr, dih, sk


def DNIII(index=None):
    """Deploy PN with 10000e9 NACKL."""
    addr, dih, sk = create_pn(10_000_000_000_000, f"PN_{index}" if index else "PN_anon")
    if index is not None:
        NAMED_PNS[index] = (addr, dih, sk)
    return addr, dih, sk


def CREATE(deployer_addr, s_0, s_1, event_id):
    """Create PMP with 2 outcomes. Returns (pmp_addr, olh, event_id, r_start)."""
    pmp_addr = deploy_pmp(deployer_addr, event_id, ORACLE_NAME, ORACLE_FEE,
                          TOKEN_TYPE, index=0, initial_stakes=[s_0, s_1])
    approved = wait_pmp_approved(pmp_addr)
    olh = approved["oracle_list_hash"]

    now = int(time.time())
    r_start = now + STAKE_PERIOD
    oracle_submit_set_timings(pmp_addr, r_start)

    return pmp_addr, olh, event_id, r_start


def STAKE(pmp_addr, olh, event_id, outcome, amount, stake_type, pn_addr):
    """Place a stake. stake_type: 'M' (clean), 'D' (debt), 'C' (coupon)."""
    use_coupon = (stake_type == 'C')
    # For 'D' (debt): use_coupon=False, contract auto-detects debt from _debt > 0
    # For 'M' (clean): use_coupon=False, _debt == 0
    pn_set_stake(pn_addr, event_id, olh, TOKEN_TYPE, outcome, amount,
                 use_coupon=use_coupon)


def RESOLVE(pmp_addr, event_id, olh, r_start, winner, participants):
    """Resolve PMP and claim for all participants.
    participants: list of (pn_addr, event_id, olh)."""
    wait_secs = r_start - int(time.time()) + 5
    if wait_secs > 0:
        print(f"  Waiting {wait_secs}s for result window...")
        time.sleep(wait_secs)
    oracle_submit_resolve(pmp_addr, winner)

    for pn_addr in participants:
        pn_claim(pn_addr, event_id, olh, TOKEN_TYPE)


def T(src_addr, src_dih, dest_addr, dest_dih, amount):
    """Transfer amount NACKL from src to dest."""
    pn_init_transfer(src_addr, dest_dih, TOKEN_TYPE, amount)


def CREATE_COUPON(pn_addr):
    """Generate coupon on PN. Requires balance < 10e6, debt==0, coupon==0."""
    pn_generate_coupon(pn_addr, TOKEN_TYPE)


def DISCARD_COUPON(pn_addr):
    """Discard coupon on PN."""
    pn_discard_coupon(pn_addr)


def read_pn_state(pn_addr):
    """Read (balance, debt, coupon_value) for a PN."""
    det = get_pn_details(pn_addr)
    bal = get_pn_balance(det, TOKEN_TYPE)
    debt = read_pn_debt(pn_addr)
    coupon = read_pn_coupons(pn_addr)
    return bal, debt, coupon


# ══════════════════════════════════════════════════════════════════════════════
# Helper functions from spec
# ══════════════════════════════════════════════════════════════════════════════

def CREATE_COUPON_BETTOR(index, event_id):
    """Result: PN with balance=0, debt=5e9, coupon=100e9."""
    d_addr, d_dih, d_sk = DNI()          # anonymous deployer
    addr, dih, sk = DNI(index)            # the bettor

    pmp_addr, olh, eid, r_start = CREATE(d_addr, MIN_NACKL, MIN_NACKL, event_id)
    STAKE(pmp_addr, olh, eid, 0, NACKL_COUPON_VALUE, 'M', addr)
    RESOLVE(pmp_addr, eid, olh, r_start, 1, [addr, d_addr])  # outcome 1 wins, x loses

    CREATE_COUPON(addr)

    bal, debt, coupon = read_pn_state(addr)
    print(f"  CREATE_COUPON_BETTOR({index}): bal={bal}, debt={debt}, coupon={coupon}")
    return addr, dih, sk


def CREATE_DEBT_BETTOR(index, target_debt, event_id):
    """Result: PN with balance=target_debt-5e9, debt=target_debt, coupon=100e9-coupon_stake."""
    coupon_stake = (target_debt - NACKL_COUPON_DEBT) // 2
    S_A = 19 * coupon_stake
    S_B = 10_000_000_000_000 - S_A

    addr, dih, sk = CREATE_COUPON_BETTOR(index, event_id)

    d_addr, d_dih, d_sk = DNIII()  # anonymous deployer with 10e12
    pmp_addr, olh, eid, r_start = CREATE(d_addr, S_A, S_B, event_id)
    STAKE(pmp_addr, olh, eid, 0, coupon_stake, 'C', addr)
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr, d_addr])  # outcome 0 wins, x wins coupon

    bal, debt, coupon = read_pn_state(addr)
    print(f"  CREATE_DEBT_BETTOR({index}, {target_debt}): bal={bal}, debt={debt}, coupon={coupon}")
    return addr, dih, sk


def CREATE_DEBT_BETTOR_WITH_BALANCE(index, target_debt, balance, event_id):
    """Result: PN with balance=balance, debt=target_debt, coupon=100e9-coupon_stake."""
    addr, dih, sk = CREATE_DEBT_BETTOR(index, target_debt, event_id)

    current_bal, _, _ = read_pn_state(addr)

    # Top up balance via transfers
    while current_bal < balance:
        h_addr, h_dih, h_sk = DNIII()
        T(h_addr, h_dih, addr, dih, 10_000_000_000_000)
        current_bal += 10_000_000_000_000

    excess = current_bal - balance
    if excess == 0:
        return addr, dih, sk

    if excess < MIN_NACKL:
        h_addr, h_dih, h_sk = DNIII()
        T(h_addr, h_dih, addr, dih, MIN_NACKL)
        excess += MIN_NACKL

    # Drain excess via losing a bet (debt > 0 -> auto debt stake, losing doesn't change debt)
    d_addr, d_dih, d_sk = DNI()
    pmp_addr, olh, eid, r_start = CREATE(d_addr, MIN_NACKL, MIN_NACKL, event_id)
    STAKE(pmp_addr, olh, eid, 1, excess, 'D', addr)  # stake on loser
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr, d_addr])

    bal, debt, coupon = read_pn_state(addr)
    print(f"  CREATE_DEBT_BETTOR_WITH_BALANCE({index}, {target_debt}, {balance}): "
          f"bal={bal}, debt={debt}, coupon={coupon}")
    return addr, dih, sk


def CREATE_PN_WITH_BALANCE(index, balance, event_id):
    """Result: PN with balance=balance, debt=0, coupon=0."""
    addr, dih, sk = DNIII(index)
    current_bal = 10_000_000_000_000

    while current_bal < balance:
        h_addr, h_dih, h_sk = DNIII()
        T(h_addr, h_dih, addr, dih, 10_000_000_000_000)
        current_bal += 10_000_000_000_000

    excess = current_bal - balance
    if excess == 0:
        return addr, dih, sk

    if excess < MIN_NACKL:
        h_addr, h_dih, h_sk = DNIII()
        T(h_addr, h_dih, addr, dih, MIN_NACKL)
        excess += MIN_NACKL

    # Drain excess via losing bet
    d_addr, d_dih, d_sk = DNI()
    pmp_addr, olh, eid, r_start = CREATE(d_addr, MIN_NACKL, MIN_NACKL, event_id)
    STAKE(pmp_addr, olh, eid, 1, excess, 'M', addr)  # stake on loser
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr, d_addr])

    bal, debt, coupon = read_pn_state(addr)
    print(f"  CREATE_PN_WITH_BALANCE({index}, {balance}): bal={bal}, debt={debt}, coupon={coupon}")
    return addr, dih, sk


# ══════════════════════════════════════════════════════════════════════════════
# Test verification
# ══════════════════════════════════════════════════════════════════════════════

def verify_pn(test_num, pn_index, expected_bal, expected_debt, expected_coupon, phase="after"):
    """Verify PN state matches expected values."""
    addr = NAMED_PNS[pn_index][0]
    bal, debt, coupon = read_pn_state(addr)

    bal_ok = bal == expected_bal
    debt_ok = debt == expected_debt
    coupon_ok = coupon == expected_coupon

    all_ok = bal_ok and debt_ok and coupon_ok

    details = (f"PN_{pn_index} {phase}: "
               f"bal={bal} (exp={expected_bal}), "
               f"debt={debt} (exp={expected_debt}), "
               f"coupon={coupon} (exp={expected_coupon})")

    record_result(f"T{test_num}: PN_{pn_index} {phase}", all_ok, details)
    return all_ok


# ══════════════════════════════════════════════════════════════════════════════
# Test cases
# ══════════════════════════════════════════════════════════════════════════════

def test_1(event_id):
    """T1: A=-- B=-- (deployer only, min stakes both sides)"""
    print("\n" + "="*70)
    print("  Test 1: A=-- B=--")
    print("="*70)

    # DNI(1)
    DNI(1)
    verify_pn(1, 1, 100_000_000_000, 0, 0, "before")

    # CREATE(p1, 1, 10e6, 10e6)
    addr1 = NAMED_PNS[1][0]
    pmp_addr, olh, eid, r_start = CREATE(addr1, MIN_NACKL, MIN_NACKL, event_id)

    # RESOLVE(p1, 0)
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr1])

    # Verify: 1(100e9, 0, 0) — deployer gets fee + payout = totalPool
    verify_pn(1, 1, 100_000_000_000, 0, 0, "after")


def test_2(event_id):
    """T2: A=-- B=$ (deployer large stake on loser)"""
    print("\n" + "="*70)
    print("  Test 2: A=-- B=$")
    print("="*70)

    # CREATE_PN_WITH_BALANCE(2, 10000001e7)  = 100_000_010_000_000
    CREATE_PN_WITH_BALANCE(2, 100_000_010_000_000, event_id)
    verify_pn(2, 2, 100_000_010_000_000, 0, 0, "before")

    # CREATE(p2, 2, 10e6, 100000e9)
    addr2 = NAMED_PNS[2][0]
    pmp_addr, olh, eid, r_start = CREATE(addr2, MIN_NACKL, 100_000_000_000_000, event_id)

    # RESOLVE(p2, 0)
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr2])

    # Verify: 2(10000001e7, 0, 0) — deployer gets everything back
    verify_pn(2, 2, 100_000_010_000_000, 0, 0, "after")


def test_3(event_id):
    """T3: A=-- B=M (deployer + extra clean on loser)"""
    print("\n" + "="*70)
    print("  Test 3: A=-- B=M")
    print("="*70)

    # CREATE_PN_WITH_BALANCE(3, 2e7), CREATE_PN_WITH_BALANCE(4, 2e13+1)
    CREATE_PN_WITH_BALANCE(3, 20_000_000, event_id)
    CREATE_PN_WITH_BALANCE(4, 20_000_000_000_001, event_id)
    verify_pn(3, 3, 20_000_000, 0, 0, "before")
    verify_pn(3, 4, 20_000_000_000_001, 0, 0, "before")

    # CREATE(p3, 3, 10e6, 10e6), STAKE(p3, 1, 2e13, M, 4), RESOLVE(p3, 0)
    addr3 = NAMED_PNS[3][0]
    addr4 = NAMED_PNS[4][0]
    pmp_addr, olh, eid, r_start = CREATE(addr3, MIN_NACKL, MIN_NACKL, event_id)
    STAKE(pmp_addr, olh, eid, 1, 20_000_000_000_000, 'M', addr4)
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr3, addr4])

    # Verify: 3(2000002e7, 0, 0), 4(1, 0, 0)
    verify_pn(3, 3, 20_000_020_000_000, 0, 0, "after")
    verify_pn(3, 4, 1, 0, 0, "after")


def test_4(event_id):
    """T4: A=-- B=D (deployer + debt stake on loser)"""
    print("\n" + "="*70)
    print("  Test 4: A=-- B=D")
    print("="*70)

    # CREATE_PN_WITH_BALANCE(5, 1e15)
    CREATE_PN_WITH_BALANCE(5, 1_000_000_000_000_000, event_id)
    # CREATE_DEBT_BETTOR_WITH_BALANCE(6, 2e11, 1e15), DISCARD_COUPON(6)
    CREATE_DEBT_BETTOR_WITH_BALANCE(6, 200_000_000_000, 1_000_000_000_000_000, event_id)
    addr6 = NAMED_PNS[6][0]
    DISCARD_COUPON(addr6)

    verify_pn(4, 5, 1_000_000_000_000_000, 0, 0, "before")
    verify_pn(4, 6, 1_000_000_000_000_000, 200_000_000_000, 0, "before")

    # CREATE(p4, 5, 10e6, 10e6), STAKE(p4, 1, 1e15, D, 6), RESOLVE(p4, 0)
    addr5 = NAMED_PNS[5][0]
    pmp_addr, olh, eid, r_start = CREATE(addr5, MIN_NACKL, MIN_NACKL, event_id)
    STAKE(pmp_addr, olh, eid, 1, 1_000_000_000_000_000, 'D', addr6)
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr5, addr6])

    # Verify: 5(2e15, 0, 0), 6(0, 2e11, 0)
    verify_pn(4, 5, 2_000_000_000_000_000, 0, 0, "after")
    verify_pn(4, 6, 0, 200_000_000_000, 0, "after")


def test_5(event_id):
    """T5: A=-- B=M+D (deployer + clean + debt on loser)"""
    print("\n" + "="*70)
    print("  Test 5: A=-- B=M+D")
    print("="*70)

    # CREATE_PN_WITH_BALANCE(7, 5e7)
    CREATE_PN_WITH_BALANCE(7, 50_000_000, event_id)
    # CREATE_PN_WITH_BALANCE(8, 3e11)
    CREATE_PN_WITH_BALANCE(8, 300_000_000_000, event_id)
    # CREATE_PN_WITH_BALANCE(9, 7e12)
    CREATE_PN_WITH_BALANCE(9, 7_000_000_000_000, event_id)
    # CREATE_DEBT_BETTOR_WITH_BALANCE(10, 1e11, 5e13)
    CREATE_DEBT_BETTOR_WITH_BALANCE(10, 100_000_000_000, 50_000_000_000_000, event_id)

    # Read coupon_stake for PN_10: (target_debt - 5e9) / 2 = (1e11 - 5e9) / 2 = 47.5e9
    coupon_stake_10 = (100_000_000_000 - NACKL_COUPON_DEBT) // 2  # = 47_500_000_000
    expected_coupon_10 = NACKL_COUPON_VALUE - coupon_stake_10  # = 52_500_000_000

    verify_pn(5, 7, 50_000_000, 0, 0, "before")
    verify_pn(5, 8, 300_000_000_000, 0, 0, "before")
    verify_pn(5, 9, 7_000_000_000_000, 0, 0, "before")
    verify_pn(5, 10, 50_000_000_000_000, 100_000_000_000, expected_coupon_10, "before")

    # CREATE(p5, 7, 10e6, 10e6)
    addr7 = NAMED_PNS[7][0]
    addr8 = NAMED_PNS[8][0]
    addr9 = NAMED_PNS[9][0]
    addr10 = NAMED_PNS[10][0]
    pmp_addr, olh, eid, r_start = CREATE(addr7, MIN_NACKL, MIN_NACKL, event_id)

    # STAKE(p5, 1, 3e11, M, 8), STAKE(p5, 1, 7e12, M, 9), STAKE(p5, 1, 5e13, D, 10)
    STAKE(pmp_addr, olh, eid, 1, 300_000_000_000, 'M', addr8)
    STAKE(pmp_addr, olh, eid, 1, 7_000_000_000_000, 'M', addr9)
    STAKE(pmp_addr, olh, eid, 1, 50_000_000_000_000, 'D', addr10)

    # RESOLVE(p5, 0)
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr7, addr8, addr9, addr10])

    # Verify: 7(5730005e7, 0, 0), 8(0, 0, 0), 9(0, 0, 0), 10(0, 1e11, 525e8)
    verify_pn(5, 7, 57_300_050_000_000, 0, 0, "after")
    verify_pn(5, 8, 0, 0, 0, "after")
    verify_pn(5, 9, 0, 0, 0, "after")
    verify_pn(5, 10, 0, 100_000_000_000, expected_coupon_10, "after")


def test_6(event_id):
    """T6: A=-- B=M+C (deployer + clean + coupon on loser)"""
    print("\n" + "="*70)
    print("  Test 6: A=-- B=M+C")
    print("="*70)

    # CREATE_PN_WITH_BALANCE(11, 2e7)
    CREATE_PN_WITH_BALANCE(11, 20_000_000, event_id)
    # CREATE_PN_WITH_BALANCE(12, 2e12)
    CREATE_PN_WITH_BALANCE(12, 2_000_000_000_000, event_id)
    # CREATE_COUPON_BETTOR(13)
    CREATE_COUPON_BETTOR(13, event_id)

    verify_pn(6, 11, 20_000_000, 0, 0, "before")
    verify_pn(6, 12, 2_000_000_000_000, 0, 0, "before")
    verify_pn(6, 13, 0, NACKL_COUPON_DEBT, NACKL_COUPON_VALUE, "before")

    # CREATE(p6, 11, 10e6, 10e6)
    addr11 = NAMED_PNS[11][0]
    addr12 = NAMED_PNS[12][0]
    addr13 = NAMED_PNS[13][0]
    pmp_addr, olh, eid, r_start = CREATE(addr11, MIN_NACKL, MIN_NACKL, event_id)

    # STAKE(p6, 1, 2e12, M, 12), STAKE(p6, 1, 3e10, C, 13)
    STAKE(pmp_addr, olh, eid, 1, 2_000_000_000_000, 'M', addr12)
    STAKE(pmp_addr, olh, eid, 1, 30_000_000_000, 'C', addr13)

    # RESOLVE(p6, 0)
    RESOLVE(pmp_addr, eid, olh, r_start, 0, [addr11, addr12, addr13])

    # Verify: 11(200002e7, 0, 0), 12(0, 0, 0), 13(0, 5e9, 7e10)
    verify_pn(6, 11, 2_000_020_000_000, 0, 0, "after")
    verify_pn(6, 12, 0, 0, 0, "after")
    verify_pn(6, 13, 0, NACKL_COUPON_DEBT, 70_000_000_000, "after")


# ══════════════════════════════════════════════════════════════════════════════
# MAIN
# ══════════════════════════════════════════════════════════════════════════════

def run_test(test_fn, event_id, test_errors):
    """Thread target: run one test, capture exceptions."""
    try:
        test_fn(event_id)
    except Exception as exc:
        test_errors.append((test_fn.__name__, exc))
        print(f"\n  [ERROR] {test_fn.__name__}: {exc}")


def main():
    global ORACLE_NAME

    common.set_config({"async_call": "false"})
    common.setup()
    time.sleep(1)

    wait_active(GIVER_ADDRESS)
    wait_active(ROOT_PN_ADDRESS)
    wait_active(ROOT_ORACLE_ADDRESS)

    # ── Infrastructure ──
    print("\n" + "="*70)
    print("  Phase 0: Infrastructure Setup")
    print("="*70)

    common.gen_keys(ORACLE_KEY_PATH)
    common.gen_keys(EPHEMERAL_KEY_PATH)

    ORACLE_NAME = "PMPCombo1"
    oracle_addr, el_addr = setup_oracle(ORACLE_NAME)
    print("  Oracle deployed with 6 events")

    # Retrieve event_ids for each test event
    for event_name in EVENT_NAMES:
        eid = get_event_id_by_name(el_addr, event_name)
        EVENT_IDS.append(eid)
        print(f"  {event_name} -> {eid}")

    # ── Run tests in parallel ──
    print("\n" + "="*70)
    print("  Running 6 tests in parallel (one event_id each)")
    print("="*70)

    test_errors = []
    tests = [test_1, test_2, test_3, test_4, test_5, test_6]
    threads = []
    for i, (fn, eid) in enumerate(zip(tests, EVENT_IDS)):
        t = threading.Thread(target=run_test, args=(fn, eid, test_errors), daemon=True)
        threads.append(t)
        t.start()

    for t in threads:
        t.join()

    if test_errors:
        for name, exc in test_errors:
            print(f"  [EXCEPTION] {name}: {exc}")

    # ── Summary ──
    print("\n" + "="*70)
    print("  PMP TEST RESULTS")
    print("="*70)

    passed = sum(1 for r in RESULTS if r["passed"])
    failed = sum(1 for r in RESULTS if not r["passed"])

    with open(RESULTS_FILE, "w") as f:
        f.write("=" * 70 + "\n")
        f.write("  PMP TEST RESULTS\n")
        f.write("=" * 70 + "\n")
        for r in RESULTS:
            status = "PASS" if r["passed"] else "FAIL"
            f.write(f"  [{status}] {r['name']}\n")
            if r["details"]:
                f.write(f"         {r['details']}\n")
        f.write(f"\n  Total: {len(RESULTS)} | Passed: {passed} | Failed: {failed}\n")
        f.write("=" * 70 + "\n")

    print(f"\n  Total: {len(RESULTS)} | Passed: {passed} | Failed: {failed}")
    print("="*70)

    if failed > 0 or test_errors:
        print(f"\n  FAILED TESTS:")
        for r in RESULTS:
            if not r["passed"]:
                print(f"    - {r['name']}: {r['details']}")
        sys.exit(1)


if __name__ == "__main__":
    main()
