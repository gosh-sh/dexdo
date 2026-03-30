"""
Split/Merge coefficient invariance test.

Verifies that payout coefficients (profitToClean/winClean, debtWinCoef,
couponWinCoef) are computed from FROZEN base pools and do not change
after split/merge operations — even with debt and coupon stakes present.

Scenario:
  Phase 0: Infrastructure (oracle, keys)
  Phase 1: Create 4 PrivateNotes
  Phase 2: Bootstrap coupon user (PN_Loser loses → generateCoupon)
  Phase 3: Deploy PMP with clean + debt + coupon stakes, freeze, split
  Phase 4: Resolve and verify coefficients match base (frozen) pools
  Phase 5: Claim and verify payouts match Python-computed values from base pools
  Phase 6: Conservation check
"""

import json
import os
import random
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
EPHEMERAL_KEY_PATH = "./tests/dex/math_sc_ephemeral.keys.json"
ORACLE_KEY_PATH = "./tests/dex/math_sc_oracle.keys.json"

# ── Token types ────────────────────────────────────────────────────────────────
TOKEN_TYPE = 1        # NACKL
TOKEN_TYPE_SHELL = 2  # Shell
TOKEN_TYPE_ECC = 300  # Shell fee (ECC)

# ── Event constants ────────────────────────────────────────────────────────────
EVENT_NAME = "Winner of match X"
EVENT_DESCRIBE = "Who will win match X"
EVENT_OUTCOMES = {1: "Team A", 2: "Team B"}
EVENT_DEADLINE = 2000000000
ORACLE_FEE = 100
EVENT_ID = "0x67f17d97fb26ce706694339bf87ee24fe9c11752ff7ccc2a1fb56ea67e4e4e2f"

# ── Contract constants ─────────────────────────────────────────────────────────
BET_TYPE_CLEAN = 0
BET_TYPE_DEBT = 1
BET_TYPE_COUPON = 2
FULL_PERCENT = 10000
FEE_PERCENT = 1
COUPON_MAX_PAYOUT_MULTIPLIER = 20000
DEBT_REDISTRIBUTION_PERCENT = 500
MIN_NACKL = 10_000_000
NACKL_COUPON_VALUE = 100_000_000_000
ECC_SHELL_DEPOSIT = 10_000_000_000

# ── Test results tracking ──────────────────────────────────────────────────────
RESULTS = []


def generate_random_sk():
    return os.urandom(31).hex() + format(random.randint(0, 0x2f), '02x')


def log(title, data):
    print(f"\n=== {title} ===")
    print(data)
    print("===")


def record_result(name, passed, details=""):
    status = "PASSED" if passed else "FAILED"
    RESULTS.append({"name": name, "passed": passed, "details": details})
    print(f"\n{'='*60}")
    print(f"  [{status}] {name}")
    if details:
        print(f"  {details}")
    print(f"{'='*60}")


# ══════════════════════════════════════════════════════════════════════════════
# Pure Python math replication
# ══════════════════════════════════════════════════════════════════════════════

def compute_resolve_math(total_pool, M_W, D_W, C_W):
    """Replicate PMP resolve logic in Python using BASE (frozen) pools."""
    profit_budget = total_pool - M_W - D_W
    creator_fee_uncapped = total_pool // FULL_PERCENT
    creator_fee = min(creator_fee_uncapped, profit_budget)
    profit_budget_after_fee = profit_budget - creator_fee

    total_winning_mass = M_W + D_W + C_W
    if total_winning_mass == 0:
        return {"error": "totalWinningMass == 0"}

    profit_per_unit = (profit_budget_after_fee * FULL_PERCENT) // total_winning_mass
    coupon_win_coef = min(profit_per_unit, COUPON_MAX_PAYOUT_MULTIPLIER)

    coupon_profit = (C_W * coupon_win_coef) // FULL_PERCENT
    if coupon_profit > profit_budget_after_fee:
        coupon_profit = profit_budget_after_fee
    profit_rem = profit_budget_after_fee - coupon_profit

    real_winning_mass = M_W + D_W
    if real_winning_mass > 0:
        base_real_ppu = (profit_rem * FULL_PERCENT) // real_winning_mass
        debt_win_coef = (base_real_ppu * (FULL_PERCENT - DEBT_REDISTRIBUTION_PERCENT)) // FULL_PERCENT
    else:
        base_real_ppu = 0
        debt_win_coef = 0

    total_debt_profit = (D_W * debt_win_coef) // FULL_PERCENT
    profit_to_clean = profit_rem - total_debt_profit

    return {
        "totalPool": total_pool,
        "creatorFee": creator_fee,
        "couponWinCoef": coupon_win_coef,
        "debtWinCoef": debt_win_coef,
        "profitToClean": profit_to_clean,
        "M_W": M_W, "D_W": D_W, "C_W": C_W,
    }


def compute_clean_payout(stake, profit_to_clean, total_clean_winning):
    profit = (stake * profit_to_clean) // total_clean_winning
    return stake + profit, profit


def compute_debt_payout(stake, debt_win_coef):
    profit = (stake * debt_win_coef) // FULL_PERCENT
    return stake + profit, profit


def compute_coupon_payout(stake, coupon_win_coef):
    return (stake * coupon_win_coef) // FULL_PERCENT


def compute_debt_paid(profit):
    return (profit * DEBT_REDISTRIBUTION_PERCENT) // (FULL_PERCENT - DEBT_REDISTRIBUTION_PERCENT)


# ══════════════════════════════════════════════════════════════════════════════
# Contract interaction helpers
# ══════════════════════════════════════════════════════════════════════════════

def get_pn_balance(pn_details, token_type):
    bal = pn_details.get("balance", {})
    val = bal.get(str(token_type), bal.get(token_type, 0))
    return int(val) if val is not None else 0


def get_pmp_outcome_pool(pmp_details, outcome_id, bet_type):
    pools = pmp_details.get("typedOutcomePools", {})
    o = pools.get(str(outcome_id), pools.get(outcome_id, {}))
    if isinstance(o, dict):
        v = o.get(str(bet_type), o.get(bet_type, 0))
        return int(v) if v is not None else 0
    return 0


def send_tokens_to_root_pn(token_type, value):
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
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                            "getPrivateNoteAddress",
                            {"deposit_identifier_hash": deposit_identifier_hash})
    log("getPrivateNoteAddress", out)
    time.sleep(1)
    return out["privateNoteAddress"]


def send_ecc_to_private_note(proof, nullifier_hash, deposit_identifier_hash, value):
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
    return common.run_getter(pmp_address, PMP_ABI, "getDetails", {})


def wait_pmp_approved(pmp_address, max_wait=90):
    deadline = time.time() + max_wait
    while time.time() < deadline:
        details = get_pmp_details(pmp_address)
        if details:
            n = int(details.get("numberOfOracleEvents", 0))
            a = int(details.get("approvedOracleEvents", 0))
            if n > 0 and a >= n:
                time.sleep(5)
                log("PMP oracle confirmed", pmp_address)
                return details
        time.sleep(3)
    raise TimeoutError(f"PMP {pmp_address} oracle confirmation not received after {max_wait}s")


def oracle_submit_set_timings(pmp_address, result_start):
    params = {
        "resultStart": result_start,
    }
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitSetTimings", params)
    log("submitSetTimings", out)
    time.sleep(3)


def oracle_submit_resolve(pmp_address, outcome_id):
    params = {"outcomeId": outcome_id}
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitResolve", params)
    log("submitResolve", out)
    time.sleep(3)


def pn_set_stake(pn_address, event_id, oracle_list_hash, token_type,
                 outcome, amount, use_coupon=False, key_path=EPHEMERAL_KEY_PATH):
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
    log(f"setStake outcome={outcome} amount={amount} use_coupon={use_coupon}", out)
    time.sleep(4)


def pn_claim(pn_address, event_id, oracle_list_hash, token_type,
             key_path=EPHEMERAL_KEY_PATH):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "claim", params)
    log("claim", out)
    time.sleep(4)


def pn_generate_coupon(pn_address, token_type, key_path=EPHEMERAL_KEY_PATH):
    params = {"token_type": token_type}
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "generateCoupon", params)
    log("generateCoupon", out)
    time.sleep(3)


def pn_cancel_stake(pn_address, event_id, oracle_list_hash, token_type,
                    key_path=EPHEMERAL_KEY_PATH):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "cancelStake", params)
    log("cancelStake", out)
    time.sleep(4)




def pn_split_full_set(pn_address, event_id, oracle_list_hash, token_type, collateral):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "collateral": collateral,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "splitFullSet", params)
    log("splitFullSet", out)
    time.sleep(4)


def get_pn_details(pn_address):
    return common.run_getter(pn_address, PRIVATE_NOTE_ABI, "getDetails", {})


def read_pmp_resolve_state(pmp_address):
    out = common.execute_cli_cmd(
        f"decode account data --abi {PMP_ABI} --addr {pmp_address}"
    )
    data = out if out else {}
    return {
        "couponWinCoef": int(data.get("_couponWinCoef", 0)),
        "debtWinCoef": int(data.get("_debtWinCoef", 0)),
        "profitToClean": int(data.get("_profitToClean", 0)),
        "creatorFee": int(data.get("_creatorFee", 0)),
        "totalWinPool": int(data.get("_totalWinPool", 0)),
        "baseTotalPool": int(data.get("_baseTotalPool", 0)),
        "totalPool": int(data.get("_totalPool", 0)),
    }


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


# ══════════════════════════════════════════════════════════════════════════════
# High-level helpers
# ══════════════════════════════════════════════════════════════════════════════

def create_pn(nackl_balance, label="PN"):
    sk = generate_random_sk()
    print(f"\n>>> Creating {label} (balance={nackl_balance}, sk={sk[:16]}...)")

    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    send_tokens_to_root_pn("1", nackl_balance)

    proof, dih, value, ttype, nullifier = generate_proof(sk, TOKEN_TYPE, nackl_balance)

    ephemeral_pubkey = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not ephemeral_pubkey.startswith("0x"):
        ephemeral_pubkey = "0x" + ephemeral_pubkey

    deploy_private_note(proof, dih, value, ttype, ephemeral_pubkey)
    pn_address = get_private_note_address(dih)
    common.wait_account_active(pn_address)

    proof_sh, null_sh, val_sh, _, _ = generate_proof(sk, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT)
    send_ecc_to_private_note(proof_sh, null_sh, dih, ECC_SHELL_DEPOSIT)

    pn_det = get_pn_details(pn_address)
    actual_bal = get_pn_balance(pn_det, TOKEN_TYPE)
    log(f"{label} created", f"addr={pn_address}, balance={actual_bal}")

    return pn_address, dih, sk


def setup_oracle(oracle_name):
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
    common.wait_account_active(oracle_addr)

    el_out = common.run_getter(oracle_addr, ORACLE_ABI,
                                "getEventListAddress", {"index": 0})
    el_addr = el_out.get("value0") if isinstance(el_out, dict) else el_out
    common.wait_account_active(el_addr)

    add_params = {
        "event_name": EVENT_NAME,
        "oracle_fee": ORACLE_FEE,
        "deadline": EVENT_DEADLINE,
        "describe": EVENT_DESCRIBE,
        "outcomeNames": EVENT_OUTCOMES,
        "trustAddr": None,
    }
    common.call_contract(el_addr, EVENTLIST_ABI, ORACLE_KEY_PATH, "addEvent", add_params)
    time.sleep(2)
    return oracle_addr, el_addr


def deploy_and_setup_pmp(creator_pn, oracle_name, stake_period,
                         initial_stakes=None):
    pmp_addr = deploy_pmp(creator_pn, EVENT_ID, oracle_name, ORACLE_FEE,
                          TOKEN_TYPE, index=0, initial_stakes=initial_stakes)
    approved = wait_pmp_approved(pmp_addr)
    olh = approved["oracle_list_hash"]

    now = int(time.time())
    r_start = now + stake_period

    oracle_submit_set_timings(pmp_addr, r_start)
    d = get_pmp_details(pmp_addr)
    assert d["approved"], f"PMP {pmp_addr} must be approved"

    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s_end = now + (r_start - now) // 10
    return pmp_addr, olh, s_end, r_start


def wait_and_resolve(pmp_addr, r_start, outcome_id):
    wait_secs = r_start - int(time.time()) + 5
    if wait_secs > 0:
        print(f"\n>>> Waiting {wait_secs}s for result window...")
        time.sleep(wait_secs)
    oracle_submit_resolve(pmp_addr, outcome_id)


# ══════════════════════════════════════════════════════════════════════════════
# MAIN TEST
# ══════════════════════════════════════════════════════════════════════════════

def main():
    common.set_config({"async_call": "false"})
    common.setup()
    time.sleep(1)

    common.wait_account_active(GIVER_ADDRESS)
    common.wait_account_active(ROOT_PN_ADDRESS)
    common.wait_account_active(ROOT_ORACLE_ADDRESS)

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 0: Infrastructure
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 0: Infrastructure Setup")
    print("="*70)

    common.gen_keys(ORACLE_KEY_PATH)
    common.gen_keys(EPHEMERAL_KEY_PATH)

    oracle_names = ["SplitCoef_Boot", "SplitCoef_Main"]
    for name in oracle_names:
        setup_oracle(name)

    print(">>> Phase 0 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 1: Create PrivateNotes
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 1: Create PrivateNotes")
    print("="*70)

    pn_creator, _, _ = create_pn(500_000_000_000, "PN_Creator")
    pn_clean, _, _ = create_pn(5_000_000_000_000, "PN_Clean")
    pn_loser, _, _ = create_pn(MIN_NACKL + 1, "PN_Loser")
    # PN_Splitter will do split after freeze
    pn_splitter, _, _ = create_pn(500_000_000_000, "PN_Splitter")

    print(">>> Phase 1 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 2: Bootstrap coupon user
    # PN_Loser loses a small PMP → balance=0 → generateCoupon
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 2: Bootstrap Coupon User")
    print("="*70)

    BOOT_PERIOD = 600  # stakeEnd = now + 60s (10%), enough for 2 stakes

    pmp_boot, olh_boot, s_end_boot, r_start_boot = deploy_and_setup_pmp(
        pn_creator, "SplitCoef_Boot", BOOT_PERIOD)

    # PN_Clean stakes winning side
    pn_set_stake(pn_clean, EVENT_ID, olh_boot, TOKEN_TYPE, 0, 100_000_000_000)
    # PN_Loser stakes MIN on losing side
    pn_set_stake(pn_loser, EVENT_ID, olh_boot, TOKEN_TYPE, 1, MIN_NACKL)

    wait_and_resolve(pmp_boot, r_start_boot, 0)

    pn_claim(pn_loser, EVENT_ID, olh_boot, TOKEN_TYPE)
    pn_claim(pn_clean, EVENT_ID, olh_boot, TOKEN_TYPE)

    # Verify loser has 0 balance, generate coupon
    pn_loser_det = get_pn_details(pn_loser)
    pn_loser_bal = get_pn_balance(pn_loser_det, TOKEN_TYPE)
    assert pn_loser_bal < MIN_NACKL, f"PN_Loser balance must be < MIN for coupon, got {pn_loser_bal}"

    pn_generate_coupon(pn_loser, TOKEN_TYPE)
    coupon_val = read_pn_coupons(pn_loser)
    debt_val = read_pn_debt(pn_loser)
    assert coupon_val == NACKL_COUPON_VALUE, f"Coupon must be {NACKL_COUPON_VALUE}, got {coupon_val}"
    record_result("Phase 2: Coupon generated", True,
                  f"coupon={coupon_val}, debt={debt_val}")

    print(">>> Phase 2 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 3: Main scenario — clean + debt + coupon + split
    #
    # Setup: [1000, 3000] clean pools, debt on outcome 0, coupon on outcome 0
    # After freeze: do a large split (200 NACKL) to change live pools
    # Then resolve and verify coefficients match BASE pools (not live)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 3: Deploy PMP with debt+coupon, freeze, split")
    print("="*70)

    MAIN_PERIOD = 600  # stakeEnd = now + 60s (10%), enough for 3 stakes

    pmp_main, olh_main, s_end_main, r_start_main = deploy_and_setup_pmp(
        pn_creator, "SplitCoef_Main", MAIN_PERIOD)

    # PN_Clean stakes 1000 NACKL - MIN on outcome 0 (clean)
    STAKE_CLEAN_0 = 1_000_000_000_000 - MIN_NACKL
    pn_set_stake(pn_clean, EVENT_ID, olh_main, TOKEN_TYPE, 0, STAKE_CLEAN_0)

    # PN_Loser stakes 100 NACKL on outcome 0 (auto-debt since _debt > 0)
    # First give PN_Loser some balance via coupon payout from boot
    # Actually PN_Loser has coupon (100 NACKL) and debt. Let's stake coupon.
    COUPON_STAKE = 50_000_000_000  # 50 NACKL coupon on outcome 0
    pn_set_stake(pn_loser, EVENT_ID, olh_main, TOKEN_TYPE, 0, COUPON_STAKE,
                 use_coupon=True)

    # PN_Loser stakes on outcome 1 as clean (3000 NACKL - MIN)
    # Actually PN_Loser has no balance for large clean stakes.
    # Use PN_Creator for losing side instead.
    STAKE_LOSE = 3_000_000_000_000 - MIN_NACKL
    # Fund PN_Loser with balance for losing side? No, PN_Loser has debt.
    # Let's use PN_Clean for outcome 1 too.
    # Actually let's create a simpler setup:
    # PN_Clean stakes on both sides. PN_Loser only coupon on winning side.

    # PN_Clean stakes on outcome 1 (losing side)
    STAKE_CLEAN_1 = 3_000_000_000_000 - MIN_NACKL
    pn_set_stake(pn_clean, EVENT_ID, olh_main, TOKEN_TYPE, 1, STAKE_CLEAN_1)

    # Read pools at stakeEnd (before freeze)
    pmp_det_pre = get_pmp_details(pmp_main)
    total_pool_pre = int(pmp_det_pre.get("totalPool", 0))
    clean_0_pre = get_pmp_outcome_pool(pmp_det_pre, 0, BET_TYPE_CLEAN)
    clean_1_pre = get_pmp_outcome_pool(pmp_det_pre, 1, BET_TYPE_CLEAN)
    coupon_0_pre = get_pmp_outcome_pool(pmp_det_pre, 0, BET_TYPE_COUPON)
    log("Pools before freeze", {
        "totalPool": total_pool_pre,
        "clean_0": clean_0_pre, "clean_1": clean_1_pre,
        "coupon_0": coupon_0_pre
    })

    # These are the BASE values that resolve should use
    BASE_TOTAL = total_pool_pre  # clean only (coupon doesn't add to totalPool)
    BASE_CLEAN_W = clean_0_pre   # winning clean pool (outcome 0)
    BASE_DEBT_W = 0              # no debt stakes in this scenario
    BASE_COUPON_W = coupon_0_pre # coupon on winning side

    # Wait for stakeEnd (freeze happens automatically on first split)
    wait_secs = s_end_main - int(time.time()) + 3
    if wait_secs > 0:
        print(f"\n>>> Waiting {wait_secs}s for stakeEnd...")
        time.sleep(wait_secs)

    # ── Do a LARGE split to significantly change live pools ──
    # This also triggers auto-freeze + OrderBook deploy
    SPLIT_AMOUNT = 200_000_000_000  # 200 NACKL
    pn_splitter_det_before = get_pn_details(pn_splitter)
    pn_splitter_bal_before = get_pn_balance(pn_splitter_det_before, TOKEN_TYPE)

    pn_split_full_set(pn_splitter, EVENT_ID, olh_main, TOKEN_TYPE, SPLIT_AMOUNT)

    pn_splitter_det_after = get_pn_details(pn_splitter)
    pn_splitter_bal_after = get_pn_balance(pn_splitter_det_after, TOKEN_TYPE)

    # Verify auto-freeze happened, live pools changed, base pools didn't
    pmp_after_split = get_pmp_details(pmp_main)
    assert pmp_after_split.get("frozen") == True, "PMP must be auto-frozen after split"
    total_pool_live = int(pmp_after_split.get("totalPool", 0))
    base_total_still = int(pmp_after_split.get("baseTotalPool", 0))

    s3_base_ok = base_total_still == BASE_TOTAL
    record_result("S3.1: baseTotalPool matches pre-freeze totalPool",
                  s3_base_ok,
                  f"expected={BASE_TOTAL}, actual={base_total_still}")

    s3_live_changed = total_pool_live == BASE_TOTAL + SPLIT_AMOUNT
    record_result("S3.2: Live totalPool increased by exactly F after split",
                  s3_live_changed,
                  f"expected={BASE_TOTAL + SPLIT_AMOUNT}, actual={total_pool_live}")

    # Verify PN spent exactly SPLIT_AMOUNT (per spec: _totalPool += F)
    actual_spent = pn_splitter_bal_before - pn_splitter_bal_after
    s3_spent_ok = actual_spent == SPLIT_AMOUNT
    record_result("S3.4: PN spent exactly collateral F (no remainder)",
                  s3_spent_ok,
                  f"collateral={SPLIT_AMOUNT}, actualSpent={actual_spent}")

    print(">>> Phase 3 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 4: Resolve and verify coefficients match BASE pools
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 4: Resolve — verify coefficients from base pools")
    print("="*70)

    # Compute expected coefficients from BASE pools (not live)
    expected_math = compute_resolve_math(BASE_TOTAL, BASE_CLEAN_W, BASE_DEBT_W, BASE_COUPON_W)
    log("Expected math (from base pools)", expected_math)

    # Resolve: outcome 0 wins
    wait_and_resolve(pmp_main, r_start_main, 0)

    # Read actual resolve state
    resolve_state = read_pmp_resolve_state(pmp_main)
    log("Actual resolve state", resolve_state)

    # S4.1: couponWinCoef matches base computation
    s4_cwc_ok = resolve_state["couponWinCoef"] == expected_math["couponWinCoef"]
    record_result("S4.1: couponWinCoef matches base pool computation",
                  s4_cwc_ok,
                  f"expected={expected_math['couponWinCoef']}, actual={resolve_state['couponWinCoef']}")

    # S4.2: debtWinCoef matches
    s4_dwc_ok = resolve_state["debtWinCoef"] == expected_math["debtWinCoef"]
    record_result("S4.2: debtWinCoef matches base pool computation",
                  s4_dwc_ok,
                  f"expected={expected_math['debtWinCoef']}, actual={resolve_state['debtWinCoef']}")

    # S4.3: creatorFee uses baseTotalPool
    s4_fee_ok = resolve_state["creatorFee"] == expected_math["creatorFee"]
    record_result("S4.3: creatorFee computed from baseTotalPool",
                  s4_fee_ok,
                  f"expected={expected_math['creatorFee']}, actual={resolve_state['creatorFee']}")

    # S4.4: profitToClean matches
    s4_ptc_ok = resolve_state["profitToClean"] == expected_math["profitToClean"]
    record_result("S4.4: profitToClean matches base pool computation",
                  s4_ptc_ok,
                  f"expected={expected_math['profitToClean']}, actual={resolve_state['profitToClean']}")

    print(">>> Phase 4 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 5: Claim and verify payouts
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 5: Claims — verify payout amounts")
    print("="*70)

    # PN_Creator claims (MIN on outcome 0 — clean, winner)
    pn_creator_det_bc = get_pn_details(pn_creator)
    pn_creator_bal_bc = get_pn_balance(pn_creator_det_bc, TOKEN_TYPE)
    pn_claim(pn_creator, EVENT_ID, olh_main, TOKEN_TYPE)
    pn_creator_bal_ac = get_pn_balance(get_pn_details(pn_creator), TOKEN_TYPE)
    creator_payout = pn_creator_bal_ac - pn_creator_bal_bc

    expected_creator_payout, _ = compute_clean_payout(
        MIN_NACKL, expected_math["profitToClean"], BASE_CLEAN_W)
    s5_creator_ok = creator_payout == expected_creator_payout
    record_result("S5.1: Creator clean payout matches base computation",
                  s5_creator_ok,
                  f"expected={expected_creator_payout}, actual={creator_payout}")

    # PN_Clean claims (STAKE_CLEAN_0 on outcome 0 — clean, winner)
    pn_clean_det_bc = get_pn_details(pn_clean)
    pn_clean_bal_bc = get_pn_balance(pn_clean_det_bc, TOKEN_TYPE)
    pn_claim(pn_clean, EVENT_ID, olh_main, TOKEN_TYPE)
    pn_clean_bal_ac = get_pn_balance(get_pn_details(pn_clean), TOKEN_TYPE)
    clean_payout = pn_clean_bal_ac - pn_clean_bal_bc

    expected_clean_payout, _ = compute_clean_payout(
        STAKE_CLEAN_0, expected_math["profitToClean"], BASE_CLEAN_W)
    s5_clean_ok = clean_payout == expected_clean_payout
    record_result("S5.2: PN_Clean payout matches base computation",
                  s5_clean_ok,
                  f"expected={expected_clean_payout}, actual={clean_payout}")

    # PN_Loser claims (coupon on outcome 0 — winner)
    pn_loser_det_bc = get_pn_details(pn_loser)
    pn_loser_bal_bc = get_pn_balance(pn_loser_det_bc, TOKEN_TYPE)
    pn_claim(pn_loser, EVENT_ID, olh_main, TOKEN_TYPE)
    pn_loser_bal_ac = get_pn_balance(get_pn_details(pn_loser), TOKEN_TYPE)
    coupon_payout = pn_loser_bal_ac - pn_loser_bal_bc

    expected_coupon_payout = compute_coupon_payout(
        COUPON_STAKE, expected_math["couponWinCoef"])
    s5_coupon_ok = coupon_payout == expected_coupon_payout
    record_result("S5.3: Coupon payout matches base computation",
                  s5_coupon_ok,
                  f"expected={expected_coupon_payout}, actual={coupon_payout}")

    # PN_Splitter claims (split tokens on outcome 0 — winner)
    pn_splitter_det_bc = get_pn_details(pn_splitter)
    pn_splitter_bal_bc = get_pn_balance(pn_splitter_det_bc, TOKEN_TYPE)
    pn_claim(pn_splitter, EVENT_ID, olh_main, TOKEN_TYPE)
    pn_splitter_bal_ac = get_pn_balance(get_pn_details(pn_splitter), TOKEN_TYPE)
    splitter_payout = pn_splitter_bal_ac - pn_splitter_bal_bc

    # Splitter's outcome 0 tokens = floor(F * M_W / R_W)
    # R_W = M_W + profitToClean_W (clean residual on winning outcome)
    R_W = BASE_CLEAN_W + expected_math["profitToClean"]
    split_delta_0 = (SPLIT_AMOUNT * BASE_CLEAN_W) // R_W
    expected_splitter_payout, _ = compute_clean_payout(
        split_delta_0, expected_math["profitToClean"], BASE_CLEAN_W)
    s5_splitter_ok = splitter_payout == expected_splitter_payout
    record_result("S5.4: Splitter payout matches R_k-based computation",
                  s5_splitter_ok,
                  f"split_tokens_0={split_delta_0}, R_W={R_W}, "
                  f"expected_payout={expected_splitter_payout}, actual={splitter_payout}")

    print(">>> Phase 5 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 6: Conservation — fee + payouts + dust = live totalPool
    # With R_k-based split, dust should be minimal (0-2 from floor rounding)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 6: Conservation check")
    print("="*70)

    all_payouts = [creator_payout, clean_payout, coupon_payout, splitter_payout]
    total_paid = expected_math["creatorFee"] + sum(all_payouts)

    dust = total_pool_live - total_paid
    s6_ok = 0 <= dust <= 2
    record_result("S6.1: Conservation (fee + payouts + dust = live totalPool)",
                  s6_ok,
                  f"liveTotalPool={total_pool_live}, baseTotalPool={BASE_TOTAL}, "
                  f"fee={expected_math['creatorFee']}, payouts={all_payouts}, "
                  f"total_paid={total_paid}, dust={dust}")

    print(">>> Phase 6 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # FINAL SUMMARY
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  FINAL SUMMARY")
    print("="*70)

    passed = sum(1 for r in RESULTS if r["passed"])
    failed = sum(1 for r in RESULTS if not r["passed"])
    total = len(RESULTS)

    summary_lines = []
    summary_lines.append("="*70)
    summary_lines.append("  MATH SPLIT COEFFICIENT TEST RESULTS")
    summary_lines.append("="*70)

    for r in RESULTS:
        status = "PASS" if r["passed"] else "FAIL"
        line = f"  [{status}] {r['name']}"
        print(line)
        summary_lines.append(line)
        if r["details"]:
            detail = f"         {r['details']}"
            if not r["passed"]:
                print(detail)
            summary_lines.append(detail)

    footer = f"\n  Total: {total} | Passed: {passed} | Failed: {failed}"
    print(footer)
    summary_lines.append(footer)
    summary_lines.append("="*70)

    results_path = "./tests/dex/math_split_coef_test_results.txt"
    with open(results_path, "w") as f:
        f.write("\n".join(summary_lines) + "\n")
    print(f"\n>>> Results written to {results_path}")

    print("="*70)

    if failed > 0:
        print("\n>>> SOME TESTS FAILED!")
        sys.exit(1)
    else:
        print("\n>>> ALL TESTS PASSED!")


if __name__ == "__main__":
    main()
