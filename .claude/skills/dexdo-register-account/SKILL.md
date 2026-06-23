---
name: dexdo-register-account
description: Register a deployed-and-funded PrivateNote with the DEX.DO backend to get an API credential (apiKey + apiSecret) for the REST API. This is the bridge between onboarding (deploy/deposit a note on-chain) and using the API (signed reads in dexdo-market-data, REST orders in dexdo-trading). It is a deliberate security step — registering delegates the note's private key to the backend. Load when the user wants to "register my note / account", "get API keys", "connect my note to the API", or before any signed REST call when no creds.json exists yet.
---

# DEX.DO — Register Account (note → API credential)

Turns a deployed PrivateNote into a usable API account: `POST /api/v1/accounts`
returns an `apiKey` + `apiSecret`, stored as `<tt>.creds.json` (mode 0600). Those
creds are what **`dexdo-market-data`** (signed reads) and **`dexdo-trading`** (REST
orders) sign requests with.

Where it sits in the flow:

```
dexdo-deploy-multisig  →  dexdo-deposit (note deployed + funded on-chain)
                              │   produces notes/<tt>.account.json
                              ▼
                       dexdo-register-account   ← THIS skill (POST /accounts → creds.json)
                              │
                      ┌───────┴────────┐
                      ▼                ▼
              dexdo-market-data   dexdo-trading        →  dexdo-total-withdraw
              (signed reads)      (REST orders)
```

> Registration is its **own** step on purpose — it is a security decision, not a
> silent side-effect of depositing.

## 🚨 Read before registering — what this actually does

Registering **hands the note's private key to the backend** (the body includes
`pnSeckeyHex`); the backend custodies it (sealed under its KEK) and signs your trades
on your behalf. So:

- Only register notes you're willing to let the backend trade with.
- The hosted dev backend (`https://dodex-dev.ackinacki.org`) is **Shellnet testing
  only, with no security guarantees for delegated keys**, and won't exist on Mainnet.
- It is **one-shot per note**: a second registration of the same note returns `-2015`
  (the original credential stays; no new one is minted). Losing an `apiSecret` can't
  be undone by re-registering.

Confirm with the user before registering — this is a deliberate, consequential action.

## Preconditions

- The note must be **deployed and funded on-chain** (the endpoint reads its on-chain
  owner key before minting creds; an undeployed note → `-2013`). Run
  `dexdo-deposit-shellnet` first if it isn't.
- You need the note's `<tt>.account.json` (from onboarding) — it is the exact POST
  body: `pnAddress`, `pnPubkeyHex`, `pnSeckeyHex`, `pnDihHex`.

## Register

Public endpoint (no auth — possession of the note's keys in the body is the
capability). Driven by the shared client:

```sh
DEXDO="python3 $PWD/.claude/skills/dexdo-common/dexdo_client.py"   # from repo root
export WORKSPACE="${WORKSPACE:-$HOME/dexdo-workspace}"

$DEXDO register \
  --account-file "$WORKSPACE/notes/<tt>.account.json" \
  --save-creds   "$WORKSPACE/notes/<tt>.creds.json"
# add --base-url https://your-host  to target a backend other than the dev default
```

On success it prints `{accountId, pnAddress, apiKey, apiSecret, permissions}` and
writes `<tt>.creds.json` **mode 0600** (it holds the `apiSecret`, which the backend
returns **only once**). That file is exactly what the read/trading skills consume via
`--creds` / `$DEXDO_CREDS`.

## Verify

```sh
$DEXDO account --creds "$WORKSPACE/notes/<tt>.creds.json"   # signed read should return balances
```

A `200` with balances confirms the credential works end-to-end (auth + signing).

## Errors (map for the user)

- `-2015` **already registered** — the note has a credential already; don't retry,
  reuse the existing `creds.json`. (Not a failure to fix.)
- `-2013` **note not deployed** on-chain — finish `dexdo-deposit-shellnet` first.
- `-2016` **submitted key does not own the note** — wrong `account.json` for this note.
- `-1130` malformed field (bad hex / pubkey≠seckey-derived). `-1500` transient backend
  read — retry.

## Scope / out of scope

- **In:** register one deployed note → store its API credential (0600). With confirmation.
- **Out:** deploying / funding the note (→ `dexdo-deposit-shellnet`); using the creds
  to read or trade (→ `dexdo-market-data` / `dexdo-trading`). Registration is public
  and unsigned, so it needs no existing credential.
