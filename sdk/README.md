# PrivateNote Deploy & Operations Flow

## Overview

```
DEPLOY (2 steps):
  Multifactor → RootPN.generateVoucher(isFee=false) → event
                          ↓
              ZK proof → RootPN.deployPrivateNote → PN deployed with balance
              
FUND GAS (2 steps, before first write operation):
  Multifactor → RootPN.generateVoucher(isFee=true) → event
                          ↓
              ZK proof → RootPN.sendEccShellToPrivateNote → PN has gas
```

## Two sets of keys

- `session_keys` — wallet EPK/ESK registered on multifactor. For `generate_voucher` only.
- `pn_keys` — derived from seed phrase `m/44'/396'/{i}'/0/0`. For all PN operations.

## Deploy PrivateNote (2 steps)

### Step 1: Deposit voucher

Multifactor wallet sends ECC tokens to RootPN. Uses `session_keys`.

```rust
wallet.generate_voucher(ParamsOfGenerateVoucher {
    multifactor_address: mf_address,
    token_type: 1, // NACKL (or 2=SHELL, 3=USDC)
    amount: Nominal::N100.raw_value(TokenType::Nackl), // 100_000_000_000
    is_fee: false,
    signer_keys: session_keys.clone(),
}).await?;
```

Allowed nominals: 100, 1000, 10000 (base × token decimals).
One PN = one token type. For multi-currency, deploy separate PNs.

### Step 2: ZK proof + deploy

Uses `pn_keys` (derived). Persist `dih_dec` immediately after.

```rust
let sk = proof::random_secret_key();
let zk = proof::generate_zk_proof_for_nominal(&sk, TokenType::Nackl, Nominal::N100)?;
let dih_dec = proof::hex_u256_to_dec(&zk.deposit_identifier_hash_hex);

dex.deploy_private_note(ParamsOfDeployPrivateNote {
    zkproof: zk.proof,
    deposit_identifier_hash: dih_dec.clone(),
    ephemeral_pubkey: proof::pubkey_to_dec(&pn_keys.public),
    value: zk.private_note_sum,
    token_type: zk.token_type,
}, Signer::Keys { keys: pn_keys.clone() }).await?;

let pn_address = dex.get_private_note_address(
    ParamsOfGetPrivateNoteAddress { deposit_identifier_hash: dih_dec.clone() }
).await?;
// SAVE dih_dec + pn_address TO DISK
```

PN is deployed. Ready for reads (get_details, get_stakes, history).

## Fund gas (before deploy_pmp)

Gas is Shell ECC[2] needed for internal messages. Fund lazily — only before
`deploy_pmp`. Amount should cover oracle_fee (from event) + operational gas.

### Step A: Gas voucher

Uses `session_keys`. Get oracle_fee from event first.

```rust
let events = dex.get_parsed_events(&event_list_address).await?;
let oracle_fee = events[0].oracle_fee;

wallet.generate_voucher(ParamsOfGenerateVoucher {
    multifactor_address: mf_address,
    token_type: 2, // Shell
    amount: gas_amount, // oracle_fee + buffer for operations
    is_fee: true,
    signer_keys: session_keys.clone(),
}).await?;
```

### Step B: Send gas to PN

Uses `pn_keys`.

```rust
let ecc_zk = proof::generate_zk_proof(
    &proof::random_secret_key(),
    TokenType::DexShellFee as u32,
    gas_amount,
)?;

dex.send_ecc_shell(ParamsOfSendEccShellToPrivateNote {
    proof: ecc_zk.proof,
    nullifier_hash: proof::hex_u256_to_dec(&ecc_zk.nullifier_hash_hex),
    deposit_identifier_hash: dih_dec,
    value: gas_amount,
}, Signer::Keys { keys: pn_keys.clone() }).await?;
```

Repeatable with new ZK proofs (new nullifier each time).

## Error recovery

| Failed at | State | Action |
|-----------|-------|--------|
| Step 1 | ECC on multifactor | Retry step 1 |
| Step 2 | Voucher emitted, no PN | New ZK proof, retry step 2. ECC stays on RootPN pool. |
| Step A-B | PN exists, no gas | Retry steps A-B anytime |

**Key rule: persist `dih_dec` after step 2.**

## Operations (all require gas on PN)

| Operation | API | Notes |
|-----------|-----|-------|
| Create market | `dex.deploy_pmp(pn, params, pn_keys)` | Needs gas. PMP approved after oracle calls `submit_set_timings`. |
| Place stake | `dex.set_stake(pn, params, pn_keys)` | Within stake window (10% of stake period). |
| Claim winnings | `dex.claim(pn, stake_key, pn_keys)` | After PMP resolved. |
| Cancel stake | `dex.cancel_stake(pn, stake_key, pn_keys)` | After PMP cancelled. |
| Transfer | `dex.init_transfer(pn, params, pn_keys)` | PN-to-PN. |
| Withdraw | `dex.withdraw_tokens(pn, params, pn_keys)` | Kills the PN (`has_withdrawn=true`). |
| Change owner | `dex.change_owner(pn, params, pn_keys)` | Old keys stop working. |

## Read operations (no gas needed)

| Operation | API |
|-----------|-----|
| PN details | `dex.get_private_note_details(address)` |
| PN stakes | `dex.get_stakes(address)` |
| PN history | `dex.get_notes_history(&[addresses], limit, cursor)` |
| Browse markets | `dex.discover_active_markets(token_type)` |
| Browse oracles | `dex.discover_oracles()` |
| Oracle events | `dex.get_parsed_events(event_list_address)` |
| PMP details | `dex.get_pmp_details(pmp_address)` |
| Aggregated balance | `dex.get_aggregated_balance(&[dih_list])` |
| Discover my notes | `dex.discover_my_notes(&pubkeys)` |

## Restore from seed

```rust
let mut pubkeys = HashSet::new();
for i in 0..20 {
    let keys = crypto.get_keys_from_mnemonic_with_path(phrase, format!("m/44'/396'/{i}'/0/0"))?;
    pubkeys.insert(proof::pubkey_to_dec(&keys.public));
}
let notes = dex.discover_my_notes(&pubkeys).await?;
// Each: deposit_identifier_hash, note_address, initial_balance, ephemeral_pubkey, token_type
```

## History

```rust
let page = dex.get_notes_history(&[pn_address], 50, None).await?;
let page2 = dex.get_notes_history(&addrs, 50, page.page_info.end_cursor).await?;
```

Event types: PmpDeployed, OwnerChanged, StakeConfirmed, ClaimAccepted, StakeCancelled,
FullSetStakeConfirmed, FullSetStakeCancelled, TransferInitiated, TransferReceived.

## Token types

| Enum | ID | Decimals | Purpose |
|------|----|----------|---------|
| `Nackl` | 1 | 9 | Main token |
| `Shell` | 2 | 9 | Shell token |
| `Usdc` | 3 | 6 | USDC |
| `DexShellFee` | 300 | 9 | Gas for PN (ZK marker; on-chain Shell ECC[2] moves) |

## Test vs Production

**Tests** use giver to fund RootPN directly, bypassing `generate_voucher`.
**Production** requires the full voucher flow via multifactor wallet.

## Running SDK integration tests

SDK integration tests are located in `sdk/tests/integration/` and require a live Acki-Nacki network with a giver (e.g., Shellnet).

By default, tests connect to Shellnet (`https://shellnet.ackinacki.org`). To run against a different network, set the `E2E_NETWORK_ENDPOINT` environment variable with a full URL including the scheme:

```sh
# Run tests against a local from-scratch network
E2E_NETWORK_ENDPOINT="http://127.0.0.1:8888" cargo nextest run --manifest-path sdk/Cargo.toml

# Run specific test
E2E_NETWORK_ENDPOINT="http://127.0.0.1:8888" cargo nextest run --manifest-path sdk/Cargo.toml -E 'test(endpoint_)'
```

**Important:** Always provide a full URL with scheme (`http://` or `https://`). A bare host will cause `tvm_client` to attempt plain HTTP on the `/v2/account` endpoint, which may time out.

### Shared account pool (`common::allocator::Allocator`)

Some integration tests rent pre-baked `PrivateNote`s from a shared pool file (`dex_test_notes.keys.json`, a JSON array of `{pn_address, pn_pubkey_hex, pn_seckey_hex, pn_dih_hex}` rows) instead of deploying their own. `Allocator::new` locates that file via, in order, `E2E_SEED_NOTES` then `PN_POOL_PATH`; it errors if neither is set. Only the last `E2E_SDK_TAIL_COUNT` entries (default 3) of the file are ever rented — the head of the same pool is reserved for the api-e2e suite.

```sh
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json E2E_SDK_TAIL_COUNT=5 \
  cargo nextest run --manifest-path sdk/Cargo.toml -E 'test(allocator)'
```

`PN_POOL_PATH` is also read elsewhere in this same test tree (`common::pn_pool`, `order_book`) for the unrelated `pn_pool.json` raw-pool format — set `E2E_SEED_NOTES` instead of `PN_POOL_PATH` whenever a test run needs both pools at once, so one env var can't be misread as the other's file.

### Stand provenance (`common::preflight`)

`common::preflight::run_preflight` returns an error unless the network under test matches the contract manifest produced when those contracts were compiled: it compares the deployed code hashes, the semantic hash of every ABI compiled into these binaries, and the state of the pre-baked notes against that manifest. It reads two environment variables, neither of which has a default — unset or empty is an error, not a skipped check:

- `E2E_MANIFEST` — path to the contract manifest JSON. TVC paths recorded inside the manifest are resolved relative to that file's own directory, since manifest and images ship together.
- `DEXDO_SHA` — the dodex commit the run is against; it must equal the manifest's own `dodex_sha`, or the manifest belongs to some other build.

The notes it validates are read from the same seed file the allocator uses (`E2E_SEED_NOTES` / `PN_POOL_PATH`, above).

It asserts how the stand was **generated**, so it has to run before anything touches the chain. A stand that has already served a wave fails it legitimately — deploying any fresh note credits `RootPN._deployedValues` beyond what the seed file accounts for, and api-e2e activity on the pool's head slice moves note balances — and that is not a provenance defect. The failure text says so too.

### Conservation scenario (`proof_money`)

`proof_money::proof_money_lifecycle_local` drives one prediction market through its whole life — deploy, stake, freeze, split, trade, resolve, claim, self-destruct — and asserts exact per-currency conservation after every phase. It is `#[ignore]`d and runs against a from-scratch local stand: it calls `run_preflight` first, so it inherits that check's freshly-generated-zerostate precondition.

It reads everything the two sections above list, plus `E2E_RUN_ID` — the ledger generation the run belongs to. The test panics immediately if it is unset.

**Every attempt needs its own generation.** A panic anywhere in the scenario drops its three leases, and `Drop` quarantines each note rather than returning it to the pool, so the tail this run drew from is used up. Re-running under the same `E2E_RUN_ID` then fails at `rent` with "no Free note left in the tail". Changing the variable alone is not enough either: `Allocator::new` only *opens* an existing generation, and an id the ledger does not carry fails with `StaleRun`. Start a new one with the bootstrapper before each attempt:

```sh
cargo run --manifest-path sdk/Cargo.toml --bin ledger-bootstrap -- \
  --dir /path/to/seed/dir --run-id <fresh id> --manifest /path/to/manifest.json
```

Two things make it exclusive of everything else on the stand:

- it holds `b0.lock` (in the seed file's own directory) in **exclusive** mode for its whole duration, so it blocks until every scenario holding that lock in shared mode has finished, and they block while it runs;
- it rents three notes — deployer, buyer, seller — which is the whole rentable tail at the default `E2E_SDK_TAIL_COUNT` of 3.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
E2E_MANIFEST=/path/to/manifest.json DEXDO_SHA=<dodex commit> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=proof_money::proof_money_lifecycle_local)'
```
