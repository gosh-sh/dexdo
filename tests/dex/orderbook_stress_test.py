"""
OrderBook stress test.

Architecture:
  - N PrivateNotes deployed (half sell, half buy)
  - K worker threads, each does M iterations
  - Worker k on iteration j sends placeOrder to PN index (k + j*K) % N
  - Worker does NOT wait for PN free — just fires TX and moves on
  - TPS measured from first TX send to last TX send (wall clock)

Args:  N  K  M
  N — number of PrivateNotes (half sell / half buy)
  K — number of parallel worker threads
  M — iterations per worker (each iteration = 1 order to 1 PN)

Examples:
  NETWORK=localhost python3 tests/dex/orderbook_stress_test.py 10 3 3     # smoke
  NETWORK=localhost python3 tests/dex/orderbook_stress_test.py 100 10 30  # main
  NETWORK=localhost python3 tests/dex/orderbook_stress_test.py 1000 50 200 # full
"""

import json
import os
import random
import sys
import threading
import time
import concurrent.futures

sys.path.append("tests")
from helper import common

# ── Args ─────────────────────────────────────────────────────────────────────
if len(sys.argv) < 4:
    print(f"Usage: {sys.argv[0]} N K M")
    print(f"  N — PrivateNotes count (half sell / half buy)")
    print(f"  K — worker threads")
    print(f"  M — iterations per worker")
    sys.exit(1)

N = int(sys.argv[1])
K = int(sys.argv[2])
M = int(sys.argv[3])

assert N >= 2, "N must be >= 2"

# ── Infrastructure addresses ─────────────────────────────────────────────────
GIVER_ADDRESS       = "0:1111111111111111111111111111111111111111111111111111111111111111"
ROOT_PN_ADDRESS     = "0:1010101010101010101010101010101010101010101010101010101010101010"
ROOT_ORACLE_ADDRESS = "0:1515151515151515151515151515151515151515151515151515151515151515"

# ── ABI paths ────────────────────────────────────────────────────────────────
GIVER_ABI        = "./contracts/giver/GiverV3.abi.json"
GIVER_KEY_PATH   = "./tests/GiverV3.keys.json"
ROOT_PN_ABI      = "./contracts/0.79.3_compiled/dex/RootPN.abi.json"
ROOT_ORACLE_ABI  = "./contracts/0.79.3_compiled/dex/RootOracle.abi.json"
PRIVATE_NOTE_ABI = "./contracts/0.79.3_compiled/dex/PrivateNote.abi.json"
PMP_ABI          = "./contracts/0.79.3_compiled/dex/PMP.abi.json"
ORACLE_ABI       = "./contracts/0.79.3_compiled/dex/Oracle.abi.json"
EVENTLIST_ABI    = "./contracts/0.79.3_compiled/dex/OracleEventList.abi.json"
ORDERBOOK_ABI    = "./contracts/0.79.3_compiled/dex/OrderBook.abi.json"

EPHEMERAL_KEY_PATH = "./tests/dex/ephemeral.keys.json"
ORACLE_KEY_PATH    = "./tests/dex/oracle.keys.json"

# ── Oracle / Event ───────────────────────────────────────────────────────────
ORACLE_NAME     = f"StressOracle_{int(time.time())}"
EVENT_NAME      = "Stress match"
EVENT_DESCRIBE  = "Stress test event"
EVENT_OUTCOMES  = {1: "Team A", 2: "Team B"}
EVENT_DEADLINE  = 2000000000
ORACLE_FEE      = 100
EVENT_ID        = "0x0"

TOKEN_TYPE     = 1
TOKEN_TYPE_ECC = 300
SHELL_DEPOSIT  = 200_000_000_000

# ── Amounts ──────────────────────────────────────────────────────────────────
DEPOSIT_PER_NOTE = 2_000_000_000_000
SPLIT_COLLATERAL = 1_500_000_000_000
ORDER_AMOUNT     = 10_000_000_000
DEPLOYER_SEED    = 100_000_000_000

SELL_PRICE_BPS = 4000
BUY_PRICE_BPS  = 6000

# ── PMP timing ───────────────────────────────────────────────────────────────
STAKE_PERIOD   = max(600, 10 + N * 5)  # scale with N
EPOCH_DURATION = 7200

# ── Parallelism ──────────────────────────────────────────────────────────────
DEPLOY_PARALLELISM = 20

# ── Thread-safe print ────────────────────────────────────────────────────────
_lock = threading.Lock()

def log(title, data=""):
    with _lock:
        print(f"\n=== {title} ===")
        if data != "":
            print(data)
        print("===")

def note(msg):
    with _lock:
        print(msg)


# ── Stats ────────────────────────────────────────────────────────────────────
class Stats:
    def __init__(self):
        self._lock  = threading.Lock()
        self.placed = 0
        self.failed = 0

    def inc_placed(self):
        with self._lock:
            self.placed += 1

    def inc_failed(self):
        with self._lock:
            self.failed += 1

    def snapshot(self):
        with self._lock:
            return self.placed, self.failed

STATS = Stats()


# ── ZK proof ─────────────────────────────────────────────────────────────────
def generate_random_sk():
    return os.urandom(31).hex() + format(random.randint(0, 0x2F), "02x")

def generate_proof(sk, token_type, value):
    out = common.execute_cmd(f"./halo2-proover {sk} {token_type} {value}")
    raw = [line for line in out.splitlines() if line.strip()][-1]
    pair = json.loads(json.loads(raw))
    dih       = "0x" + pair["private_note_digest"]
    nullifier = "0x" + pair["private_note_digest"]
    return pair["proof"], dih, int(pair["private_note_sum"]), int(pair["token_type"]), nullifier


# ── Helpers ──────────────────────────────────────────────────────────────────
def wait_active(address, timeout=1500):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            acc = common.execute_cli_cmd(f"account {address}")
            if isinstance(acc, dict) and acc.get("acc_type") == "Active":
                return True
        except Exception:
            pass
        time.sleep(2)
    return False

def run_parallel(tasks, label="", max_workers=DEPLOY_PARALLELISM):
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as ex:
        futs = {ex.submit(fn, *args): i for i, (fn, args) in enumerate(tasks)}
        for fut in concurrent.futures.as_completed(futs):
            try:
                results.append(fut.result())
            except Exception as exc:
                note(f"  [parallel {label}] exception: {exc}")
    return results

def ob_details(ob_address):
    try:
        return common.run_getter(ob_address, ORDERBOOK_ABI, "getDetails", {})
    except Exception:
        return None



# ── Infra helpers ────────────────────────────────────────────────────────────
def generate_oracle_pubkey():
    common.gen_keys(ORACLE_KEY_PATH)
    pub = common.read_public_key(ORACLE_KEY_PATH)
    return pub if pub.startswith("0x") else "0x" + pub

def generate_ephemeral_pubkey():
    common.gen_keys(EPHEMERAL_KEY_PATH)
    pub = common.read_public_key(EPHEMERAL_KEY_PATH)
    return pub if pub.startswith("0x") else "0x" + pub

def deploy_oracle(oracle_pubkey):
    common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                         EPHEMERAL_KEY_PATH, "deployOracle",
                         {"oraclePubkey": oracle_pubkey, "oracleName": ORACLE_NAME})
    time.sleep(3)

def get_oracle_address():
    out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                            "getOracleAddress", {"name": ORACLE_NAME})
    addr = out.get("oracleAddress") if isinstance(out, dict) else out
    common.wait_account_active(addr)
    return addr

def get_eventlist_address(oracle_address):
    out = common.run_getter(oracle_address, ORACLE_ABI,
                            "getEventListAddress", {"index": 0})
    addr = out.get("value0") if isinstance(out, dict) else out
    common.wait_account_active(addr)
    return addr

def add_event(eventlist_address):
    common.call_contract(eventlist_address, EVENTLIST_ABI, ORACLE_KEY_PATH,
                         "addEvent", {
                             "event_name":   EVENT_NAME,
                             "oracle_fee":   ORACLE_FEE,
                             "deadline":     EVENT_DEADLINE,
                             "describe":     EVENT_DESCRIBE,
                             "outcomeNames": EVENT_OUTCOMES,
                             "trustAddr":    None,
                         })
    time.sleep(2)
    out = common.run_getter(eventlist_address, EVENTLIST_ABI, "_events", {})
    events_map = out.get("_events", {}) if isinstance(out, dict) else {}
    assert len(events_map) > 0, "No events found after addEvent"
    event_id = list(events_map.keys())[0]
    if not str(event_id).startswith("0x"):
        event_id = "0x" + hex(int(event_id))[2:]
    return event_id

def fund_root_pn(amount):
    common.call_contract(GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
                         "sendCurrencyWithFlag", {
                             "dest":  ROOT_PN_ADDRESS,
                             "value": 2_000_000_000,
                             "ecc":   {"1": amount},
                             "flag":  1,
                         })
    time.sleep(3)

def fund_deployer_with_shell(deployer_sk, deployer_dih):
    common.call_contract(GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
                         "sendCurrencyWithFlag", {
                             "dest":  ROOT_PN_ADDRESS,
                             "value": 2_000_000_000,
                             "ecc":   {"2": SHELL_DEPOSIT},
                             "flag":  1,
                         })
    time.sleep(3)
    proof_sh, _, _, _, nullifier_sh = generate_proof(deployer_sk, TOKEN_TYPE_ECC, SHELL_DEPOSIT)
    common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI, EPHEMERAL_KEY_PATH,
                         "sendEccShellToPrivateNote", {
                             "proof":                   proof_sh,
                             "nullifier_hash":          nullifier_sh,
                             "deposit_identifier_hash": deployer_dih,
                             "value":                   SHELL_DEPOSIT,
                         })
    time.sleep(6)

def deploy_deployer_note(sk, ephemeral_pubkey):
    proof, dih, value, token_type, _ = generate_proof(sk, TOKEN_TYPE, DEPOSIT_PER_NOTE)
    common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI, EPHEMERAL_KEY_PATH,
                         "deployPrivateNote", {
                             "zkproof":                 proof,
                             "deposit_identifier_hash": dih,
                             "ephemeral_pubkey":        ephemeral_pubkey,
                             "value":                   value,
                             "token_type":              token_type,
                         })
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                            "getPrivateNoteAddress",
                            {"deposit_identifier_hash": dih})
    addr = out["privateNoteAddress"]
    assert wait_active(addr, timeout=120), f"Deployer PN not Active: {addr}"
    return addr, dih

def deploy_pmp(pn_address):
    common.call_contract(pn_address, PRIVATE_NOTE_ABI, EPHEMERAL_KEY_PATH,
                         "deployPMP", {
                             "event_id":      EVENT_ID,
                             "oracleFee":     [ORACLE_FEE],
                             "token_type":    TOKEN_TYPE,
                             "names":         [ORACLE_NAME],
                             "index":         [0],
                             "initialStakes": [DEPLOYER_SEED, DEPLOYER_SEED],
                         })
    time.sleep(5)
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI, "getPMPAddress", {
        "event_id":   EVENT_ID,
        "names":      [ORACLE_NAME],
        "token_type": TOKEN_TYPE,
    })
    pmp_address = out["pmpAddress"]
    assert wait_active(pmp_address, timeout=120), f"PMP not Active: {pmp_address}"
    return pmp_address

def wait_pmp_approved(pmp_address, max_wait=120):
    deadline = time.time() + max_wait
    while time.time() < deadline:
        d = common.run_getter(pmp_address, PMP_ABI, "getDetails", {})
        if d and int(d.get("numberOfOracleEvents", 0)) > 0 and \
           int(d.get("approvedOracleEvents", 0)) >= int(d.get("numberOfOracleEvents", 0)):
            time.sleep(3)
            return d
        time.sleep(3)
    raise TimeoutError(f"PMP not approved after {max_wait}s")

def set_timings(pmp_address, r_start):
    common.call_contract(pmp_address, PMP_ABI, ORACLE_KEY_PATH, "submitSetTimings", {
        "resultStart": r_start,
    })
    time.sleep(3)


def get_orderbook_address(pmp_address):
    out = common.run_getter(pmp_address, PMP_ABI, "getOrderBookAddress", {})
    return out.get("orderBookAddress") if isinstance(out, dict) else out

def stake_one(idx, pn_addr, oracle_list_hash, total):
    common.call_contract(pn_addr, PRIVATE_NOTE_ABI, EPHEMERAL_KEY_PATH,
                         "setStake", {
                             "event_id":        EVENT_ID,
                             "oracle_list_hash": oracle_list_hash,
                             "token_type":       TOKEN_TYPE,
                             "outcome":          0,
                             "amount":           SPLIT_COLLATERAL,
                             "use_coupon":       False,
                         })
    note(f"  [{idx+1}/{total}] stake done")

def split_one(idx, pn_addr, oracle_list_hash, total):
    common.call_contract(pn_addr, PRIVATE_NOTE_ABI, EPHEMERAL_KEY_PATH,
                         "splitFullSet", {
                             "event_id":        EVENT_ID,
                             "oracle_list_hash": oracle_list_hash,
                             "token_type":       TOKEN_TYPE,
                             "collateral":       SPLIT_COLLATERAL,
                         })
    note(f"  [{idx+1}/{total}] split done")


# ── Order worker ─────────────────────────────────────────────────────────────
def order_worker(worker_id, pn_addresses, oracle_list_hash, sell_count):
    """
    Worker k sends M orders:
      iteration j -> PN index = (worker_id + j * K) % N
      if index < sell_count -> sell order, else -> buy order
    Fire TX (call_contract is sync = waits for TX inclusion), then move on.
    Waits for PN to be free before sending (PN rejects if busy).
    """
    ok = 0
    fail = 0
    n = len(pn_addresses)
    for j in range(M):
        pn_idx = (worker_id + j * K) % n
        pn_addr = pn_addresses[pn_idx]
        is_sell = pn_idx < sell_count
        params = {
            "event_id":         EVENT_ID,
            "oracle_list_hash": oracle_list_hash,
            "token_type":       TOKEN_TYPE,
            "outcomeId":        0,
            "isBuy":            not is_sell,
            "priceBps":         SELL_PRICE_BPS if is_sell else BUY_PRICE_BPS,
            "amount":           ORDER_AMOUNT,
            "flags":            0,
            "minAmount":        0,
            "epochId":          0,
        }
        # Wait for PN to be free (busy = previous order callback not yet received)
        for _ in range(120):
            try:
                d = common.run_getter(pn_addr, PRIVATE_NOTE_ABI, "getDetails", {})
                if not (d and d.get("busyAddress")):
                    break
            except Exception:
                pass
            time.sleep(1)
        try:
            common.WAS_ERROR = False
            common.call_contract(pn_addr, PRIVATE_NOTE_ABI,
                                 EPHEMERAL_KEY_PATH, "placeOrder", params)
            if common.WAS_ERROR:
                STATS.inc_failed()
                fail += 1
            else:
                STATS.inc_placed()
                ok += 1
        except Exception:
            STATS.inc_failed()
            fail += 1
    note(f"  [worker {worker_id}] done: {ok} ok, {fail} fail")
    return ok, fail


# ── Main ─────────────────────────────────────────────────────────────────────
def main():
    total_orders = K * M
    print(f"\n{'='*65}")
    print(f"  OrderBook Stress Test: N={N} K={K} M={M} -> {total_orders} orders")
    print(f"{'='*65}")

    common.set_config({"async_call": "false"})
    common.setup()
    time.sleep(1)

    common.wait_account_active(GIVER_ADDRESS)
    common.wait_account_active(ROOT_PN_ADDRESS)
    common.wait_account_active(ROOT_ORACLE_ADDRESS)

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 1 — Oracle + Event + Deployer PN + PMP
    # ══════════════════════════════════════════════════════════════════════════
    print("\n>>> Phase 1: Oracle + PMP setup")

    oracle_pubkey    = generate_oracle_pubkey()
    ephemeral_pubkey = generate_ephemeral_pubkey()

    deploy_oracle(oracle_pubkey)
    oracle_address    = get_oracle_address()
    eventlist_address = get_eventlist_address(oracle_address)
    global EVENT_ID
    EVENT_ID = add_event(eventlist_address)
    log("event_id", EVENT_ID)

    # Fund RootPN for N+1 notes
    total_nackl = (N + 1) * DEPOSIT_PER_NOTE
    note(f"  Funding RootPN with {total_nackl // 10**9} NACKL for {N+1} notes")
    fund_root_pn(total_nackl)

    deployer_sk = generate_random_sk()
    deployer_pn, deployer_dih = deploy_deployer_note(deployer_sk, ephemeral_pubkey)
    log("Deployer PN", deployer_pn)
    fund_deployer_with_shell(deployer_sk, deployer_dih)

    pmp_address = deploy_pmp(deployer_pn)
    pmp_approved = wait_pmp_approved(pmp_address)
    oracle_list_hash = pmp_approved["oracle_list_hash"]
    log("PMP", pmp_address)

    now     = int(time.time())
    s_start = now
    r_start = now + STAKE_PERIOD
    set_timings(pmp_address, r_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s_end = s_start + (r_start - s_start) // 10
    print(">>> Phase 1 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 2 — Pre-generate ZK proofs (parallel) + Deploy N PNs (sequential)
    # ══════════════════════════════════════════════════════════════════════════
    print(f"\n>>> Phase 2: Generate {N} proofs (parallel) + deploy {N} PNs (sequential)")

    sks = [generate_random_sk() for _ in range(N)]
    proofs = [None] * N
    def _gen(i):
        proofs[i] = generate_proof(sks[i], TOKEN_TYPE, DEPOSIT_PER_NOTE)
        return i
    gen_tasks = [(_gen, (i,)) for i in range(N)]
    t0 = time.time()
    run_parallel(gen_tasks, label="gen proofs", max_workers=DEPLOY_PARALLELISM)
    proof_ok = sum(1 for p in proofs if p is not None)
    note(f"  {proof_ok}/{N} proofs generated in {time.time()-t0:.1f}s")
    assert proof_ok == N, f"Only {proof_ok}/{N} proofs generated"

    # Deploy one by one, wait for Active each time (no timeout — just wait)
    pn_addresses = []
    t0 = time.time()
    for i in range(N):
        proof, dih, value, token_type, _ = proofs[i]
        common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI, EPHEMERAL_KEY_PATH,
                             "deployPrivateNote", {
                                 "zkproof":                 proof,
                                 "deposit_identifier_hash": dih,
                                 "ephemeral_pubkey":        ephemeral_pubkey,
                                 "value":                   value,
                                 "token_type":              token_type,
                             })
        out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                                "getPrivateNoteAddress",
                                {"deposit_identifier_hash": dih})
        addr = out["privateNoteAddress"]
        # Wait for Active — no timeout, just poll
        while True:
            try:
                acc = common.execute_cli_cmd(f"account {addr}")
                if isinstance(acc, dict) and acc.get("acc_type") == "Active":
                    break
            except Exception:
                pass
            time.sleep(2)
        pn_addresses.append(addr)
        note(f"  [{i+1}/{N}] PN deployed + Active: {addr[-12:]}")
    note(f"  All {N} PNs deployed in {time.time()-t0:.1f}s")

    print(">>> Phase 2 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 3 — Stake sell PNs + wait stakeEnd + freeze + split
    # ══════════════════════════════════════════════════════════════════════════
    sell_count = N // 2
    buy_count  = N - sell_count
    sell_pns   = pn_addresses[:sell_count]

    print(f"\n>>> Phase 3: Stake {sell_count} sell PNs + freeze + split")

    stake_tasks = [(stake_one, (i, sell_pns[i], oracle_list_hash, sell_count))
                   for i in range(sell_count)]
    run_parallel(stake_tasks, label="stake")
    note("  Waiting 30s for stake callbacks...")
    time.sleep(30)

    wait_sec = s_end - int(time.time()) + 3
    if wait_sec > 0:
        note(f"  Waiting {wait_sec}s for stakeEnd...")
        time.sleep(wait_sec)

    # Freeze happens automatically on first split
    split_tasks = [(split_one, (i, sell_pns[i], oracle_list_hash, sell_count))
                   for i in range(sell_count)]
    run_parallel(split_tasks, label="split")
    note("  Waiting 15s for split callbacks...")
    time.sleep(15)

    ob_address = get_orderbook_address(pmp_address)
    assert ob_address, "OrderBook address is empty after split"
    assert wait_active(ob_address, timeout=120), f"OrderBook not Active: {ob_address}"
    log("OrderBook (auto-deployed)", ob_address)

    print(">>> Phase 3 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 4 — BLAST: K workers x M iterations
    # ══════════════════════════════════════════════════════════════════════════
    print(f"\n>>> Phase 4: BLAST — {K} workers x {M} iterations = {total_orders} orders")
    print(f"  {sell_count} sell PNs @ {SELL_PRICE_BPS}bps, {buy_count} buy PNs @ {BUY_PRICE_BPS}bps")

    ob_before = ob_details(ob_address)
    next_id_before = int(ob_before.get("nextOrderId", 0)) if ob_before else 0
    ob_ts_before   = int(ob_before.get("state_timestamp", 0)) if ob_before else 0

    # Progress ticker
    stop_ticker = threading.Event()
    def ticker():
        while not stop_ticker.is_set():
            stop_ticker.wait(30)
            if stop_ticker.is_set():
                break
            p, f = STATS.snapshot()
            d = ob_details(ob_address)
            ob_next = int(d.get("nextOrderId", 0)) if d else 0
            note(f"  [progress] placed={p} failed={f} ob_nextId={ob_next}")
    tick_t = threading.Thread(target=ticker, daemon=True)
    tick_t.start()

    t_start = time.time()

    with concurrent.futures.ThreadPoolExecutor(max_workers=K) as ex:
        futs = [ex.submit(order_worker, k, pn_addresses, oracle_list_hash, sell_count)
                for k in range(K)]
        concurrent.futures.wait(futs)

    t_end = time.time()
    stop_ticker.set()
    elapsed = t_end - t_start

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 5 — Results
    # ══════════════════════════════════════════════════════════════════════════
    print(f"\n>>> Phase 5: Results (blast took {elapsed:.1f}s)")
    time.sleep(10)  # let OB settle

    placed, failed = STATS.snapshot()
    d = ob_details(ob_address)
    next_order_id = int(d.get("nextOrderId", 0)) if d else 0
    resting       = int(d.get("orderCount", 0)) if d else -1
    maker_fees    = int(d.get("totalMakerFees", 0)) if d else 0
    taker_fees    = int(d.get("totalTakerFees", 0)) if d else 0
    ob_ts_after   = int(d.get("state_timestamp", 0)) if d else 0
    orders_in_ob  = next_order_id - next_id_before
    matched_est   = orders_in_ob - resting if resting >= 0 else 0

    tps_wall = placed / elapsed if elapsed > 0 else 0.0
    if ob_ts_after > ob_ts_before:
        chain_s = (ob_ts_after - ob_ts_before) / 1000.0
    else:
        chain_s = elapsed
    tps_chain = orders_in_ob / chain_s if chain_s > 0 else 0.0

    print("\n")
    print("=" * 65)
    print(f"  STRESS TEST RESULTS  (N={N} K={K} M={M})")
    print("=" * 65)
    print(f"  Target orders:       {total_orders}")
    print(f"  Orders placed (TX):  {placed:>8}")
    print(f"  Orders failed (TX):  {failed:>8}")
    print(f"  OB orders received:  {orders_in_ob:>8}")
    print(f"  OB resting at end:   {resting:>8}")
    print(f"  Matched (est.):      {matched_est:>8}")
    print(f"  Maker fees:          {maker_fees:>12}  ({maker_fees / 10**9:.4f} NACKL)")
    print(f"  Taker fees:          {taker_fees:>12}  ({taker_fees / 10**9:.4f} NACKL)")
    if placed + failed > 0:
        print(f"  Fail rate:           {100*failed/(placed+failed):.1f}%")
    print(f"  ---")
    print(f"  Wall clock:          {elapsed:.1f}s  ({tps_wall:.2f} tps)")
    print(f"  On-chain time:       {chain_s:.1f}s  ({tps_chain:.2f} tps)")
    print("=" * 65)

    errors = []
    if placed == 0:
        errors.append("FAIL: zero orders placed")
    if orders_in_ob == 0:
        errors.append("FAIL: OB received zero orders")

    if errors:
        for e in errors:
            print(f"\n  {e}")
        print("\n>>> STRESS TEST FAILED")
        sys.exit(1)

    print("\n>>> STRESS TEST PASSED")


if __name__ == "__main__":
    main()
