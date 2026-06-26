---
name: dexdo-deposit-shellnet
description: Deposit onto DEXDO on Acki Nacki Shellnet by creating exactly the PrivateNotes the user asks for — any count, of any currency (NACKL / SHELL / USDC) at any nominal (e.g. "3 notes of 10000 NACKL", or one note per currency) — from an ALREADY-DEPLOYED multisig. The multisig is an INPUT, supplied either as a 12-word seed phrase or as a key pair (keys.json file or 64-hex secret key); the skill re-derives the address, funds the wallet with the needed currencies from the public giver, then deploys and funds each PrivateNote (each with its own random key). Load when the user wants to deposit, create one or more PrivateNotes, fund notes, or trade — and already has a multisig (if they don't, run `dexdo-deploy-multisig-shellnet` first).
---

# DEXDO — Deposit / Create PrivateNotes (Shellnet)

Skill **#0b** in the series. It takes an **already-deployed multisig as input** and produces **exactly the PrivateNotes the user asked for** — any number, of any currency, at any nominal (see [What to create](#what-to-create--the-notes-parameters)). Each note is saved to files, usable two ways:

- **directly with the libraries** (`dodex-sdk` / `ackinacki-kit`) — sign on-chain operations with each note's key from its file;
- **loaded into the API service** — each note's `account.json` is a ready-to-send `POST /api/v1/accounts` body.

After the run the user has:

- the requested PrivateNotes (PN) deployed — each with its own random key and its own deposit at the requested nominal;
- each PN topped up with vmshell (gas) for trading operations;
- the multisig holding a SHELL reserve for future trading calls.

> **Default (no spec given):** if the user just says "onboard / deposit" with no count/currency, fall back to the classic full onboarding — **one `N100` note each of SHELL, NACKL, USDC**. Any explicit request ("3 notes of 10000 NACKL") overrides this.

## Input — the multisig (required)

This skill does **not** deploy a multisig — it consumes one. The deploy step lives in **`dexdo-deploy-multisig-shellnet`**; if the user has no multisig yet, run that skill first, then this one.

The multisig is supplied **one of two ways** (the user picks whichever they have):

1. **Seed phrase** — the 12 words from `Multisig.seed` (or wherever the user backed them up).
2. **Key pair** — either an existing `Multisig.keys.json` file, or a raw 64-hex secret key.

Both reduce to the same thing: a `Multisig.keys.json` keypair, from which the skill re-derives the on-chain address with `genaddr`. The seed phrase and the key pair are equivalent — either uniquely controls the wallet. Resolving the input is **Step 2**.

> The address is **not** asked for separately — it is deterministically recomputed from the keypair + the `Multisig.tvc` (`v2.1.0`). This is why both skills must use the same artifact tag.

## What to create — the notes (parameters)

Resolve **what to create** from the user's request, then build a `NOTES` work-list the rest of the skill consumes. Three things define each note:

| Parameter | Meaning | Values | Default |
|---|---|---|---|
| **count** | how many notes | any `N ≥ 1` | — |
| **token** | deposit currency of each note | `nackl` / `shell` / `usdc` | — |
| **nominal** | deposit per note | `N100` / `N1000` / `N10000` (= 100 / 1 000 / 10 000 units) | `N100` |

Examples:

- "3 notes of 10000 NACKL" → `count=3, token=nackl, nominal=N10000`.
- "a note with 1000 USDC" → `count=1, token=usdc, nominal=N1000`.
- "onboard me" / no spec → the default full set: one `N100` note each of shell, nackl, usdc.

**Each note is an independent PrivateNote with its own random key** — the binary calls `generate_random_sign_keys` once per fresh `--output`, so creating N notes of the *same* `(token, nominal)` with distinct state files yields N distinct notes (distinct DIH, distinct address). No special index/derivation flag is needed.

Build the work-list as `token:nominal` entries — **one entry per note to create**. This single array drives both funding (Step 3) and creation (Step 4):

```sh
# Fill this from the user's request. One entry per note.
# Example — "3 notes of 10000 NACKL":
NOTES=(nackl:N10000 nackl:N10000 nackl:N10000)
# Example — default full onboarding (one per currency at N100):
# NOTES=(shell:N100 nackl:N100 usdc:N100)
# Example — a mix is fine too:
# NOTES=(nackl:N10000 nackl:N10000 shell:N1000)
echo "will create ${#NOTES[@]} note(s): ${NOTES[*]}"
```

Nominal → units: `N100`=100, `N1000`=1 000, `N10000`=10 000. Raw on-chain value = `units × 10^decimals` (NACKL & SHELL: 9 decimals; USDC: 6).

## When to run

This skill targets **Shellnet** (Acki Nacki's public testnet). If the user is on Mainnet or another network — stop and say this skill does not apply; mainnet has no public giver and a different funding flow (separate skill).

Otherwise load the skill when:

- the user says "deposit", "create PrivateNotes", "fund a PrivateNote on Shellnet", "I want to start trading", or otherwise needs funded PNs **and already has a multisig**;
- the user has just finished `dexdo-deploy-multisig-shellnet` and wants to continue onboarding;
- the user explicitly asks to run `onboard_user_shellnet`, the deposit flow, or PrivateNote setup, and can supply a multisig seed phrase or key pair.

If the user does **not** have a multisig yet → run **`dexdo-deploy-multisig-shellnet`** first.

## Scope

- **Network:** Shellnet only. Mainnet has no public giver and needs a different funding flow — out of scope.
- **Wallet:** a standard `Multisig` with **a single custodian** (`reqConfirms=1`), already Active. Multi-custodian configs require extra `confirmTransaction` calls and are not covered here.
- **What to create:** whatever the `NOTES` work-list says (see [What to create](#what-to-create--the-notes-parameters)) — any count, any currency, any nominal. Step 4 runs the binary once per `NOTES` entry; Step 3 funds the aggregate. The old "always three, one per currency" behaviour is just the default `NOTES=(shell:N100 nackl:N100 usdc:N100)`.

## Out of scope

- Deploying the multisig (→ `dexdo-deploy-multisig-shellnet`).
- Trading itself (covered by the research / trading skills).
- Mainnet onboarding.
- Recovery from a lost seed phrase / lost keypair — there is no recovery, the user MUST have backed it up.

## Setup

The agent walks the user through environment setup **before** any on-chain action. For each item: **check first, install/clone only what's missing**. Setup is idempotent — on a partially-configured machine it simply skips what's already there.

Target layout after setup:

```
<WORKSPACE>/
├── dexdo/        gosh-sh/dexdo @ dev (default branch)
├── multisig/     wallet keypair (INPUT): Multisig.keys.json + Multisig.abi.json + Multisig.tvc (artifacts)
├── notes/        PrivateNotes (SECRET): one subdir per note — <token>-<i>/pn_state.json (resume) + <token>-<i>/<token>.account.json (POST /accounts body for loading into the API)
└── giver/        GiverV3.abi.json — shellnet-only funding artifact (no such folder on mainnet)
```

`multisig/` and `notes/` are the universal layout (same on mainnet); `giver/` is network-specific (shellnet). Secrecy is per-file: recovery-critical are `Multisig.keys.json` and every note's `pn_state.json` (they go into the back-up checklist in Step 5); `*.abi.json`/`*.tvc` are disposable and re-downloadable.

Each note's `pn_state.json` and `<token>.account.json` hold the note's **secret key** (`pnSeckeyHex`), so `onboard_user_shellnet` writes them **mode 0600** (owner-only). If you copy or regenerate them by other means, keep `0600` (`chmod 600 notes/*/*.json`) — never leave a note's key world-readable, and never commit these files.

The dexdo SDK pulls `ackinacki-kit` as a git dependency (tag `v2.1.0`) — cargo clones the kit into its own cache at build time. We do **not** clone the kit locally: the three ABI/TVC artifacts are downloaded straight from `raw.githubusercontent` of that tag (Step 1).

### Setup 0.1 — Working directory

First confirm the base CLI prerequisites are present (`git`, `curl`, `jq` — used throughout setup and funding); the skill installs Rust and `tvm-cli` itself but assumes these three:

```sh
for tool in git curl jq; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing prerequisite: $tool (install it, then re-run)"; exit 1; }
done
```

Default workspace: `~/dexdo-workspace` (override via `$WORKSPACE`). Export the variable and create the folders; lock down `multisig/` and `notes/` (they hold secrets):

```sh
export WORKSPACE="${WORKSPACE:-$HOME/dexdo-workspace}"
mkdir -p "$WORKSPACE/multisig" "$WORKSPACE/notes" "$WORKSPACE/giver"
chmod 700 "$WORKSPACE/multisig" "$WORKSPACE/notes"
```

If you ran `dexdo-deploy-multisig-shellnet` in this same workspace, `multisig/` already holds `Multisig.keys.json` — Step 2 will detect and reuse it.

### Setup 0.2 — Rust toolchain

```sh
cargo --version || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo --version
```

The SDK builds on stable. Nightly is only needed for `cargo fmt` — not required for this skill.

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
# Track the repo's default branch (currently `dev`) — that's where the
# onboard_user_shellnet binary lives. Resolve it dynamically so this keeps
# working if the default is renamed.
DEFAULT_BRANCH=$(git -C "$WORKSPACE/dexdo" remote show origin | sed -nE 's/.*HEAD branch: (.*)/\1/p')
git -C "$WORKSPACE/dexdo" fetch origin "$DEFAULT_BRANCH"
git -C "$WORKSPACE/dexdo" checkout "$DEFAULT_BRANCH"
git -C "$WORKSPACE/dexdo" pull --ff-only origin "$DEFAULT_BRANCH"
```

If the clone/checkout fails — **stop and report**.

### Setup 0.5 — Build `onboard_user_shellnet`

```sh
cd "$WORKSPACE/dexdo/sdk" && cargo build --release --bin onboard_user_shellnet
./target/release/onboard_user_shellnet --help
```

First build — 5–15 minutes on a clean machine (heavy halo2 stack). Subsequent builds are fast.

If the build fails resolving `ackinacki-kit` — check that the kit's `v2.1.0` tag exists on origin (the dexdo SDK pulls it as a git dependency). When the kit ref changes, refresh the lock: `cargo update -p ackinacki-kit`.

After setup the body of the skill operates on:
- `$WORKSPACE/dexdo` — for the repo,
- `$WORKSPACE/multisig` — wallet: `Multisig.keys.json` (resolved in Step 2) + `Multisig.abi.json` + `Multisig.tvc`,
- `$WORKSPACE/notes` — one subdir per note: `<token>-<i>/pn_state.json` (secret),
- `$WORKSPACE/giver` — `GiverV3.abi.json`,
- `$MULTISIG_ADDRESS` — derived in Step 2.

## Autonomy contract — read carefully

The skill writes user-owned secrets to disk and submits real transactions. Run the flow without gating prompts — the user who requested the deposit has thereby already authorized the full set of on-chain actions in the "Overall flow" below. The constraints that remain:

- **Show every command in full** before executing. No silent work — this is about transparency, not consent. **Exception:** when the user pastes a seed phrase or secret key as the input, do NOT echo it back into the chat or into a logged command — write it to the keys file and refer to it by file path thereafter (see Step 2).
- `multisig/Multisig.keys.json` (multisig keypair) and every `notes/<token>-<i>/pn_state.json` (PN keypair + state, one subdir per note) live **only** in `$WORKSPACE/multisig/` and `$WORKSPACE/notes/` (mode 0700) on the user's machine. Don't copy them into the chat log, the paste buffer, or anywhere else.
- If a step fails mid-way — **stop and report**. Don't retry blindly: partial on-chain state may need inspection before the next attempt. This is an error stop, not a consent request.

## Overall flow

```
[1] fetch multisig + giver artifacts from ackinacki-kit (Multisig.abi.json, Multisig.tvc, GiverV3.abi.json)
[2] resolve the INPUT multisig → Multisig.keys.json + re-derived $MULTISIG_ADDRESS; assert it is Active on-chain
[3] giver: fund the Active multisig with the aggregate of NOTES — (flag=1) ECC: deposit currency per note + 100-SHELL gas voucher per note + SHELL reserve; AND (flag=16) native vmshell gas scaled to the note count (the multisig pays submitTransaction in native — skipping this makes a later note silently time out)
[4] cargo run --bin onboard_user_shellnet once per NOTES entry — one PN per note, each in its own notes/<token>-<i>/ subdir: pn_state.json (resume) + <token>.account.json (POST /accounts body for the API)
[5] handoff: summary of every PN created + where each account.json is + back-up checklist
```

## Step 1 — Prepare artifacts

The Multisig (ABI + TVC) and the GiverV3 ABI are vendored in `ackinacki-kit` — download the three files straight from raw of that tag (`v2.1.0`), without cloning the repo. The Multisig ABI+TVC are needed to **re-derive the wallet address** from the input keypair (Step 2); the giver ABI is for funding (Step 3):

```sh
KIT_RAW="https://raw.githubusercontent.com/gosh-sh/ackinacki-kit/v2.1.0"
curl -fL -o "$WORKSPACE/multisig/Multisig.abi.json" "$KIT_RAW/contracts/abi/multisig/Multisig.abi.json"
curl -fL -o "$WORKSPACE/multisig/Multisig.tvc"      "$KIT_RAW/contracts/abi/multisig/Multisig.tvc"
curl -fL -o "$WORKSPACE/giver/GiverV3.abi.json"     "$KIT_RAW/contracts/abi/giver/GiverV3.abi.json"
```

Check that all three files are non-empty.

> **Tag must match the deploy.** The address is a function of the keypair **and** the `Multisig.tvc`. The wallet was deployed with the `v2.1.0` TVC by `dexdo-deploy-multisig-shellnet`; using a different tag here re-derives a *different* (wrong, uninit) address and Step 2's Active check fails.

## Step 2 — Resolve the input multisig

Turn whatever the user supplied (seed phrase **or** key pair) into `$WORKSPACE/multisig/Multisig.keys.json`, then re-derive the address and assert the wallet is Active on-chain.

**Case A — an existing keys file is already present** (e.g. the deploy skill ran in this workspace, or the user copied their `Multisig.keys.json` into `multisig/`). Use it as-is:

```sh
ls -l "$WORKSPACE/multisig/Multisig.keys.json"   # already there → skip to address derivation below
```

> **Case A is the secure path — prefer it whenever possible.** `tvm-cli getkeypair`
> accepts the seed/secret **only** as the `-p` command-line argument (it has no
> stdin/file/env input — verified on 3.0.0), so Cases B and C unavoidably expose the
> secret in that process's argv for the moment it runs, readable via `ps` /
> `/proc/<pid>/cmdline` by other local users. This is a limitation of the external
> tool, not something the skill can encrypt away. Therefore:
>
> - **On any shared / multi-user / untrusted host, do NOT use Case B or C.** Derive
>   `Multisig.keys.json` once on a machine you trust and drop it into `multisig/` as
>   **Case A** — no secret ever touches argv here.
> - Use Case B/C **only** on a single-user machine you control. The commands also
>   read the input **without echoing** it (`read -rs`), but that does not remove the
>   argv exposure above — only Case A does.

**Case B — seed phrase.** Derive the keypair from the 12 words. Read the phrase with
`read -rs` so it is **not echoed** to the terminal, then write straight to the keys
file. Never put the phrase into any other command that gets shown or logged:

```sh
# -s: silent (no echo). The phrase lands only in $SEED, never on screen.
read -rs -p "Paste the 12-word seed phrase: " SEED; echo
umask 077
tvm-cli getkeypair -o "$WORKSPACE/multisig/Multisig.keys.json" -p "$SEED"
unset SEED
chmod 600 "$WORKSPACE/multisig/Multisig.keys.json"
```

**Case C — key pair as a raw 64-hex secret key** (not a file). `getkeypair` also accepts a secret key in place of the phrase; read it silently too:

```sh
read -rs -p "Paste the 64-hex secret key: " SECKEY; echo
umask 077
tvm-cli getkeypair -o "$WORKSPACE/multisig/Multisig.keys.json" -p "$SECKEY"
unset SECKEY
chmod 600 "$WORKSPACE/multisig/Multisig.keys.json"
```

**Re-derive the address** (identical for all three cases) — `genaddr` is deterministic over the keypair + TVC, so it reproduces the address the wallet was deployed at:

```sh
cd "$WORKSPACE/multisig"
GENADDR_OUT=$(tvm-cli genaddr \
  --abi "$WORKSPACE/multisig/Multisig.abi.json" \
  --setkey Multisig.keys.json \
  --save "$WORKSPACE/multisig/Multisig.tvc")
echo "$GENADDR_OUT"
export MULTISIG_ADDRESS=$(echo "$GENADDR_OUT" | sed -nE 's/^Raw address: (0:[a-f0-9]+)$/\1/p')
test -n "$MULTISIG_ADDRESS" || { echo "failed to parse multisig address from genaddr output"; exit 1; }

export MS_ID="${MULTISIG_ADDRESS#0:}"          # bare 64-hex account id (= dapp_id)
export MS_ADDR="${MS_ID}::${MS_ID}"            # address form for `tvm-cli account`
export GIVER_ADDR="0000000000000000000000000000000000000000000000000000000000000000::1111111111111111111111111111111111111111111111111111111111111111"
echo "multisig=$MULTISIG_ADDRESS"
echo "ms_addr=$MS_ADDR"
```

**Assert the wallet is Active.** This is the guard that the input multisig really is deployed — if it isn't, the user gave the wrong wallet or hasn't run the deploy skill:

```sh
tvm-cli -u shellnet.ackinacki.org account "$MS_ADDR" | grep -E 'acc_type|balance|ecc'
```

Expected: `acc_type: Active`. If it shows `Uninit` or `acc_type: -` (not found) — **stop and report**: the keypair/seed is for an undeployed wallet. Direct the user to run `dexdo-deploy-multisig-shellnet` (or supply the correct seed/keypair). Do not attempt to deploy here — that's the other skill's job.

## Step 3 — Fund the multisig (aggregate of the NOTES list)

The multisig is Active. **Two giver flags matter, and you need BOTH:**

- **`flag=1`** credits funds to the recipient's **ECC** balance — the deposit currencies and the SHELL gas vouchers each PrivateNote draws from.
- **`flag=16`** credits SHELL as **native vmshell** — the gas the multisig itself spends on its own `submitTransaction` calls.

> ⚠️ **The classic multi-note trap (this is a real bug if you skip it).** Each note costs the multisig **native vmshell** — it does two `submitTransaction`s (deposit voucher + gas voucher), and `submitTransaction` is paid in native, **not** ECC. Funding only ECC works for one or two notes, then the native balance drains and a later note **fails silently**: the external `submitTransaction` is accepted, but its internal message to `RootPN.generateVoucher` under-funds, no `VoucherGenerated` event is emitted, and the binary dies on a ~480s `"Timed out waiting for VoucherGenerated event"`. **That timeout is depleted native gas — NOT an indexer lag.** So fund native vmshell up front, scaled to the note count.

### 3a — compute funding from the NOTES list

> **Shell note:** these snippets use only indexed arrays + scalars — **no bash associative arrays** (`declare -A` / `${!arr[@]}`), which break under zsh (the macOS default shell). They run as-is in bash *and* zsh.

```sh
# decimals: nackl/shell = 9, usdc = 6.  gas voucher per note = 100 SHELL (ECC id 2).
nackl_raw=0; usdc_raw=0; shell_dep=0; shell_voucher=0
for spec in "${NOTES[@]}"; do
  tok=${spec%%:*}; units=${spec##*:}; units=${units#N}     # nackl:N10000 -> tok=nackl, units=10000
  case "$tok" in
    nackl) nackl_raw=$(( nackl_raw + units * 1000000000 )) ;;
    usdc)  usdc_raw=$((  usdc_raw  + units * 1000000 )) ;;
    shell) shell_dep=$(( shell_dep + units * 1000000000 )) ;;
    *) echo "bad token in NOTES: $tok"; exit 1 ;;
  esac
  shell_voucher=$(( shell_voucher + 100000000000 ))   # +100 SHELL gas voucher per note
done
shell_total=$(( shell_dep + shell_voucher + 300000000000 ))   # + 300 SHELL flat ECC reserve

# Build the ECC JSON for flag=1 — only the non-zero currencies.
ecc=""
[ "$nackl_raw"   -gt 0 ] && ecc="${ecc:+$ecc,}\"1\":$nackl_raw"
[ "$shell_total" -gt 0 ] && ecc="${ecc:+$ecc,}\"2\":$shell_total"
[ "$usdc_raw"    -gt 0 ] && ecc="${ecc:+$ecc,}\"3\":$usdc_raw"
ECC_JSON="{$ecc}"
echo "ecc (flag=1) = $ECC_JSON"

# Native vmshell gas for the flag=16 call. Each note burns a few vmshell across its
# two submitTransactions; provision generously (SHELL is free from the giver on
# shellnet): 10 vmshell/note + 20 base.
NATIVE_GAS=$(( (${#NOTES[@]} * 10 + 20) * 1000000000 ))
echo "native vmshell (flag=16) = $NATIVE_GAS"
```

> Example — `NOTES=(nackl:N10000 nackl:N10000 nackl:N10000)` → ecc `{"1":30000000000000,"2":600000000000}` (30 000 NACKL + 300 SHELL reserve + 3×100 SHELL vouchers) **and** native `50000000000` (50 vmshell).

### 3b — two giver calls: ECC (flag=1) then native vmshell (flag=16)

```sh
fund() {  # $1 = ecc-json, $2 = flag — sends, then blocks until the tx confirms
  local tx
  tx=$(tvm-cli -j -u shellnet.ackinacki.org callx \
    --abi "$WORKSPACE/giver/GiverV3.abi.json" --addr "$GIVER_ADDR" \
    -m sendCurrencyWithFlag \
    '{"dest":"'"$MULTISIG_ADDRESS"'","value":0,"ecc":'"$1"',"flag":'"$2"'}' | jq -r .tx_hash)
  [ -n "$tx" ] && [ "$tx" != "null" ] || { echo "giver callx (flag=$2) returned no tx_hash; stop and report" >&2; return 1; }
  until curl -sX POST https://shellnet.ackinacki.org/graphql -H 'Content-Type: application/json' \
    --data "{\"query\":\"{blockchain{transaction(hash:\\\"$tx\\\"){now aborted}}}\"}" | grep -q '"now"'; do sleep 1; done
  echo "flag=$2 fund tx=$tx confirmed"
}

fund "$ECC_JSON" 1            || exit 1   # deposits + SHELL vouchers -> ECC
fund "{\"2\":$NATIVE_GAS}" 16 || exit 1   # SHELL -> native vmshell gas
```

> The `flag=16` call is the fix for the silent multi-note failure above. If a note ever still dies on the 480s `VoucherGenerated` timeout mid-run, the cure is the same: re-run the `flag=16` top-up (more native vmshell) and resume Step 4 — never assume it's the indexer.

Verify both balances:

```sh
tvm-cli -u shellnet.ackinacki.org account "$MS_ADDR" | grep -E 'acc_type|balance|ecc'
```

Expected: the `ecc:` line contains the `$ECC_JSON` amounts (raw nano), and the native `balance` rose by ~`$NATIVE_GAS` nanovmshell.

## Step 4 — Create the notes (`onboard_user_shellnet`, once per NOTES entry)

The binary creates **one PN per run** — a single `--token-type` + `--nominal`, with its own `--output`. Loop over `NOTES`: each entry gets its own **subdirectory** `notes/<token>-<i>/`. Each run does three idempotent steps (it prints `step 1/3 … 3/3`):

```
step 1/3  deposit voucher (multisig.submitTransaction) + RootPN.deployPrivateNote
step 2/3  SHELL gas voucher (multisig.submitTransaction) + RootPN.sendEccShellToPrivateNote
step 3/3  PrivateNote.getDetails — sanity check
```

The deposit + gas-voucher stages forward correctly because the Step-3 funds were credited as ECC (`flag=1`), not burned into native vmshell.

> **Why a subdirectory per note (do not skip).** The binary writes the API file as `<token>.account.json` **next to `--output`, named by currency only — no index**. Two notes of the *same* currency in the *same* directory would therefore overwrite each other's `account.json`. A dedicated `notes/<token>-<i>/` per note keeps both files isolated and keeps `--output` stable for resume. (PN keys themselves never collide — each fresh `--output` gets a new random key.)

```sh
cd "$WORKSPACE/dexdo/sdk"
# Per-token index counters (no associative arrays — zsh-safe).
i_nackl=0; i_shell=0; i_usdc=0
for spec in "${NOTES[@]}"; do
  tok=${spec%%:*}; nom=${spec##*:}
  case "$tok" in
    nackl) i_nackl=$(( i_nackl + 1 )); idx=$i_nackl ;;
    shell) i_shell=$(( i_shell + 1 )); idx=$i_shell ;;
    usdc)  i_usdc=$((  i_usdc  + 1 )); idx=$i_usdc ;;
  esac
  note_dir="$WORKSPACE/notes/$tok-$idx"
  mkdir -p "$note_dir"
  echo "=== creating note $tok-$idx ($nom) ==="
  cargo run --release --bin onboard_user_shellnet -- \
    --multisig-address "$MULTISIG_ADDRESS" \
    --multisig-keys-file "$WORKSPACE/multisig/Multisig.keys.json" \
    --nominal "$nom" \
    --token-type "$tok" \
    --endpoint shellnet.ackinacki.org \
    --output "$note_dir/pn_state.json"
done
```

After the loop, each `notes/<token>-<i>/` holds **two** files:

- `pn_state.json` — the binary's resume state (PN address, PN keys, DIH in decimal, checkpoints). **Not loaded into the API.**
- `<token>.account.json` — the `POST /api/v1/accounts` body (`pnAddress`/`pnPubkeyHex`/`pnSeckeyHex`/`pnDihHex`, **DIH in hex**). The artifact for loading the PN into the API later. **This skill sends it nowhere** — it only saves it.

E.g. `NOTES=(nackl:N10000 nackl:N10000 nackl:N10000)` → `notes/nackl-1/`, `notes/nackl-2/`, `notes/nackl-3/`, each with `pn_state.json` + `nackl.account.json`.

Notes for the agent:

- The **first** run generates the KZG SRS (~64 MB, ~30 s CPU) under `./params/`; later runs reuse it → faster.
- The halo2 voucher steps are slow (several seconds per stage). Silence in the output is not a hang for at least a few minutes.
- State persists after each successful step. If a run fails — re-run with the **same** `--output`/`--multisig-address`/`--nominal`/`--token-type`/`--endpoint` and it resumes from the first incomplete step. Changing any of them for an existing `--output` is rejected (common case: a stale state file from a previous multisig — `existing state multisig_address … != requested …`; remove/rename that note's subdir and re-run).
- Every `pn_state.json` and `<token>.account.json` holds the PN's **secret key** (`pnSeckeyHex`). Treat them all as user secrets — don't commit, don't paste.

## Step 5 — Handoff

Read every `notes/<token>-<i>/pn_state.json` and show one row per note created:

```sh
# Summary table from the per-note state files.
printf '\nDEXDO deposit complete (Shellnet).\n\nMultisig wallet:   %s\n\n' "$MULTISIG_ADDRESS"
printf '%-12s | %-3s | %-50s | %s\n' "Dir" "Tok" "PrivateNote address" "API file to load"
for d in "$WORKSPACE"/notes/*/; do
  [ -f "$d/pn_state.json" ] || continue
  addr=$(jq -r '.pn_address // .pnAddress // "?"' "$d/pn_state.json")
  acct=$(ls "$d"/*.account.json 2>/dev/null | head -1)
  printf '%-12s | %-3s | %-50s | %s\n' "$(basename "$d")" "${d%/}" "$addr" "$acct"
done
printf '\nKeys file:         %s\n' "$WORKSPACE/multisig/Multisig.keys.json"
```

Render it for the user as a clean table — one line per note: its directory (`<token>-<i>`), PrivateNote address, nominal, and the path to its `<token>.account.json`. Each `account.json` is a ready `POST /api/v1/accounts` body (load into the API later; this skill sends nothing); alongside it sits `pn_state.json` (resume).

**Back-up checklist** (show the user at the end as a reminder; don't gate later skills on these items):

- [ ] `$WORKSPACE/multisig/Multisig.keys.json` (and the seed phrase it came from) copied somewhere safe — wallet loss = funds loss
- [ ] `$WORKSPACE/notes/` in full (every `<token>-<i>/pn_state.json` + `<token>.account.json`) copied somewhere safe — both file types contain PN secret keys

The PN keys (`pnSeckeyHex`) inside each note's `account.json` / `pn_state.json` are what the API/trading skill signs orders with. The `account.json` is what you load into the API service once you get to trading.

## What the user has after the skill

The requested set of funded on-chain PrivateNotes (whatever `NOTES` specified — N notes of one currency, one per currency, or a mix) under user-held keys, ready to:

- place bets on prediction markets;
- buy/sell full sets of outcome tokens;
- place/cancel orders in the outcome books;
- claim payouts after a market resolves.

**Next step to trade via the API:** the notes are deployed but **not yet registered**
with the backend. To get API credentials (and use `dexdo-market-data` signed reads /
`dexdo-trading` REST orders), register each note with the **`dexdo-register-account`**
skill (`POST /api/v1/accounts` → `<tt>.creds.json`). That step is separate on purpose
— it delegates the note's key to the backend. (The on-chain SDK path — `dexdo stake` /
`place-order` — needs no registration; it signs with the note key directly.)

All of this is covered by the **research / trading skills** built on top of this onboarding.
