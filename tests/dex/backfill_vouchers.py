"""
Backfill missing event BOCs in vouchers.txt by querying all events from DB
and matching by sk_u_commit.
"""

import json
import subprocess
import sys

sys.path.append("tests")
from helper import common

ROOT_PN_ADDRESS = "0:1010101010101010101010101010101010101010101010101010101010101010"
ROOT_PN_ABI = "./contracts/0.79.3_compiled/dex/RootPN.abi.json"
VOUCHER_EVENT_DST = ":0000000000000000000000000000000000000000000000000000000000000087"
VOUCHERS_FILE = "./tests/dex/vouchers.txt"

def fetch_all_events():
    """Fetch all voucherGenerated events from DB."""
    cmd = (
        f'{common.TVM_CLI} -j query-raw messages "id boc body src dst created_at" '
        f'--filter \'{{"src":{{"eq":"{ROOT_PN_ADDRESS}"}},"msg_type":{{"eq":2}},"dst":{{"eq":"{VOUCHER_EVENT_DST}"}}}}\' '
        f'--limit 200 '
        f'--order \'[{{"path":"created_at","direction":"ASC"}}]\''
    )
    output = subprocess.check_output(cmd, shell=True, stderr=subprocess.STDOUT).decode("utf-8").strip()
    return json.loads(output)


def decode_event_body(body_b64):
    """Decode event body to extract sk_u_commit."""
    cmd = f'{common.TVM_CLI} -j decode body --abi {ROOT_PN_ABI} "{body_b64}"'
    output = subprocess.check_output(cmd, shell=True, stderr=subprocess.STDOUT).decode("utf-8").strip()
    result = json.loads(output)
    call = result.get("BodyCall", {})
    vg = call.get("voucherGenerated", {})
    sk_u_commit_raw = vg.get("sk_u_commit", "")
    # Remove 0x prefix and leading zeros -> normalize to 64-char hex
    if sk_u_commit_raw.startswith("0x"):
        sk_u_commit_raw = sk_u_commit_raw[2:]
    return sk_u_commit_raw.zfill(64)


def main():
    common.setup()

    # Step 1: Read vouchers.txt
    with open(VOUCHERS_FILE, "r") as f:
        content = f.read()

    # Parse into voucher entries (4-line groups: sk_u, sk_u_commit, boc, blank)
    lines = content.split("\n")
    vouchers = []
    i = 0
    while i < len(lines):
        if i + 2 < len(lines) and lines[i].strip():
            sk_u = lines[i].strip()
            sk_u_commit = lines[i+1].strip()
            boc = lines[i+2].strip()
            vouchers.append({"sk_u": sk_u, "sk_u_commit": sk_u_commit, "boc": boc})
            i += 4  # skip blank line
        else:
            i += 1

    no_event_count = sum(1 for v in vouchers if v["boc"] == "NO_EVENT")
    print(f"Total vouchers: {len(vouchers)}")
    print(f"NO_EVENT entries: {no_event_count}")

    if no_event_count == 0:
        print("Nothing to backfill!")
        return

    # Step 2: Fetch all events and build sk_u_commit -> boc map
    print("\nFetching all events from DB...")
    events = fetch_all_events()
    print(f"Found {len(events)} events")

    print("Decoding event bodies...")
    commit_to_boc = {}
    for idx, event in enumerate(events):
        body = event.get("body", "")
        boc = event.get("boc", "")
        if not body or not boc:
            continue
        try:
            sk_u_commit = decode_event_body(body)
            commit_to_boc[sk_u_commit] = boc
            if (idx + 1) % 10 == 0:
                print(f"  Decoded {idx+1}/{len(events)}")
        except Exception as e:
            print(f"  Error decoding event {idx}: {e}")

    print(f"Built map with {len(commit_to_boc)} unique sk_u_commit -> boc entries")

    # Step 3: Backfill NO_EVENT entries
    fixed = 0
    still_missing = 0
    for v in vouchers:
        if v["boc"] == "NO_EVENT":
            matched_boc = commit_to_boc.get(v["sk_u_commit"])
            if matched_boc:
                v["boc"] = matched_boc
                fixed += 1
            else:
                still_missing += 1
                print(f"  WARNING: No matching event for sk_u_commit={v['sk_u_commit']}")

    print(f"\nFixed: {fixed}")
    print(f"Still missing: {still_missing}")

    # Step 4: Write updated file
    with open(VOUCHERS_FILE, "w") as f:
        for v in vouchers:
            f.write(f"{v['sk_u']}\n")
            f.write(f"{v['sk_u_commit']}\n")
            f.write(f"{v['boc']}\n")
            f.write("\n")

    print(f"\nUpdated {VOUCHERS_FILE}")
    print(f"Final stats: {len(vouchers)} vouchers, {sum(1 for v in vouchers if v['boc'] == 'NO_EVENT')} still NO_EVENT")


if __name__ == "__main__":
    main()
