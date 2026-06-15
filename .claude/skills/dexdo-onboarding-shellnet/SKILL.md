---
name: dexdo-onboarding-shellnet
description: Walk a user through end-to-end onboarding to DEXDO on Acki Nacki Shellnet — install tvm-cli, deploy a multisig wallet, fund it from the public giver, then deploy and fund three PrivateNotes (one per currency: SHELL / NACKL / USDC) so the user is ready to trade. Load when the user wants to start using DEXDO, open an account, run `onboard_user_shellnet`, or talk about getting tokens / deploying a multisig / PrivateNote setup.
---

# DEXDO Onboarding — Shellnet

Skill **#0** in the series. **The output is a list of PrivateNotes** (one per currency: SHELL / NACKL / USDC), saved to files, usable two ways:

- **directly with the libraries** (`dodex-sdk` / `ackinacki-kit`) — sign on-chain operations with each note's key from its file;
- **loaded into the API service** — each `notes/<tt>.account.json` is a ready-to-send `POST /api/v1/accounts` body.

After the run the user has:

- a deployed `Multisig` (single custodian, `reqConfirms=1`) under their control;
- three deployed PrivateNotes (PN) — one per currency (SHELL / NACKL / USDC), each with its own deposit;
- each PN topped up with vmshell (gas) for trading operations;
- a multisig holding a native-vmshell reserve for future trading calls.

## When to run

This skill targets **Shellnet** (Acki Nacki's public testnet). If the user is on Mainnet or another network — stop and say this skill does not apply; mainnet onboarding has a different funding flow and is a separate skill.

Otherwise load the skill when:

- the user says "I want to start using DEXDO", "open an account", "deposit", "set up a wallet", "fund a PrivateNote on Shellnet", or otherwise indicates they have no active PN on Shellnet yet;
- the user explicitly asks to run `onboard_user_shellnet`, the multisig flow on Shellnet, or asks how to get Shellnet tokens onto DEXDO;
- the user has Shellnet tokens (or wants to get them from the public giver) and wants to participate in DEXDO markets.

## Scope

- **Network:** Shellnet only. Mainnet has no public giver and needs a different funding flow — out of scope.
- **Wallet:** a standard `Multisig` with **a single custodian**. Multi-custodian configs require extra `confirmTransaction` calls and are not covered here.
- **Currency:** a single run creates **three PNs** — one each for SHELL / NACKL / USDC (Step 6 runs the binary three times). Don't need a currency — drop it from the loop in Step 6 and from the funding in Step 5.

## Out of scope

- Trading itself (covered by the research / trading skills).
- Mainnet onboarding.
- Recovery from a lost seed phrase — there is no recovery, the user MUST back it up.

## Setup

The agent walks the user through environment setup **before** any on-chain action. For each item: **check first, install/clone only what's missing**. Setup is idempotent — on a partially-configured machine it simply skips what's already there.

Target layout after setup:

```
<WORKSPACE>/
├── dexdo/        gosh-sh/dexdo @ DEX-SKILLS
├── multisig/     wallet: Multisig.keys.json + Multisig.seed (SECRET) + Multisig.abi.json + Multisig.tvc (artifacts)
├── notes/        PrivateNotes (SECRET): pn_state.<tt>.json (resume) + <tt>.account.json (POST /accounts body for loading into the API)
└── giver/        GiverV3.abi.json — shellnet-only funding artifact (no such folder on mainnet)
```

`multisig/` and `notes/` are the universal layout (same on mainnet); `giver/` is network-specific (shellnet). Secrecy is per-file: recovery-critical are `Multisig.keys.json`, `Multisig.seed`, and all `pn_state.*.json` (they go into the back-up checklist in Step 7); `*.abi.json`/`*.tvc` are disposable and re-downloadable.

The dexdo SDK pulls `ackinacki-kit` as a git dependency (branch `dexdo-agent-integration`) — cargo clones the kit into its own cache at build time. We do **not** clone the kit locally: the three ABI/TVC artifacts are downloaded straight from `raw.githubusercontent` of that branch (Step 1).

### Setup 0.1 — Working directory

Default workspace: `~/dexdo-workspace` (override via `$WORKSPACE`). Export the variable and create the folders; lock down `multisig/` and `notes/` (they hold secrets):

```sh
export WORKSPACE="${WORKSPACE:-$HOME/dexdo-workspace}"
mkdir -p "$WORKSPACE/multisig" "$WORKSPACE/notes" "$WORKSPACE/giver"
chmod 700 "$WORKSPACE/multisig" "$WORKSPACE/notes"
```

### Setup 0.2 — Rust toolchain

```sh
cargo --version || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo --version
```

The SDK builds on stable. Nightly is only needed for `cargo fmt` — not required for skill #0.

### Setup 0.3 — `tvm-cli`

Where we install: **`~/.local/bin/tvm-cli`** (per-user, no `sudo`). First add that dir to `$PATH` if it isn't there:

```sh
mkdir -p "$HOME/.local/bin"
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.profile" && export PATH="$HOME/.local/bin:$PATH" ;;
esac
```

Check presence with the **subcommand** `version` (not the `--version` flag — on 2.24.x the flag exits 0 with empty stdout and silently lies). `tvm-cli version` prints `tvm-cli <semver>` plus commit-id / build-date / branch. If something prints — skip the rest of this section. Otherwise download the matching asset from the **latest** `tvmlabs/tvm-sdk` release (assets are named `tvm-cli-<version>-<platform>.tar.gz`):

```sh
if ! command -v tvm-cli >/dev/null 2>&1; then
  # Host mapping (uname -s / uname -m) -> platform substring in the asset name.
  # Canonical list — https://github.com/tvmlabs/tvm-sdk/releases
  # (currently: macos-arm64, linux-musl-amd64, linux-musl-arm64, x86_64 [macOS x86_64]).
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)   PLATFORM_SUBSTR="macos-arm64" ;;
    Darwin-x86_64)  PLATFORM_SUBSTR="x86_64" ;;
    Linux-x86_64)   PLATFORM_SUBSTR="linux-musl-amd64" ;;
    Linux-aarch64)  PLATFORM_SUBSTR="linux-musl-arm64" ;;
    *) echo "unsupported host $(uname -s)-$(uname -m); inspect https://github.com/tvmlabs/tvm-sdk/releases"; exit 1 ;;
  esac

  # Strict filter: tvm-cli (not tvm-debugger), our platform, .tar.gz.
  RELEASE_URL=$(curl -sL https://api.github.com/repos/tvmlabs/tvm-sdk/releases/latest \
    | jq -r --arg p "$PLATFORM_SUBSTR" '
        .assets[]
        | select(.name | startswith("tvm-cli-"))
        | select(.name | endswith(".tar.gz"))
        | select(.name | contains($p))
        | .browser_download_url' \
    | head -1)
  test -n "$RELEASE_URL" || { echo "no tvm-cli asset matched '$PLATFORM_SUBSTR'; inspect releases page manually"; exit 1; }

  WORK=$(mktemp -d)
  curl -L "$RELEASE_URL" -o "$WORK/tvm-cli.tar.gz"
  tar -xzf "$WORK/tvm-cli.tar.gz" -C "$WORK"

  # The archive may put the binary at the root or inside a versioned subdir. Find it explicitly.
  BIN=$(find "$WORK" -type f -name tvm-cli -perm -u+x | head -1)
  test -n "$BIN" || { echo "tvm-cli binary not found inside $WORK after extract; inspect manually"; exit 1; }

  install -m 0755 "$BIN" "$HOME/.local/bin/tvm-cli"
  rm -rf "$WORK"
fi
tvm-cli version
```

If the host isn't covered by the table — print the releases page URL and stop, don't guess.

Point the endpoint at Shellnet:

```sh
tvm-cli config -g --url shellnet.ackinacki.org
```

### Setup 0.4 — Clone dexdo

Clone only `dexdo`. `ackinacki-kit` isn't needed locally — cargo pulls it as a git dependency at build time (Setup 0.5), and the ABI/TVC artifacts are downloaded from raw in Step 1.

```sh
[ -d "$WORKSPACE/dexdo" ] || git clone https://github.com/gosh-sh/dexdo.git "$WORKSPACE/dexdo"
git -C "$WORKSPACE/dexdo" fetch origin DEX-SKILLS
git -C "$WORKSPACE/dexdo" checkout DEX-SKILLS
```

If checkout fails because the branch is absent on origin — **stop and report**.

### Setup 0.5 — Build `onboard_user_shellnet`

```sh
cd "$WORKSPACE/dexdo/sdk" && cargo build --release --bin onboard_user_shellnet
./target/release/onboard_user_shellnet --help
```

First build — 5–15 minutes on a clean machine (heavy halo2 stack). Subsequent builds are fast.

If the build fails resolving `ackinacki-kit` — check that the repo's `dexdo-agent-integration` branch is pushed to origin (the dexdo SDK pulls it as a git dependency). When the kit branch changes, refresh the lock: `cargo update -p ackinacki-kit`.

After setup the body of the skill operates on:
- `$WORKSPACE/dexdo` — for the repo,
- `$WORKSPACE/multisig` — wallet: `Multisig.keys.json` + `Multisig.seed` (secret) + `Multisig.abi.json` + `Multisig.tvc`,
- `$WORKSPACE/notes` — `pn_state.{shell,nackl,usdc}.json` (secret),
- `$WORKSPACE/giver` — `GiverV3.abi.json`,
- `$MULTISIG_ADDRESS` — exported in Step 2.

## Autonomy contract — read carefully

The skill writes user-owned secrets to disk and submits real transactions. Run the flow without gating prompts — the user who requested onboarding has thereby already authorized the full set of on-chain actions in the "Overall flow" below. The constraints that remain:

- **Show every command in full** before executing. No silent work — this is about transparency, not consent.
- The 12-word seed phrase is the **only** source of recovery. Step 2 prints it to stdout AND writes it to `multisig/Multisig.seed` (mode `0600`) so it isn't lost in scrollback. Show it to the user and **inform** them an offline copy is needed (paper / password manager) — this is a warning, not a blocker. Don't put the seed into commands/logs/summaries that get pasted somewhere.
- `multisig/Multisig.keys.json` (multisig keypair) and all `notes/pn_state.<tt>.json` (PN keypair + state, one file per currency) live **only** in `$WORKSPACE/multisig/` and `$WORKSPACE/notes/` (mode 0700) on the user's machine. Don't copy them into the chat log, the paste buffer, or anywhere else.
- If a step fails mid-way — **stop and report**. Don't retry blindly: partial on-chain state may need inspection before the next attempt. This is an error stop, not a consent request.

## Overall flow

```
[1] fetch multisig + giver artifacts from ackinacki-kit (Multisig.abi.json, Multisig.tvc, GiverV3.abi.json)
[2] tvm-cli genphrase + genaddr → 12-word seed + multisig keypair + precomputed address
      └─ SHOW THE SEED AND WARN ABOUT THE OFFLINE BACKUP (don't block on it)
[3] giver: 10_000 vmshell + 10_000 SHELL ecc[2] to the precomputed multisig address (sendCurrencyWithFlag, flag=17) — bootstrap native balance for deploy gas
[4] tvm-cli deploy → multisig becomes Active
[5] giver: 1000 SHELL + 100 NACKL + 100 USDC to the Active multisig (flag=1) — deposit + gas vouchers for 3 PNs
[6] cargo run --bin onboard_user_shellnet × 3 (shell, nackl, usdc) — three PNs; per PN: pn_state.<tt>.json (resume) + <tt>.account.json (POST /accounts body for the API)
[7] handoff: summary of the three PNs + where the <tt>.account.json files are + back-up checklist
```

## Step 1 — Prepare artifacts

The Multisig (ABI + TVC) and the GiverV3 ABI are vendored in `ackinacki-kit` — download the three files straight from raw of that branch (`dexdo-agent-integration`), without cloning the repo:

```sh
KIT_RAW="https://raw.githubusercontent.com/gosh-sh/ackinacki-kit/dexdo-agent-integration"
curl -fL -o "$WORKSPACE/multisig/Multisig.abi.json" "$KIT_RAW/contracts/abi/multisig/Multisig.abi.json"
curl -fL -o "$WORKSPACE/multisig/Multisig.tvc"      "$KIT_RAW/contracts/abi/multisig/Multisig.tvc"
curl -fL -o "$WORKSPACE/giver/GiverV3.abi.json"     "$KIT_RAW/contracts/abi/giver/GiverV3.abi.json"
```

Check that all three files are non-empty.

## Step 2 — Generate the multisig keys

`tvm-cli genphrase --dump` writes the keypair to disk but prints the seed phrase only to stdout — so capture stdout, parse out the seed, and store it next to the keypair in `multisig/Multisig.seed` (mode 0600), then pull the precomputed address out of `genaddr`:

```sh
cd "$WORKSPACE/multisig"

GENPHRASE_OUT=$(tvm-cli genphrase --dump Multisig.keys.json)
echo "$GENPHRASE_OUT"

# Save the seed next to the keypair so it doesn't live only in scrollback.
SEED=$(echo "$GENPHRASE_OUT" | sed -nE 's/^Seed phrase: "(.*)"$/\1/p')
test -n "$SEED" || { echo "failed to parse seed phrase from genphrase output"; exit 1; }
umask 077 && printf '%s\n' "$SEED" > Multisig.seed
chmod 600 Multisig.keys.json Multisig.seed

GENADDR_OUT=$(tvm-cli genaddr \
  --abi "$WORKSPACE/multisig/Multisig.abi.json" \
  --setkey Multisig.keys.json \
  --save "$WORKSPACE/multisig/Multisig.tvc")
echo "$GENADDR_OUT"
export MULTISIG_ADDRESS=$(echo "$GENADDR_OUT" | sed -nE 's/^Raw address: (0:[a-f0-9]+)$/\1/p')
test -n "$MULTISIG_ADDRESS" || { echo "failed to parse multisig address from genaddr output"; exit 1; }
echo "multisig=$MULTISIG_ADDRESS"

# tvm-cli 3.0 accepts addresses ONLY in the `dapp_id::account_id` form (64 hex
# each, no 0x, no workchain). The old `0:…` form is rejected. The wallet deploys
# under its OWN account-id as the dapp_id (genaddr prints `dapp::account: <hex>::<hex>`),
# so dapp_id == the multisig's account_id.
export MS_ID="${MULTISIG_ADDRESS#0:}"          # bare 64-hex account id (= dapp_id)
export MS_ADDR="${MS_ID}::${MS_ID}"            # address form for `tvm-cli account`
# The public giver is a system contract under the System dApp (all-zero id):
export GIVER_ADDR="0000000000000000000000000000000000000000000000000000000000000000::1111111111111111111111111111111111111111111111111111111111111111"
echo "ms_addr=$MS_ADDR"
```

Artifacts after this step:

- `$WORKSPACE/multisig/Multisig.keys.json` — keypair, mode `0600`
- `$WORKSPACE/multisig/Multisig.seed` — 12-word seed, mode `0600` (**warn the user** that disk-only storage = funds loss if the disk is lost, and ask them to copy it offline — but this is informational, don't stop the run)
- `$MULTISIG_ADDRESS` — exported into the session for the later steps

Throughout the skill we confirm crediting by **polling the tx hash in GraphQL** (see Step 3) — transaction history is reliable. `tvm-cli account "$MS_ADDR"` (in the `dapp_id::account_id` form) shows both the pre-deploy account (`acc_type: Uninit` + balance after funding) and Active after deploy — we use it as a balance sanity check.

## Step 3 — Pre-deploy funding

Deploy needs two balances on the pre-deploy address:
- **native vmshell** — the gas payer for the deploy compute/action itself (the node rejects deploy with `"empty balance"` if native = 0);
- **SHELL (ECC currency 2)** — a post-deploy gas-voucher reserve (topped up further in Step 5).

**Empirically, against the public shellnet giver:**

- The `sendCurrencyWithFlag` method with **`flag=17`** (= `16|1` — the TVM flags "bounce on action error" + "pay forward fee from msg"). Neither 16 nor 1 alone works to bootstrap an already-`Uninit` address.
- A single call **must** fill both `value` (native) and `ecc.2` (SHELL). Send only `value` — native isn't credited (an uninit account only receives ecc from the giver). Send only ecc — native stays 0 and the node rejects deploy.
- What actually lands on dst is **as much as `ecc.2` specifies**: the account's native balance becomes = `ecc.2` (the giver routes ECC SHELL into native vmshell on Uninit accounts). The `value` field is decorative for the bootstrap, but drop it and native crediting won't fire. Just keep both in sync.

Safe baseline for a single deploy: 100 vmshell is enough, but for headroom we send 10_000 vmshell (deploy gas is pennies, the remainder keeps sitting on the multisig after Step 5 in the native balance as a reserve for future trading `submitTransaction`s):

```sh
GIVER_TX=$(tvm-cli -j -u shellnet.ackinacki.org callx \
  --abi "$WORKSPACE/giver/GiverV3.abi.json" \
  --addr "$GIVER_ADDR" \
  -m sendCurrencyWithFlag \
  '{"dest":"'"$MULTISIG_ADDRESS"'","value":10000000000000,"ecc":{"2":10000000000000},"flag":17}' \
  | jq -r .tx_hash)
echo "giver_tx=$GIVER_TX"
```

**Confirmation — poll the tx hash in GraphQL.** When the tx resolves by hash — the block is finalized and the giver tx landed. Then check the balance: `tvm-cli -u shellnet.ackinacki.org account "$MS_ADDR"` should show `acc_type: Uninit`, `balance: ≥10000000000000 nanovmshell` (and `dapp_id` equal to the account-id). If balance=0 — Step 4 will reject with "empty balance"; resend the funding.

```sh
until curl -sX POST https://shellnet.ackinacki.org/graphql -H 'Content-Type: application/json' \
  --data "{\"query\":\"{blockchain{transaction(hash:\\\"$GIVER_TX\\\"){now aborted}}}\"}" \
  | grep -q '"now"'; do sleep 1; done
echo "funding confirmed"
tvm-cli -u shellnet.ackinacki.org account "$MS_ADDR"
```

Note: on an `Uninit` account the dst_transaction in the giver out_msg is flagged `aborted=true` with `compute_type=0` (skipped — no code). This is **not** a bootstrap error; the node has nothing to compute and nothing to return, the value is credited to native+ecc anyway. The real sanity check is the balance via `tvm-cli account`, not the `aborted` flag.

## Step 4 — Deploy the multisig

Read the pubkey from the keys file and assemble the constructor payload. Single custodian, `reqConfirms=1`, `value` is the initial-data deploy balance:

```sh
PUBKEY=$(jq -r .public "$WORKSPACE/multisig/Multisig.keys.json")
tvm-cli deploy \
  --abi "$WORKSPACE/multisig/Multisig.abi.json" \
  --sign "$WORKSPACE/multisig/Multisig.keys.json" \
  --dst-dapp-id "$MS_ID" \
  "$WORKSPACE/multisig/Multisig.tvc" \
  "{\"owners_pubkey\":[\"0x${PUBKEY}\"],\"owners_address\":[],\"reqConfirms\":1,\"reqConfirmsData\":1,\"value\":10000000000}"
```

tvm-cli 3.0 requires `--dst-dapp-id` on deploy — this sets the wallet's dApp. Pass `$MS_ID` (its own account-id) so the dapp_id matches what genaddr computed and what the SDK looks it up under in Step 6.

Check that the contract became Active:

```sh
tvm-cli account "$MS_ADDR"
```

Expected: `acc_type: Active`. Deploy consumes as much gas as the VM needs (a fraction of a vmshell — ~0.06 in a run), not "everything available".

## Step 5 — Post-deploy funding (all three currencies)

The multisig is now Active — flag `=16` is no longer needed; use `flag=1`. Fund **all three ECC currencies** in one giver call: SHELL (currency 2), NACKL (currency 1), USDC (currency 3). With `flag=1` to an Active recipient they all land correctly in the `ecc` balance (verified empirically — with `flag=16` SHELL would go into native vmshell, which we don't want here).

Per-currency breakdown — sized for 3 PNs (one per token) with deposit nominal N100 + a 100-SHELL gas voucher per PN:

| Currency | id | decimals | Amount funded | Why |
|---|---|---|---|---|
| SHELL | 2 | 9 | **1000 SHELL** (`1_000_000_000_000` nano) | 100 for its own deposit voucher + 3×100 for the gas vouchers of all PNs + ~600 reserve for trading `submitTransaction`s |
| NACKL | 1 | 9 | **100 NACKL** (`100_000_000_000` nano) | N100 deposit voucher for the NACKL PN |
| USDC  | 3 | 6 | **100 USDC** (`100_000_000` nano) | N100 deposit voucher for the USDC PN |

If you want larger nominals (N1000 / N10000) — scale: each PN at the desired nominal is 10/100× more. SHELL = 100 (own deposit, if there's a SHELL PN at the same nominal) + 100×N (gas voucher) + reserve.

Send it all in one giver call:

```sh
FUND_TX=$(tvm-cli -j -u shellnet.ackinacki.org callx \
  --abi "$WORKSPACE/giver/GiverV3.abi.json" \
  --addr "$GIVER_ADDR" \
  -m sendCurrencyWithFlag \
  '{"dest":"'"$MULTISIG_ADDRESS"'","value":0,"ecc":{"1":100000000000,"2":1000000000000,"3":100000000},"flag":1}' \
  | jq -r .tx_hash)
echo "fund_tx=$FUND_TX"
```

Confirm by polling the hash:

```sh
until curl -sX POST https://shellnet.ackinacki.org/graphql -H 'Content-Type: application/json' \
  --data "{\"query\":\"{blockchain{transaction(hash:\\\"$FUND_TX\\\"){now aborted}}}\"}" \
  | grep -q '"now"'; do sleep 1; done
echo "post-deploy fund confirmed"
```

Verify all three currencies arrived:

```sh
tvm-cli -u shellnet.ackinacki.org account "$MS_ADDR" | grep -E 'acc_type|balance|ecc'
```

Expected: the `ecc:` line contains `{"1":"100000000000","2":"1000000000000","3":"100000000"}` (raw nominals). The native `balance` is whatever's left after deploy from the Step-3 funding.

## Step 6 — Run `onboard_user_shellnet` (three times, one PN per currency)

The binary takes a single `--token-type` per invocation and creates one PN. To get three PNs — run it three times, each with a different `--token-type` and its own `--output` (a separate state file so they don't overwrite each other). Each run goes through **three steps** (the binary prints `step 1/3 … 3/3`), state is idempotent:

```
step 1/3  deposit voucher (via multisig.submitTransaction) + RootPN.deployPrivateNote
step 2/3  SHELL gas voucher (via multisig.submitTransaction) + RootPN.sendEccShellToPrivateNote
step 3/3  PrivateNote.getDetails — sanity check
```

Both multisig stages (in steps 1/3 and 2/3) go through `multisig.submitTransaction`: after Step 5 the multisig holds ECC SHELL (currency 2) in `ecc[2]`, plus the needed deposit token in `ecc[1|3]`. The ECC currencies forward through correctly because incoming funds with `flag=1` are credited as ECC, not burned into native.

Run for all three currencies (order doesn't matter — each run is self-contained):

```sh
cd "$WORKSPACE/dexdo/sdk"

for TT in shell nackl usdc; do
  cargo run --release --bin onboard_user_shellnet -- \
    --multisig-address "$MULTISIG_ADDRESS" \
    --multisig-keys-file "$WORKSPACE/multisig/Multisig.keys.json" \
    --nominal N100 \
    --token-type "$TT" \
    --endpoint shellnet.ackinacki.org \
    --output "$WORKSPACE/notes/pn_state.$TT.json"
done
```

After the runs, `$WORKSPACE/notes/` will hold **two** files per currency:

- `pn_state.<tt>.json` — the binary's resume state (address, PN keys, DIH in decimal, checkpoints). **Not loaded into the API.**
- `<tt>.account.json` — the `POST /api/v1/accounts` request body (`pnAddress`/`pnPubkeyHex`/`pnSeckeyHex`/`pnDihHex`, **DIH in hex**). This is the artifact for loading the PN into the API service later (by hand or via the registration endpoint). **This skill sends it nowhere** — it only saves it.

So: `shell.account.json`, `nackl.account.json`, `usdc.account.json` + three `pn_state.*.json`.

Notes for the agent:

- The **first** run generates the KZG SRS (~64 MB, ~30 s CPU) under `./params/`. The next two reuse it → faster.
- The halo2 deposit-voucher step is slow (several seconds per stage). Don't treat silence in the output as a hang for at least a few minutes.
- The tool **persists state after each successful step**. If one of the three runs fails — re-run with the same `--output`/`--multisig-address`/`--nominal`/`--token-type`/`--endpoint` and it resumes from the first incomplete step. Changing any of them for an existing `--output` is rejected (common case: a stale state file from a previous multisig — the binary fails with `existing state multisig_address … != requested …`; remove/rename the old state and re-run).
- Both `pn_state.<tt>.json` and `<tt>.account.json` contain the PN's **secret key** (`pnSeckeyHex`). Treat them all as user secrets — don't commit, don't paste.
- If a PN isn't needed for some currency — just drop it from `for TT in ...` and don't fund the corresponding entry in Step 5.

## Step 7 — Handoff

Read the three `pn_state.<tt>.json` files and show a summary table:

```
DEXDO onboarding complete (Shellnet).

Multisig wallet:   <multisig_address>

Token       | PrivateNote address              | Nominal | API file to load
------------|----------------------------------|---------|--------------------------
SHELL (2)   | <pn_address from shell state>    | N100    | $WORKSPACE/notes/shell.account.json
NACKL (1)   | <pn_address from nackl state>    | N100    | $WORKSPACE/notes/nackl.account.json
USDC (3)    | <pn_address from usdc state>     | N100    | $WORKSPACE/notes/usdc.account.json

Each <tt>.account.json is a ready POST /api/v1/accounts body (load into the API
later; this skill sends nothing). Alongside it sits pn_state.<tt>.json (resume).

Keys file:         $WORKSPACE/multisig/Multisig.keys.json
Seed file:         $WORKSPACE/multisig/Multisig.seed
```

**Back-up checklist** (show the user at the end as a reminder; don't gate later skills on these items):

- [ ] `$WORKSPACE/multisig/Multisig.seed` — contents also saved offline (paper / password manager) — disk loss = funds loss
- [ ] `$WORKSPACE/multisig/Multisig.keys.json` copied somewhere safe
- [ ] `$WORKSPACE/notes/` in full (`pn_state.*.json` + `*.account.json`) copied somewhere safe — both file types contain PN secret keys

The PN keys (`pnSeckeyHex`) inside `<tt>.account.json` / `pn_state.<tt>.json` are what the API/trading skill signs orders with. `<tt>.account.json` is what you load into the API service once you get to trading.

## What the user has after the skill

Three funded on-chain PrivateNotes (one per currency: SHELL / NACKL / USDC) under user-held keys, ready to:

- place bets on prediction markets;
- buy/sell full sets of outcome tokens;
- place/cancel orders in the outcome books;
- claim payouts after a market resolves.

All of this is covered by the **research / trading skills** built on top of this onboarding.
