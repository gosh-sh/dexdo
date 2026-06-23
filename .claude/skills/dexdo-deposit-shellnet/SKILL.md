---
name: dexdo-deposit-shellnet
description: Deposit onto DEXDO on Acki Nacki Shellnet by creating PrivateNotes (one per currency — SHELL / NACKL / USDC) from an ALREADY-DEPLOYED multisig. The multisig is an INPUT, supplied either as a 12-word seed phrase or as a key pair (keys.json file or 64-hex secret key); the skill re-derives the address, funds the wallet with the deposit currencies from the public giver, then deploys and funds the PrivateNotes. Load when the user wants to deposit, create PrivateNotes, fund a note, or trade — and already has a multisig (if they don't, run `dexdo-deploy-multisig-shellnet` first).
---

# DEXDO — Deposit / Create PrivateNotes (Shellnet)

Skill **#0b** in the series. It takes an **already-deployed multisig as input** and produces **a list of PrivateNotes** (one per currency: SHELL / NACKL / USDC), saved to files, usable two ways:

- **directly with the libraries** (`dodex-sdk` / `ackinacki-kit`) — sign on-chain operations with each note's key from its file;
- **loaded into the API service** — each `notes/<tt>.account.json` is a ready-to-send `POST /api/v1/accounts` body.

After the run the user has:

- three deployed PrivateNotes (PN) — one per currency (SHELL / NACKL / USDC), each with its own deposit;
- each PN topped up with vmshell (gas) for trading operations;
- the multisig holding a SHELL reserve for future trading calls.

## Input — the multisig (required)

This skill does **not** deploy a multisig — it consumes one. The deploy step lives in **`dexdo-deploy-multisig-shellnet`**; if the user has no multisig yet, run that skill first, then this one.

The multisig is supplied **one of two ways** (the user picks whichever they have):

1. **Seed phrase** — the 12 words from `Multisig.seed` (or wherever the user backed them up).
2. **Key pair** — either an existing `Multisig.keys.json` file, or a raw 64-hex secret key.

Both reduce to the same thing: a `Multisig.keys.json` keypair, from which the skill re-derives the on-chain address with `genaddr`. The seed phrase and the key pair are equivalent — either uniquely controls the wallet. Resolving the input is **Step 2**.

> The address is **not** asked for separately — it is deterministically recomputed from the keypair + the `Multisig.tvc` (`v2.1.0`). This is why both skills must use the same artifact tag.

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
- **Currency:** a single run creates **three PNs** — one each for SHELL / NACKL / USDC (Step 4 runs the binary three times). Don't need a currency — drop it from the loop in Step 4 and from the funding in Step 3.

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
├── notes/        PrivateNotes (SECRET): pn_state.<tt>.json (resume) + <tt>.account.json (POST /accounts body for loading into the API)
└── giver/        GiverV3.abi.json — shellnet-only funding artifact (no such folder on mainnet)
```

`multisig/` and `notes/` are the universal layout (same on mainnet); `giver/` is network-specific (shellnet). Secrecy is per-file: recovery-critical are `Multisig.keys.json` and all `pn_state.*.json` (they go into the back-up checklist in Step 5); `*.abi.json`/`*.tvc` are disposable and re-downloadable.

`pn_state.<tt>.json` and `<tt>.account.json` hold the note's **secret key** (`pnSeckeyHex`), so `onboard_user_shellnet` writes them **mode 0600** (owner-only). If you copy or regenerate them by other means, keep `0600` (`chmod 600 notes/*.json`) — never leave a note's key world-readable, and never commit these files.

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
- `$WORKSPACE/notes` — `pn_state.{shell,nackl,usdc}.json` (secret),
- `$WORKSPACE/giver` — `GiverV3.abi.json`,
- `$MULTISIG_ADDRESS` — derived in Step 2.

## Autonomy contract — read carefully

The skill writes user-owned secrets to disk and submits real transactions. Run the flow without gating prompts — the user who requested the deposit has thereby already authorized the full set of on-chain actions in the "Overall flow" below. The constraints that remain:

- **Show every command in full** before executing. No silent work — this is about transparency, not consent. **Exception:** when the user pastes a seed phrase or secret key as the input, do NOT echo it back into the chat or into a logged command — write it to the keys file and refer to it by file path thereafter (see Step 2).
- `multisig/Multisig.keys.json` (multisig keypair) and all `notes/pn_state.<tt>.json` (PN keypair + state, one file per currency) live **only** in `$WORKSPACE/multisig/` and `$WORKSPACE/notes/` (mode 0700) on the user's machine. Don't copy them into the chat log, the paste buffer, or anywhere else.
- If a step fails mid-way — **stop and report**. Don't retry blindly: partial on-chain state may need inspection before the next attempt. This is an error stop, not a consent request.

## Overall flow

```
[1] fetch multisig + giver artifacts from ackinacki-kit (Multisig.abi.json, Multisig.tvc, GiverV3.abi.json)
[2] resolve the INPUT multisig → Multisig.keys.json + re-derived $MULTISIG_ADDRESS; assert it is Active on-chain
[3] giver: 1000 SHELL + 100 NACKL + 100 USDC to the Active multisig (flag=1) — deposit + gas vouchers for 3 PNs
[4] cargo run --bin onboard_user_shellnet × 3 (shell, nackl, usdc) — three PNs; per PN: pn_state.<tt>.json (resume) + <tt>.account.json (POST /accounts body for the API)
[5] handoff: summary of the three PNs + where the <tt>.account.json files are + back-up checklist
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

## Step 3 — Fund the multisig (all three deposit currencies)

The multisig is Active — use `flag=1`. Fund **all three ECC currencies** in one giver call: SHELL (currency 2), NACKL (currency 1), USDC (currency 3). With `flag=1` to an Active recipient they all land correctly in the `ecc` balance (verified empirically — with `flag=16` SHELL would go into native vmshell, which we don't want here).

Per-currency breakdown — sized for 3 PNs (one per token) with deposit nominal N100 + a 100-SHELL gas voucher per PN:

| Currency | id | decimals | Amount funded | Why |
|---|---|---|---|---|
| SHELL | 2 | 9 | **1000 SHELL** (`1_000_000_000_000` nano) | 100 for its own deposit voucher + 3×100 for the gas vouchers of all PNs + ~600 reserve for trading `submitTransaction`s |
| NACKL | 1 | 9 | **100 NACKL** (`100_000_000_000` nano) | N100 deposit voucher for the NACKL PN |
| USDC  | 3 | 6 | **100 USDC** (`100_000_000` nano) | N100 deposit voucher for the USDC PN |

If you want larger nominals (N1000 / N10000) — scale: each PN at the desired nominal is 10/100× more. SHELL = 100 (own deposit, if there's a SHELL PN at the same nominal) + 100×N (gas voucher) + reserve.

> The wallet may already hold a SHELL reserve from the deploy skill's pre-deploy funding (it sat in native vmshell, not ecc). This Step adds the **ecc** balances the PN vouchers actually draw from, so run it even if the native balance looks healthy.

Send it all in one giver call:

```sh
FUND_TX=$(tvm-cli -j -u shellnet.ackinacki.org callx \
  --abi "$WORKSPACE/giver/GiverV3.abi.json" \
  --addr "$GIVER_ADDR" \
  -m sendCurrencyWithFlag \
  '{"dest":"'"$MULTISIG_ADDRESS"'","value":0,"ecc":{"1":100000000000,"2":1000000000000,"3":100000000},"flag":1}' \
  | jq -r .tx_hash)
echo "fund_tx=$FUND_TX"
# Guard: a failed callx yields empty/"null" — polling that would spin forever.
test -n "$FUND_TX" && [ "$FUND_TX" != "null" ] || { echo "giver callx returned no tx_hash — inspect the callx output above; stop and report"; exit 1; }
```

Confirm by polling the hash:

```sh
until curl -sX POST https://shellnet.ackinacki.org/graphql -H 'Content-Type: application/json' \
  --data "{\"query\":\"{blockchain{transaction(hash:\\\"$FUND_TX\\\"){now aborted}}}\"}" \
  | grep -q '"now"'; do sleep 1; done
echo "deposit fund confirmed"
```

Verify all three currencies arrived:

```sh
tvm-cli -u shellnet.ackinacki.org account "$MS_ADDR" | grep -E 'acc_type|balance|ecc'
```

Expected: the `ecc:` line contains `{"1":"100000000000","2":"1000000000000","3":"100000000"}` (raw nominals). The native `balance` is whatever the wallet already held.

## Step 4 — Run `onboard_user_shellnet` (three times, one PN per currency)

The binary takes a single `--token-type` per invocation and creates one PN. To get three PNs — run it three times, each with a different `--token-type` and its own `--output` (a separate state file so they don't overwrite each other). Each run goes through **three steps** (the binary prints `step 1/3 … 3/3`), state is idempotent:

```
step 1/3  deposit voucher (via multisig.submitTransaction) + RootPN.deployPrivateNote
step 2/3  SHELL gas voucher (via multisig.submitTransaction) + RootPN.sendEccShellToPrivateNote
step 3/3  PrivateNote.getDetails — sanity check
```

Both multisig stages (in steps 1/3 and 2/3) go through `multisig.submitTransaction`: after Step 3 the multisig holds ECC SHELL (currency 2) in `ecc[2]`, plus the needed deposit token in `ecc[1|3]`. The ECC currencies forward through correctly because incoming funds with `flag=1` are credited as ECC, not burned into native.

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
- If a PN isn't needed for some currency — just drop it from `for TT in ...` and don't fund the corresponding entry in Step 3.

## Step 5 — Handoff

Read the three `pn_state.<tt>.json` files and show a summary table:

```
DEXDO deposit complete (Shellnet).

Multisig wallet:   <multisig_address>

Token       | PrivateNote address              | Nominal | API file to load
------------|----------------------------------|---------|--------------------------
SHELL (2)   | <pn_address from shell state>    | N100    | $WORKSPACE/notes/shell.account.json
NACKL (1)   | <pn_address from nackl state>    | N100    | $WORKSPACE/notes/nackl.account.json
USDC (3)    | <pn_address from usdc state>     | N100    | $WORKSPACE/notes/usdc.account.json

Each <tt>.account.json is a ready POST /api/v1/accounts body (load into the API
later; this skill sends nothing). Alongside it sits pn_state.<tt>.json (resume).

Keys file:         $WORKSPACE/multisig/Multisig.keys.json
```

**Back-up checklist** (show the user at the end as a reminder; don't gate later skills on these items):

- [ ] `$WORKSPACE/multisig/Multisig.keys.json` (and the seed phrase it came from) copied somewhere safe — wallet loss = funds loss
- [ ] `$WORKSPACE/notes/` in full (`pn_state.*.json` + `*.account.json`) copied somewhere safe — both file types contain PN secret keys

The PN keys (`pnSeckeyHex`) inside `<tt>.account.json` / `pn_state.<tt>.json` are what the API/trading skill signs orders with. `<tt>.account.json` is what you load into the API service once you get to trading.

## What the user has after the skill

Three funded on-chain PrivateNotes (one per currency: SHELL / NACKL / USDC) under user-held keys, ready to:

- place bets on prediction markets;
- buy/sell full sets of outcome tokens;
- place/cancel orders in the outcome books;
- claim payouts after a market resolves.

All of this is covered by the **research / trading skills** built on top of this onboarding.
