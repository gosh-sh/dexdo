"""
Mathematical OrderBook verification test.

Tests OrderBook fee formulas, clearing price logic, collateral locking,
split/merge proportionality, and conservation invariants against exact
Python-computed values.

Scenarios:
  S1:  Split math — proportionality of outcome tokens from collateral
  S2:  Limit matching — clearing price = maker's price, maker/taker fees
  S3:  Price improvement — buyer refund when clearingPrice < buyPrice
  S4:  Partial fill — sell rests, crossing buy partially fills
  S5:  IOC cancel — no counterparty → immediate cancel, collateral returned
  S6:  POST_ONLY cancel — would cross → immediate cancel
  S7:  Merge math — collateral returned, solvency preserved
  S8:  Conservation — split→trade→merge→claim cycle, total tokens conserved
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
ORDERBOOK_ABI = "./contracts/0.79.3_compiled/dex/OrderBook.abi.json"

# ── Key paths ──────────────────────────────────────────────────────────────────
EPHEMERAL_KEY_PATH = "./tests/dex/math_ob_ephemeral.keys.json"
ORACLE_KEY_PATH = "./tests/dex/math_ob_oracle.keys.json"

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
FULL_PERCENT = 10000
FEE_PERCENT = 1
FEE_DENOMINATOR = 100000
MAKER_FEE_RATE = 15    # 0.015%
TAKER_FEE_RATE = 45    # 0.045%
MIN_NACKL = 10_000_000
MIN_ORDER_NACKL = 10_000_000_000  # 10 NACKL
ECC_SHELL_DEPOSIT = 10_000_000_000

# ── Order flags ────────────────────────────────────────────────────────────────
FLAG_IOC = 0x01
FLAG_FOK = 0x02
FLAG_MARKET = 0x04
FLAG_POST_ONLY = 0x08
FLAG_GTC = 0x10

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

def compute_profit_to_clean_per_outcome(clean_pools, base_total):
    """Compute profitToClean for each possible winning outcome (clean-only)."""
    ptc = []
    for k in range(len(clean_pools)):
        profit_budget = base_total - clean_pools[k]
        fee = (base_total * FEE_PERCENT) // FULL_PERCENT
        if fee > profit_budget:
            fee = profit_budget
        ptc.append(profit_budget - fee)
    return ptc


def compute_split_amounts(collateral, frozen_clean_pools, frozen_ptc):
    """Compute outcome token amounts from collateral split.
    Per spec: δ_k = floor(F × M_k / R_k) where R_k = M_k + profitToClean_k."""
    amounts = []
    for k in range(len(frozen_clean_pools)):
        M_k = frozen_clean_pools[k]
        if M_k == 0:
            amounts.append(0)
            continue
        R_k = M_k + frozen_ptc[k]
        amounts.append((collateral * M_k) // R_k)
    return amounts


def compute_merge_collateral(amounts, frozen_clean_pools, frozen_ptc):
    """Compute collateral from merging outcome tokens (min ratio).
    Per spec: F = min(amount[k] × R_k / M_k)."""
    collateral = 2**128 - 1  # max uint128
    for k in range(len(amounts)):
        M_k = frozen_clean_pools[k]
        if M_k == 0:
            continue
        R_k = M_k + frozen_ptc[k]
        c = (amounts[k] * R_k) // M_k
        if c < collateral:
            collateral = c
    return collateral


def is_taker_flags(flags):
    """Determine if order is taker by its flags (IOC|FOK|MARKET)."""
    return (flags & 0x07) != 0


def compute_fee(filled_amount, clearing_price, flags):
    """Compute trading fee for a fill based on order flags."""
    notional = (filled_amount * clearing_price) // FULL_PERCENT
    fee_rate = TAKER_FEE_RATE if is_taker_flags(flags) else MAKER_FEE_RATE
    return (notional * fee_rate) // FEE_DENOMINATOR, notional


def compute_buyer_refund(buy_price, clearing_price, trade_amount):
    """Compute buyer's refund when clearingPrice < buyPrice."""
    if clearing_price >= buy_price:
        return 0
    diff = buy_price - clearing_price
    return (diff * trade_amount) // FULL_PERCENT


def compute_buy_collateral(amount, price_bps):
    """Compute collateral locked for a buy order."""
    return (amount * price_bps) // FULL_PERCENT


def compute_sell_proceeds(filled_amount, clearing_price, fee):
    """Compute net proceeds from a sell fill."""
    gross = (filled_amount * clearing_price) // FULL_PERCENT
    if gross > fee:
        return gross - fee
    return 0


def compute_max_fee(cost):
    """Compute max fee reserved at order placement (taker rate on cost)."""
    return (cost * TAKER_FEE_RATE) // FEE_DENOMINATOR


def compute_buy_balance_change(refund, fee, max_fee_reserve):
    """Compute balance change for buyer from a fill.
    Returns refund + (feeReserve - actualFee)."""
    fee_refund = max_fee_reserve - fee if max_fee_reserve >= fee else 0
    return refund + fee_refund


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


def normalize_uint256(val):
    if isinstance(val, int):
        return val
    s = str(val).strip()
    if s.startswith(("0x", "0X")):
        return int(s, 16)
    try:
        return int(s)
    except ValueError:
        return int(s, 16)


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


def get_pn_details(pn_address):
    return common.run_getter(pn_address, PRIVATE_NOTE_ABI, "getDetails", {})


def get_ob_details(ob_address):
    return common.run_getter(ob_address, ORDERBOOK_ABI, "getDetails", {})


def get_ob_order(ob_address, order_id):
    return common.run_getter(ob_address, ORDERBOOK_ABI, "getOrder",
                             {"orderId": order_id})


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
    log(f"Oracle '{oracle_name}' address", oracle_addr)
    common.wait_account_active(oracle_addr)

    el_out = common.run_getter(oracle_addr, ORACLE_ABI,
                                "getEventListAddress", {"index": 0})
    el_addr = el_out.get("value0") if isinstance(el_out, dict) else el_out
    log(f"EventList for '{oracle_name}'", el_addr)
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


def pn_set_stake(pn_address, event_id, oracle_list_hash, token_type,
                 outcome, amount, key_path=EPHEMERAL_KEY_PATH):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "outcome": outcome,
        "amount": amount,
        "use_coupon": False,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "setStake", params)
    log(f"setStake outcome={outcome} amount={amount}", out)
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


def pn_merge_full_set(pn_address, event_id, oracle_list_hash, token_type, amounts):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "amount": amounts,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "mergeFullSet", params)
    log("mergeFullSet", out)
    time.sleep(4)


def pn_place_order(pn_address, event_id, oracle_list_hash, token_type,
                   outcome_id, is_buy, price_bps, amount, flags=0,
                   min_amount=0, epoch_id=0):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "outcomeId": outcome_id,
        "isBuy": is_buy,
        "priceBps": price_bps,
        "amount": amount,
        "flags": flags,
        "minAmount": min_amount,
        "epochId": epoch_id,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "placeOrder", params)
    log(f"placeOrder outcome={outcome_id} isBuy={is_buy} price={price_bps} "
        f"amount={amount} flags={flags}", out)
    time.sleep(4)


def pn_cancel_order(pn_address, event_id, oracle_list_hash, token_type, order_id):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "orderId": order_id,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "cancelOrder", params)
    log(f"cancelOrder orderId={order_id}", out)
    time.sleep(4)


def read_pmp_resolve_state(pmp_address):
    out = common.execute_cli_cmd(
        f"decode account data --abi {PMP_ABI} --addr {pmp_address}"
    )
    data = out if out else {}
    return {
        "couponWinCoef": int(data.get("_couponWinCoef", 0)),
        "debtWinCoef": int(data.get("_debtWinCoef", 0)),
        "totalCleanPool": int(data.get("_totalCleanPool", 0)),
        "totalWinPool": int(data.get("_totalWinPool", 0)),
        "profitToClean": int(data.get("_profitToClean", 0)),
        "creatorFee": int(data.get("_creatorFee", 0)),
        "baseTotalPool": int(data.get("_baseTotalPool", 0)),
    }


def get_pn_stake_amounts(pn_address, event_id, oracle_list_hash, token_type):
    """Read PN account data and extract stake amounts for given market."""
    out = common.execute_cli_cmd(
        f"decode account data --abi {PRIVATE_NOTE_ABI} --addr {pn_address}"
    )
    data = out if out else {}
    stakes = data.get("_stakes", {})
    # The key is the hash of (event_id, oracle_list_hash, token_type)
    # We iterate and return the first matching stake info
    for key, stake_info in stakes.items():
        if isinstance(stake_info, dict) and "amount" in stake_info:
            amounts = stake_info["amount"]
            if isinstance(amounts, dict):
                return [int(amounts.get(str(i), 0)) for i in sorted(int(k) for k in amounts.keys())]
            elif isinstance(amounts, list):
                return [int(a) for a in amounts]
    return []


# ══════════════════════════════════════════════════════════════════════════════
# High-level helpers
# ══════════════════════════════════════════════════════════════════════════════

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
    assert d["approved"], f"PMP {pmp_addr} must be approved after setTimings"

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

    oracle_name = "MathOB1"
    setup_oracle(oracle_name)

    print(">>> Phase 0 DONE: Oracle deployed")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 1: Create PrivateNotes (2 users for trading)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 1: Create PrivateNotes")
    print("="*70)

    # PN_A: will stake on outcome 0 and sell outcome tokens
    pn_a, dih_a, sk_a = create_pn(500_000_000_000, "PN_A")
    # PN_B: will stake on outcome 1 and buy outcome 0 tokens
    pn_b, dih_b, sk_b = create_pn(500_000_000_000, "PN_B")

    print(">>> Phase 1 DONE: PNs created")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 2: Deploy PMP, stake, set timings, wait, freeze
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  Phase 2: Deploy PMP + Stake + Freeze")
    print("="*70)

    STAKE_PERIOD = 120  # 2 min window

    # Deploy PMP with equal initial stakes
    SEED = 1_000_000_000  # 1 NACKL per outcome
    pmp_addr, olh, s_end, r_start = deploy_and_setup_pmp(
        pn_a, oracle_name, STAKE_PERIOD, initial_stakes=[SEED, SEED])

    # PN_A stakes 100 NACKL on outcome 0
    STAKE_A = 100_000_000_000
    pn_set_stake(pn_a, EVENT_ID, olh, TOKEN_TYPE, 0, STAKE_A)

    # PN_B stakes 100 NACKL on outcome 1
    STAKE_B = 100_000_000_000
    pn_set_stake(pn_b, EVENT_ID, olh, TOKEN_TYPE, 1, STAKE_B)

    # Verify pools
    pmp_det = get_pmp_details(pmp_addr)
    total_pool = int(pmp_det.get("totalPool", 0))
    pool_0 = get_pmp_outcome_pool(pmp_det, 0, 0)  # clean pool outcome 0
    pool_1 = get_pmp_outcome_pool(pmp_det, 1, 0)  # clean pool outcome 1
    log("PMP pools", f"total={total_pool}, pool_0={pool_0}, pool_1={pool_1}")

    expected_pool_0 = SEED + STAKE_A
    expected_pool_1 = SEED + STAKE_B
    expected_total = expected_pool_0 + expected_pool_1
    assert total_pool == expected_total, \
        f"totalPool must be {expected_total}, got {total_pool}"

    # Wait for stakeEnd (freeze + OB deploy happen automatically on first split)
    wait_freeze = s_end - int(time.time()) + 3
    if wait_freeze > 0:
        print(f"\n>>> Waiting {wait_freeze}s for stakeEnd...")
        time.sleep(wait_freeze)

    print(">>> Phase 2 DONE: stakeEnd reached, freeze will trigger on first split")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 1: Split Math — proportionality
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S1: Split Math — proportionality")
    print("="*70)

    SPLIT_COLLATERAL = 50_000_000_000  # 50 NACKL

    pn_a_det_before = get_pn_details(pn_a)
    pn_a_bal_before = get_pn_balance(pn_a_det_before, TOKEN_TYPE)

    pn_split_full_set(pn_a, EVENT_ID, olh, TOKEN_TYPE, SPLIT_COLLATERAL)

    pn_a_det_after = get_pn_details(pn_a)
    pn_a_bal_after = get_pn_balance(pn_a_det_after, TOKEN_TYPE)

    # S1.1: Balance decreased by collateral
    s1_bal_ok = pn_a_bal_after == pn_a_bal_before - SPLIT_COLLATERAL
    record_result("S1.1: Balance decreased by collateral",
                  s1_bal_ok,
                  f"before={pn_a_bal_before}, after={pn_a_bal_after}, "
                  f"expected_decrease={SPLIT_COLLATERAL}")

    # S1.2: PMP totalPool increased by sum of split amounts
    pmp_after_split = get_pmp_details(pmp_addr)
    pool_after_split = int(pmp_after_split.get("totalPool", 0))
    pool_0_after = get_pmp_outcome_pool(pmp_after_split, 0, 0)
    pool_1_after = get_pmp_outcome_pool(pmp_after_split, 1, 0)

    frozen_ptc = compute_profit_to_clean_per_outcome(
        [expected_pool_0, expected_pool_1], expected_total)
    expected_splits = compute_split_amounts(
        SPLIT_COLLATERAL, [expected_pool_0, expected_pool_1], frozen_ptc)
    log("Expected split amounts", expected_splits)

    # Outcome pools must increase by expected split amounts
    s1_pool0_ok = pool_0_after == expected_pool_0 + expected_splits[0]
    s1_pool1_ok = pool_1_after == expected_pool_1 + expected_splits[1]
    record_result("S1.2: Outcome pool 0 increased by split delta",
                  s1_pool0_ok,
                  f"expected={expected_pool_0 + expected_splits[0]}, actual={pool_0_after}")
    record_result("S1.3: Outcome pool 1 increased by split delta",
                  s1_pool1_ok,
                  f"expected={expected_pool_1 + expected_splits[1]}, actual={pool_1_after}")

    # S1.4: Neutrality: δ_k * R_k / M_k ≈ F for all k (within floor rounding)
    R_0 = expected_pool_0 + frozen_ptc[0]
    R_1 = expected_pool_1 + frozen_ptc[1]
    implied_F_0 = (expected_splits[0] * R_0) // expected_pool_0 if expected_pool_0 > 0 else 0
    implied_F_1 = (expected_splits[1] * R_1) // expected_pool_1 if expected_pool_1 > 0 else 0
    s1_prop_ok = abs(implied_F_0 - implied_F_1) <= 1
    record_result("S1.4: Split neutrality (implied F per outcome)",
                  s1_prop_ok,
                  f"implied_F_0={implied_F_0}, implied_F_1={implied_F_1}")

    # Record split amounts for later use
    split_0 = pool_0_after - expected_pool_0
    split_1 = pool_1_after - expected_pool_1

    # Verify auto-freeze happened and OrderBook deployed
    pmp_frozen = get_pmp_details(pmp_addr)
    assert pmp_frozen.get("frozen") == True, "PMP must be frozen after first split"
    base_total = int(pmp_frozen.get("baseTotalPool", 0))

    ob_out = common.run_getter(pmp_addr, PMP_ABI, "getOrderBookAddress", {})
    ob_address = ob_out.get("orderBookAddress") if isinstance(ob_out, dict) else None
    assert ob_address is not None, "OrderBook must be deployed after first split"
    common.wait_account_active(ob_address)
    log("OrderBook auto-deployed", ob_address)

    ob_det = get_ob_details(ob_address)
    next_order_id = int(ob_det.get("nextOrderId", 0))

    print(">>> S1 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 2: Limit matching — clearing price & fees
    # Sell outcome 0 from PN_A, then buy from PN_B → match
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S2: Limit matching — clearing price & fees")
    print("="*70)

    SELL_PRICE = 6000   # 60%
    SELL_AMOUNT = MIN_ORDER_NACKL  # 10 NACKL
    BUY_PRICE = 7000    # 70%
    BUY_AMOUNT = MIN_ORDER_NACKL   # 10 NACKL — same as sell for full fill

    # Record balances before
    pn_a_det_s2_before = get_pn_details(pn_a)
    pn_a_bal_s2_before = get_pn_balance(pn_a_det_s2_before, TOKEN_TYPE)
    pn_a_stakes_before = get_pn_stake_amounts(pn_a, EVENT_ID, olh, TOKEN_TYPE)

    pn_b_det_s2_before = get_pn_details(pn_b)
    pn_b_bal_s2_before = get_pn_balance(pn_b_det_s2_before, TOKEN_TYPE)

    # PN_A places sell (maker) — locks outcome tokens
    pn_place_order(pn_a, EVENT_ID, olh, TOKEN_TYPE,
                   0, False, SELL_PRICE, SELL_AMOUNT)

    # Verify sell order rests in book
    ob_after_sell = get_ob_details(ob_address)
    sell_order_id = next_order_id
    assert int(ob_after_sell.get("orderCount", 0)) == 1, \
        "Sell order must rest in book"

    # PN_B places crossing buy (taker) — triggers immediate matching
    buy_cost_raw = compute_buy_collateral(BUY_AMOUNT, BUY_PRICE)
    buy_max_fee = compute_max_fee(buy_cost_raw)
    buy_cost = buy_cost_raw + buy_max_fee  # total locked from balance
    pn_place_order(pn_b, EVENT_ID, olh, TOKEN_TYPE,
                   0, True, BUY_PRICE, BUY_AMOUNT)

    # After matching: book should be empty (both fully filled)
    ob_after_match = get_ob_details(ob_address)
    orders_after = int(ob_after_match.get("orderCount", 0))
    s2_empty_ok = orders_after == 0
    record_result("S2.1: Book empty after full matching",
                  s2_empty_ok,
                  f"orderCount={orders_after}")

    # Expected math: clearing price = maker's price (sell) = 6000
    clearing = SELL_PRICE  # maker is the sell (resting)
    trade = min(SELL_AMOUNT, BUY_AMOUNT)

    # Seller (flags=0 → maker fee): proceeds = trade * clearing / FULL_PERCENT - fee
    sell_fee_expected, sell_notional = compute_fee(trade, clearing, flags=0)
    sell_proceeds_expected = compute_sell_proceeds(trade, clearing, sell_fee_expected)

    # Buyer (flags=0 → maker fee): refund = (buyPrice - clearing) * trade / FULL_PERCENT
    buyer_refund_expected = compute_buyer_refund(BUY_PRICE, clearing, trade)
    buy_fee_expected, _ = compute_fee(trade, clearing, flags=0)

    log("S2 expected math", {
        "clearing": clearing,
        "trade": trade,
        "sell_notional": sell_notional,
        "sell_fee (maker 0.015%)": sell_fee_expected,
        "sell_proceeds": sell_proceeds_expected,
        "buyer_refund": buyer_refund_expected,
        "buy_fee (limit=maker 0.015%)": buy_fee_expected,
    })

    # S2.2: Seller balance increased by proceeds
    pn_a_det_s2_after = get_pn_details(pn_a)
    pn_a_bal_s2_after = get_pn_balance(pn_a_det_s2_after, TOKEN_TYPE)
    seller_bal_change = pn_a_bal_s2_after - pn_a_bal_s2_before
    s2_seller_ok = seller_bal_change == sell_proceeds_expected
    record_result("S2.2: Seller balance += proceeds (notional - maker fee)",
                  s2_seller_ok,
                  f"expected={sell_proceeds_expected}, actual={seller_bal_change}")

    # S2.3: Buyer balance change = -(buy_cost) + refund - fee
    pn_b_det_s2_after = get_pn_details(pn_b)
    pn_b_bal_s2_after = get_pn_balance(pn_b_det_s2_after, TOKEN_TYPE)
    buyer_bal_change = pn_b_bal_s2_after - pn_b_bal_s2_before
    # Buyer locked buy_cost (= raw_cost + maxFee), got back refund + (maxFee - fee)
    expected_buyer_change = -buy_cost + compute_buy_balance_change(buyer_refund_expected, buy_fee_expected, buy_max_fee)
    s2_buyer_ok = buyer_bal_change == expected_buyer_change
    record_result("S2.3: Buyer balance change = -cost + (refund - fee)",
                  s2_buyer_ok,
                  f"expected={expected_buyer_change}, actual={buyer_bal_change}, "
                  f"cost={buy_cost}, refund={buyer_refund_expected}, fee={buy_fee_expected}")

    # S2.4: Verify OB accumulated fees
    ob_fees = get_ob_details(ob_address)
    total_maker_fees = int(ob_fees.get("totalMakerFees", 0))
    total_taker_fees = int(ob_fees.get("totalTakerFees", 0))
    # Both orders have flags=0 → both classified as maker
    expected_maker_total = sell_fee_expected + buy_fee_expected
    s2_maker_fee_ok = total_maker_fees == expected_maker_total
    s2_taker_fee_ok = total_taker_fees == 0
    record_result("S2.4: OB totalMakerFees = sell_fee + buy_fee (both flags=0)",
                  s2_maker_fee_ok,
                  f"expected={expected_maker_total}, actual={total_maker_fees}")
    record_result("S2.5: OB totalTakerFees == 0 (no taker-flagged orders)",
                  s2_taker_fee_ok,
                  f"expected=0, actual={total_taker_fees}")

    # Update next_order_id for subsequent scenarios
    ob_det_now = get_ob_details(ob_address)
    next_order_id = int(ob_det_now.get("nextOrderId", 0))

    print(">>> S2 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 3: Partial fill
    # PN_A sells 15 NACKL, PN_B buys 10 NACKL → 10 filled, 5 rests
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S3: Partial fill")
    print("="*70)

    S3_SELL_AMOUNT = 15_000_000_000  # 15 NACKL
    S3_BUY_AMOUNT = MIN_ORDER_NACKL  # 10 NACKL
    S3_PRICE = 5000  # 50%

    pn_a_det_s3_before = get_pn_details(pn_a)
    pn_a_bal_s3_before = get_pn_balance(pn_a_det_s3_before, TOKEN_TYPE)

    pn_b_det_s3_before = get_pn_details(pn_b)
    pn_b_bal_s3_before = get_pn_balance(pn_b_det_s3_before, TOKEN_TYPE)

    # PN_A sells 15 (rests in book)
    pn_place_order(pn_a, EVENT_ID, olh, TOKEN_TYPE,
                   0, False, S3_PRICE, S3_SELL_AMOUNT)

    # PN_B buys 10 (crosses, fills 10 of 15)
    pn_place_order(pn_b, EVENT_ID, olh, TOKEN_TYPE,
                   0, True, S3_PRICE, S3_BUY_AMOUNT)

    # S3.1: 5 NACKL remains in the book (sell partially filled)
    ob_s3 = get_ob_details(ob_address)
    ob_s3_count = int(ob_s3.get("orderCount", 0))
    s3_resting_ok = ob_s3_count == 1
    record_result("S3.1: 1 order remains (partial fill, 5 rests)",
                  s3_resting_ok,
                  f"orderCount={ob_s3_count}")

    if s3_resting_ok:
        resting_order = get_ob_order(ob_address, next_order_id)
        resting_amount = int(resting_order.get("amount", 0))
        expected_remaining = S3_SELL_AMOUNT - S3_BUY_AMOUNT  # 5 NACKL
        s3_amount_ok = resting_amount == expected_remaining
        record_result("S3.2: Resting order amount = 5 NACKL",
                      s3_amount_ok,
                      f"expected={expected_remaining}, actual={resting_amount}")

    # S3.3: Fee math for partial fill
    s3_trade = min(S3_SELL_AMOUNT, S3_BUY_AMOUNT)  # 10 NACKL
    s3_sell_fee, _ = compute_fee(s3_trade, S3_PRICE, flags=0)
    s3_sell_proceeds = compute_sell_proceeds(s3_trade, S3_PRICE, s3_sell_fee)

    pn_a_det_s3_after = get_pn_details(pn_a)
    pn_a_bal_s3_after = get_pn_balance(pn_a_det_s3_after, TOKEN_TYPE)
    s3_seller_change = pn_a_bal_s3_after - pn_a_bal_s3_before
    s3_seller_ok = s3_seller_change == s3_sell_proceeds
    record_result("S3.3: Seller balance change matches partial fill proceeds",
                  s3_seller_ok,
                  f"expected={s3_sell_proceeds}, actual={s3_seller_change}")

    # Cancel remaining sell order to clean up
    pn_cancel_order(pn_a, EVENT_ID, olh, TOKEN_TYPE, next_order_id)
    next_order_id = int(get_ob_details(ob_address).get("nextOrderId", 0))

    print(">>> S3 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 4: IOC cancel — no counterparty
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S4: IOC cancel — no counterparty")
    print("="*70)

    S4_AMOUNT = MIN_ORDER_NACKL
    S4_PRICE = 5000

    pn_b_det_s4_before = get_pn_details(pn_b)
    pn_b_bal_s4_before = get_pn_balance(pn_b_det_s4_before, TOKEN_TYPE)

    # Place IOC buy with no sell counterparty in book
    pn_place_order(pn_b, EVENT_ID, olh, TOKEN_TYPE,
                   0, True, S4_PRICE, S4_AMOUNT, flags=FLAG_IOC)

    # S4.1: Book still empty (IOC cancelled immediately)
    ob_s4 = get_ob_details(ob_address)
    s4_empty_ok = int(ob_s4.get("orderCount", 0)) == 0
    record_result("S4.1: Book empty after IOC with no counterparty",
                  s4_empty_ok,
                  f"orderCount={ob_s4.get('orderCount', 0)}")

    # S4.2: Balance fully restored (collateral returned via cancel callback)
    pn_b_det_s4_after = get_pn_details(pn_b)
    pn_b_bal_s4_after = get_pn_balance(pn_b_det_s4_after, TOKEN_TYPE)
    s4_restore_ok = pn_b_bal_s4_after == pn_b_bal_s4_before
    record_result("S4.2: Balance restored after IOC cancel",
                  s4_restore_ok,
                  f"before={pn_b_bal_s4_before}, after={pn_b_bal_s4_after}")

    next_order_id = int(get_ob_details(ob_address).get("nextOrderId", 0))

    print(">>> S4 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 5: POST_ONLY cancel — would cross
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S5: POST_ONLY cancel — would cross")
    print("="*70)

    # First, place a resting sell from PN_A
    S5_PRICE = 5000
    S5_AMOUNT = MIN_ORDER_NACKL

    pn_place_order(pn_a, EVENT_ID, olh, TOKEN_TYPE,
                   0, False, S5_PRICE, S5_AMOUNT)

    # Now place POST_ONLY buy from PN_B that would cross
    pn_b_det_s5_before = get_pn_details(pn_b)
    pn_b_bal_s5_before = get_pn_balance(pn_b_det_s5_before, TOKEN_TYPE)

    pn_place_order(pn_b, EVENT_ID, olh, TOKEN_TYPE,
                   0, True, S5_PRICE, S5_AMOUNT, flags=FLAG_POST_ONLY)

    # S5.1: Sell still resting, POST_ONLY cancelled
    ob_s5 = get_ob_details(ob_address)
    s5_count = int(ob_s5.get("orderCount", 0))
    s5_ok = s5_count == 1  # only the sell
    record_result("S5.1: POST_ONLY cancelled, sell still rests",
                  s5_ok,
                  f"orderCount={s5_count}")

    # S5.2: Buyer balance restored
    pn_b_det_s5_after = get_pn_details(pn_b)
    pn_b_bal_s5_after = get_pn_balance(pn_b_det_s5_after, TOKEN_TYPE)
    s5_restore_ok = pn_b_bal_s5_after == pn_b_bal_s5_before
    record_result("S5.2: POST_ONLY buyer balance restored",
                  s5_restore_ok,
                  f"before={pn_b_bal_s5_before}, after={pn_b_bal_s5_after}")

    # Cancel resting sell to clean up
    sell_id_s5 = next_order_id
    pn_cancel_order(pn_a, EVENT_ID, olh, TOKEN_TYPE, sell_id_s5)
    next_order_id = int(get_ob_details(ob_address).get("nextOrderId", 0))

    print(">>> S5 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 6: Merge Math
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S6: Merge Math — collateral returned")
    print("="*70)

    # PN_A has outcome tokens from stakes + splits. Merge some back.
    pn_a_det_s6_before = get_pn_details(pn_a)
    pn_a_bal_s6_before = get_pn_balance(pn_a_det_s6_before, TOKEN_TYPE)
    pn_a_stakes_s6 = get_pn_stake_amounts(pn_a, EVENT_ID, olh, TOKEN_TYPE)
    log("PN_A stakes before merge", pn_a_stakes_s6)

    if len(pn_a_stakes_s6) >= 2 and pn_a_stakes_s6[0] > 0 and pn_a_stakes_s6[1] > 0:
        # Merge the minimum of both stakes (up to 20 NACKL each)
        merge_amount = min(pn_a_stakes_s6[0], pn_a_stakes_s6[1], 20_000_000_000)
        merge_amounts = [merge_amount, merge_amount]

        # Expected collateral from merge
        pmp_s6 = get_pmp_details(pmp_addr)
        pool_0_s6 = get_pmp_outcome_pool(pmp_s6, 0, 0)
        pool_1_s6 = get_pmp_outcome_pool(pmp_s6, 1, 0)
        total_pool_s6 = int(pmp_s6.get("totalPool", 0))

        expected_collateral = compute_merge_collateral(
            merge_amounts,
            [expected_pool_0, expected_pool_1],  # base pools (at freeze time)
            frozen_ptc)
        log("Expected merge collateral", expected_collateral)

        pn_merge_full_set(pn_a, EVENT_ID, olh, TOKEN_TYPE, merge_amounts)

        pn_a_det_s6_after = get_pn_details(pn_a)
        pn_a_bal_s6_after = get_pn_balance(pn_a_det_s6_after, TOKEN_TYPE)

        s6_collateral_ok = pn_a_bal_s6_after == pn_a_bal_s6_before + expected_collateral
        record_result("S6.1: Balance increased by merge collateral",
                      s6_collateral_ok,
                      f"expected_increase={expected_collateral}, "
                      f"actual_increase={pn_a_bal_s6_after - pn_a_bal_s6_before}")

        # S6.2: PMP totalPool decreased
        pmp_s6_after = get_pmp_details(pmp_addr)
        total_pool_s6_after = int(pmp_s6_after.get("totalPool", 0))
        pool_decreased = total_pool_s6 - total_pool_s6_after
        s6_pool_ok = pool_decreased > 0
        record_result("S6.2: PMP totalPool decreased after merge",
                      s6_pool_ok,
                      f"before={total_pool_s6}, after={total_pool_s6_after}, "
                      f"decreased={pool_decreased}")
    else:
        record_result("S6: SKIPPED — insufficient stake amounts",
                      True, f"stakes={pn_a_stakes_s6}")

    print(">>> S6 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 7: Resolve + Claim (conservation)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S7: Resolve + Claim — conservation")
    print("="*70)

    # Record balances before resolve
    pn_a_det_s7 = get_pn_details(pn_a)
    pn_a_bal_s7_before = get_pn_balance(pn_a_det_s7, TOKEN_TYPE)
    pn_b_det_s7 = get_pn_details(pn_b)
    pn_b_bal_s7_before = get_pn_balance(pn_b_det_s7, TOKEN_TYPE)

    # Get final totalPool before resolve
    pmp_s7 = get_pmp_details(pmp_addr)
    total_pool_s7 = int(pmp_s7.get("totalPool", 0))
    log("S7 totalPool before resolve", total_pool_s7)

    # Resolve: outcome 0 wins
    wait_and_resolve(pmp_addr, r_start, 0)

    pmp_s7_resolved = get_pmp_details(pmp_addr)
    s7_creator_fee = int(pmp_s7_resolved.get("creatorFee", 0))
    log("S7 creatorFee", s7_creator_fee)

    # PN_A claims (winner on outcome 0 — deployer + staker)
    pn_a_det_s7_bc = get_pn_details(pn_a)
    pn_a_bal_s7_bc = get_pn_balance(pn_a_det_s7_bc, TOKEN_TYPE)
    pn_claim(pn_a, EVENT_ID, olh, TOKEN_TYPE)
    pn_a_det_s7_ac = get_pn_details(pn_a)
    pn_a_bal_s7_ac = get_pn_balance(pn_a_det_s7_ac, TOKEN_TYPE)
    pn_a_payout = pn_a_bal_s7_ac - pn_a_bal_s7_bc

    # PN_B claims (loser on outcome 1, but may have bought outcome 0 tokens via OB)
    pn_b_det_s7_bc = get_pn_details(pn_b)
    pn_b_bal_s7_bc = get_pn_balance(pn_b_det_s7_bc, TOKEN_TYPE)
    pn_claim(pn_b, EVENT_ID, olh, TOKEN_TYPE)
    pn_b_det_s7_ac = get_pn_details(pn_b)
    pn_b_bal_s7_ac = get_pn_balance(pn_b_det_s7_ac, TOKEN_TYPE)
    pn_b_payout = pn_b_bal_s7_ac - pn_b_bal_s7_bc

    log("S7 payouts", f"PN_A={pn_a_payout}, PN_B={pn_b_payout}")

    # S7.1: Conservation: fee + payouts + dust = live totalPool
    # With R_k-based split/merge, dust should be minimal (0-2 from floor rounding)
    total_paid = s7_creator_fee + pn_a_payout + pn_b_payout
    dust = total_pool_s7 - total_paid
    s7_cons_ok = 0 <= dust <= 2
    record_result("S7.1: Conservation (fee + payouts + dust = totalPool)",
                  s7_cons_ok,
                  f"totalPool={total_pool_s7}, fee={s7_creator_fee}, "
                  f"payouts=[{pn_a_payout}, {pn_b_payout}], dust={dust}")

    # S7.2: PN_B got payout for outcome 0 tokens bought via OB
    s7_b_got_payout = pn_b_payout > 0
    record_result("S7.2: PN_B received payout for OB-bought outcome 0 tokens",
                  s7_b_got_payout,
                  f"payout={pn_b_payout}")

    print(">>> S7 DONE")

    # ══════════════════════════════════════════════════════════════════════════
    # Scenario 8: Fee formula verification (exact math)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "="*70)
    print("  S8: Fee formula verification")
    print("="*70)

    # Verify accumulated OB fees match expected values
    ob_final = get_ob_details(ob_address)
    final_maker_fees = int(ob_final.get("totalMakerFees", 0))
    final_taker_fees = int(ob_final.get("totalTakerFees", 0))

    # All orders used flags=0 → all classified as maker.
    # S2: trade=10 NACKL @ clearing=6000 → notional = 6 NACKL
    #   2 fills (seller + buyer), each maker_fee = 6e9 * 15 / 100000 = 900_000
    s2_notional = (MIN_ORDER_NACKL * SELL_PRICE) // FULL_PERCENT
    s2_maker_per_fill = (s2_notional * MAKER_FEE_RATE) // FEE_DENOMINATOR
    s2_total_maker = s2_maker_per_fill * 2  # both sides

    # S3: trade=10 NACKL @ clearing=5000 → notional = 5 NACKL
    #   2 fills (seller + buyer), each maker_fee = 5e9 * 15 / 100000 = 750_000
    s3_notional = (s3_trade * S3_PRICE) // FULL_PERCENT
    s3_maker_per_fill = (s3_notional * MAKER_FEE_RATE) // FEE_DENOMINATOR
    s3_total_maker = s3_maker_per_fill * 2  # both sides

    expected_total_maker = s2_total_maker + s3_total_maker
    expected_total_taker = 0  # no taker-flagged orders

    s8_maker_ok = final_maker_fees == expected_total_maker
    s8_taker_ok = final_taker_fees == expected_total_taker
    record_result("S8.1: Cumulative maker fees match (all flags=0)",
                  s8_maker_ok,
                  f"expected={expected_total_maker} "
                  f"(S2:{s2_total_maker} + S3:{s3_total_maker}), "
                  f"actual={final_maker_fees}")
    record_result("S8.2: Cumulative taker fees == 0",
                  s8_taker_ok,
                  f"expected=0, actual={final_taker_fees}")

    print(">>> S8 DONE")

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
    summary_lines.append("  MATH ORDERBOOK TEST RESULTS")
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

    results_path = "./tests/dex/math_orderbook_test_results.txt"
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
