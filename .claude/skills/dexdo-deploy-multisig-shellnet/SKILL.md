---
name: dexdo-deploy-multisig-shellnet
description: Deploy and fund a single-custodian Multisig wallet on Acki Nacki Shellnet — install tvm-cli, generate a 12-word seed + keypair, fund the precomputed address from the public giver, and deploy the wallet until it is Active. The output (seed phrase + keypair + address) is the INPUT to the deposit skill (`dexdo-deposit-shellnet`). Load when the user wants to create/deploy a multisig wallet on Shellnet, get a Shellnet wallet for DEXDO, or run the multisig deploy flow.
---

# DEXDO — Deploy Multisig (Shellnet)

Skill **#0a** in the series. **The output is a deployed, Active `Multisig`** wallet (single custodian, `reqConfirms=1`) under the user's control, plus its recovery material:

- `multisig/Multisig.seed` — the 12-word seed phrase (SECRET, only source of recovery);
- `multisig/Multisig.keys.json` — the wallet keypair (SECRET);
- the multisig address (`0:<hex>`).

These three are the **input** to the next skill, **`dexdo-deposit-shellnet`** (deposit onto the DEX / create PrivateNotes), which accepts the multisig either as the seed phrase or as the key pair. This skill stops once the wallet is Active and funded — it does **not** create PrivateNotes.

After the run the user has:

- a deployed `Multisig` (single custodian, `reqConfirms=1`) on Shellnet;
- a native-vmshell + SHELL (ECC currency 2) reserve on the wallet, enough for deploy gas plus a head-start on later trading/deposit calls.

## When to run

This skill targets **Shellnet** (Acki Nacki's public testnet). If the user is on Mainnet or another network — stop and say this skill does not apply; mainnet has no public giver and a different funding flow (separate skill).

Otherwise load the skill when:

- the user says "deploy a multisig", "create a Shellnet wallet", "set up a wallet for DEXDO", or otherwise needs a wallet before they can deposit;
- the user is starting DEXDO onboarding and has no multisig yet (deploy first, then run `dexdo-deposit-shellnet`).

If the user already has a deployed multisig (seed phrase or keypair in hand) and only wants to deposit / create PrivateNotes — skip this skill and go straight to **`dexdo-deposit-shellnet`**.

## Scope

- **Network:** Shellnet only. Mainnet is out of scope.
- **Wallet:** a standard `Multisig` with **a single custodian**. Multi-custodian configs require extra `confirmTransaction` calls and are not covered here.
- **Stops at:** an Active, funded multisig. PrivateNote creation lives in `dexdo-deposit-shellnet`.

## Out of scope

- Creating / funding PrivateNotes (→ `dexdo-deposit-shellnet`).
- Trading (→ research / trading skills).
- Mainnet onboarding.
- Recovery from a lost seed phrase — there is no recovery, the user MUST back it up.

## Setup

The agent walks the user through environment setup **before** any on-chain action. For each item: **check first, install only what's missing**. Setup is idempotent — on a partially-configured machine it skips what's already there.

This skill needs only **`tvm-cli`** plus the Multisig/giver ABI+TVC artifacts — no Rust toolchain and no dexdo checkout (those are only needed by the deposit skill's binary).

Target layout after this skill:

```
<WORKSPACE>/
├── multisig/   wallet: Multisig.keys.json + Multisig.seed (SECRET) + Multisig.abi.json + Multisig.tvc (artifacts)
└── giver/      GiverV3.abi.json — shellnet-only funding artifact
```

`multisig/` is the universal layout (same on mainnet); `giver/` is network-specific (shellnet). Secrecy is per-file: recovery-critical are `Multisig.keys.json` and `Multisig.seed` (they go into the back-up checklist at the end); `*.abi.json`/`*.tvc` are disposable and re-downloadable.

### Setup 0.1 — Working directory

First confirm the base CLI prerequisites are present (`git`, `curl`, `jq` — used throughout setup and funding); the skill installs `tvm-cli` itself but assumes these three:

```sh
for tool in git curl jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing prerequisite: $tool (install it, then re-run)"; exit 1; }
done
```

Default workspace: `~/dexdo-workspace` (override via `$WORKSPACE`). Export the variable and create the folders; lock down `multisig/` (it holds secrets):

```sh
export WORKSPACE="${WORKSPACE:-$HOME/dexdo-workspace}"
mkdir -p "$WORKSPACE/multisig" "$WORKSPACE/giver"
chmod 700 "$WORKSPACE/multisig"
```

### Setup 0.2 — `tvm-cli`

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

## Autonomy contract — read carefully

The skill writes user-owned secrets to disk and submits real transactions. Run the flow without gating prompts — the user who requested the deploy has thereby already authorized the full set of on-chain actions in the "Overall flow" below. The constraints that remain:

- **Show every command in full** before executing. No silent work — this is about transparency, not consent.
- The 12-word seed phrase is the **only** source of recovery. Step 2 prints it to stdout AND writes it to `multisig/Multisig.seed` (mode `0600`) so it isn't lost in scrollback. Show it to the user and **inform** them an offline copy is needed (paper / password manager) — this is a warning, not a blocker. Don't put the seed into commands/logs/summaries that get pasted somewhere.
- `multisig/Multisig.keys.json` and `multisig/Multisig.seed` live **only** in `$WORKSPACE/multisig/` (mode 0700) on the user's machine. Don't copy them into the chat log, the paste buffer, or anywhere else.
- If a step fails mid-way — **stop and report**. Don't retry blindly: partial on-chain state may need inspection before the next attempt. This is an error stop, not a consent request.

## Overall flow

```
[1] fetch multisig + giver artifacts from ackinacki-kit (Multisig.abi.json, Multisig.tvc, GiverV3.abi.json)
[2] tvm-cli genphrase + genaddr → 12-word seed + multisig keypair + precomputed address
      └─ SHOW THE SEED AND WARN ABOUT THE OFFLINE BACKUP (don't block on it)
[3] giver: 10_000 vmshell + 10_000 SHELL ecc[2] to the precomputed multisig address (sendCurrencyWithFlag, flag=17) — bootstrap native balance for deploy gas
[4] tvm-cli deploy → multisig becomes Active
[5] handoff: multisig address + seed/keys location + back-up checklist + pointer to dexdo-deposit-shellnet
```

## Step 1 — Prepare artifacts

The Multisig (ABI + TVC) and the GiverV3 ABI are vendored in `ackinacki-kit` — download the three files straight from raw of that tag (`v2.1.0`), without cloning the repo:

```sh
KIT_RAW="https://raw.githubusercontent.com/gosh-sh/ackinacki-kit/v2.1.0"
curl -fL -o "$WORKSPACE/multisig/Multisig.abi.json" "$KIT_RAW/contracts/abi/multisig/Multisig.abi.json"
curl -fL -o "$WORKSPACE/multisig/Multisig.tvc"      "$KIT_RAW/contracts/abi/multisig/Multisig.tvc"
curl -fL -o "$WORKSPACE/giver/GiverV3.abi.json"     "$KIT_RAW/contracts/abi/giver/GiverV3.abi.json"
```

Check that all three files are non-empty.

> **Pin the tag.** The deposit skill re-derives the same address from the same `Multisig.tvc` via `genaddr`. Both skills MUST use the `v2.1.0` artifacts — a different TVC yields a different address for the same keypair.

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
- **SHELL (ECC currency 2)** — a post-deploy gas-voucher reserve (the deposit skill tops this up further).

**Empirically, against the public shellnet giver:**

- The `sendCurrencyWithFlag` method with **`flag=17`** (= `16|1` — the TVM flags "bounce on action error" + "pay forward fee from msg"). Neither 16 nor 1 alone works to bootstrap an already-`Uninit` address.
- A single call **must** fill both `value` (native) and `ecc.2` (SHELL). Send only `value` — native isn't credited (an uninit account only receives ecc from the giver). Send only ecc — native stays 0 and the node rejects deploy.
- What actually lands on dst is **as much as `ecc.2` specifies**: the account's native balance becomes = `ecc.2` (the giver routes ECC SHELL into native vmshell on Uninit accounts). The `value` field is decorative for the bootstrap, but drop it and native crediting won't fire. Just keep both in sync.

Safe baseline for a single deploy: 100 vmshell is enough, but for headroom we send 10_000 vmshell (deploy gas is pennies, the remainder keeps sitting on the multisig as a reserve for later deposit/trading calls):

```sh
GIVER_TX=$(tvm-cli -j -u shellnet.ackinacki.org callx \
  --abi "$WORKSPACE/giver/GiverV3.abi.json" \
  --addr "$GIVER_ADDR" \
  -m sendCurrencyWithFlag \
  '{"dest":"'"$MULTISIG_ADDRESS"'","value":10000000000000,"ecc":{"2":10000000000000},"flag":17}' \
  | jq -r .tx_hash)
echo "giver_tx=$GIVER_TX"
# Guard: a failed callx yields empty/"null" — polling that would spin forever.
test -n "$GIVER_TX" && [ "$GIVER_TX" != "null" ] || { echo "giver callx returned no tx_hash — inspect the callx output above; stop and report"; exit 1; }
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

tvm-cli 3.0 requires `--dst-dapp-id` on deploy — this sets the wallet's dApp. Pass `$MS_ID` (its own account-id) so the dapp_id matches what genaddr computed and what the deposit skill re-derives.

Check that the contract became Active:

```sh
tvm-cli account "$MS_ADDR"
```

Expected: `acc_type: Active`. Deploy consumes as much gas as the VM needs (a fraction of a vmshell — ~0.06 in a run), not "everything available".

## Step 5 — Handoff

Show the user a summary:

```
Multisig deployed and Active (Shellnet).

Multisig address:  <multisig_address>
Keys file:         $WORKSPACE/multisig/Multisig.keys.json
Seed file:         $WORKSPACE/multisig/Multisig.seed

Next: deposit onto the DEX / create PrivateNotes with the `dexdo-deposit-shellnet`
skill. It takes this multisig as input — pass it EITHER the seed phrase (from
Multisig.seed) OR the key pair (Multisig.keys.json). If you run the deposit skill
in the same workspace, it auto-detects $WORKSPACE/multisig/Multisig.keys.json.
```

**Back-up checklist** (show the user as a reminder; don't gate the deposit skill on these items):

- [ ] `$WORKSPACE/multisig/Multisig.seed` — contents also saved offline (paper / password manager) — disk loss = funds loss
- [ ] `$WORKSPACE/multisig/Multisig.keys.json` copied somewhere safe

The seed phrase **and** the keypair are each sufficient to control the wallet — either one is the input to `dexdo-deposit-shellnet`. Keep both secret.

## What the user has after the skill

An Active, funded single-custodian Multisig on Shellnet, plus its seed phrase and keypair. This is the prerequisite for **`dexdo-deposit-shellnet`**, which funds the wallet with deposit currencies and creates the PrivateNotes the user trades with.
