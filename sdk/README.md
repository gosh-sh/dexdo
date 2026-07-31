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

SDK integration tests are located in `sdk/tests/integration/`. Most exercise on-chain flows and require a live Acki-Nacki network with a giver (e.g., Shellnet); a few — `ledger_race` among them — are fully hermetic and need no network at all (see below).

By default, tests connect to Shellnet (`https://shellnet.ackinacki.org`). To run against a different network, set the `E2E_NETWORK_ENDPOINT` environment variable with a full URL including the scheme:

```sh
# Run tests against a local from-scratch network
E2E_NETWORK_ENDPOINT="http://127.0.0.1:8888" cargo nextest run --manifest-path sdk/Cargo.toml

# Run specific test
E2E_NETWORK_ENDPOINT="http://127.0.0.1:8888" cargo nextest run --manifest-path sdk/Cargo.toml -E 'test(endpoint_)'
```

**Important:** Always provide a full URL with scheme (`http://` or `https://`). A bare host will cause `tvm_client` to attempt plain HTTP on the `/v2/account` endpoint, which may time out.

### Shared account pool (`common::allocator::Allocator`)

Some integration tests rent pre-baked `PrivateNote`s from a shared pool file (`dex_test_notes.keys.json`, a JSON array of `{pn_address, pn_pubkey_hex, pn_seckey_hex, pn_dih_hex}` rows, each optionally carrying a `profile` label) instead of deploying their own. `Allocator::new` locates that file via, in order, `E2E_SEED_NOTES` then `PN_POOL_PATH`; it errors if neither is set.

The pool is shared with the api-e2e suite, and how the two suites divide it depends on how the pool was baked:

- **No note carries a `profile`** — the pool is a run of identical notes (the zerostate generator's `DEX_TEST_NOTES_CNT` path). Every note is interchangeable, so only position can separate the two suites: this suite rents the last `E2E_SDK_TAIL_COUNT` entries (default 3). The api-e2e suite indexes the same file as a whole rather than confining itself to the head, so on this path the two do overlap; what keeps them apart is that the e2e pipeline runs them in sequence, never at once.
- **Notes carry a `profile`** — the pool was baked from a `DEX_TEST_NOTES_SPEC`, where each group of notes has a label, a token type, a balance, and its own ECC seeding. Position then means nothing and `E2E_SDK_TAIL_COUNT` is ignored: this suite rents only the notes whose label it recognises, wherever they sit in the file, and a request for one role never returns a note baked for another. Labels are `PN-DEP`, `PN-TRD`, `PN-CONS`, `PN-CPN`, `PN-SHELL`, `PN-USDC`, `PN-INF`, `PN-ROT`; any other label marks a note this suite will not touch.

A rent that finds no free note of the requested role reports what the pool actually holds, per label, so an exhausted pool and a pool never baked with that role are told apart at the point of failure.

```sh
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json E2E_SDK_TAIL_COUNT=5 \
  cargo nextest run --manifest-path sdk/Cargo.toml -E 'test(allocator)'
```

`PN_POOL_PATH` is also read elsewhere in this same test tree (`common::pn_pool`, `order_book`) for the unrelated `pn_pool.json` raw-pool format — set `E2E_SEED_NOTES` instead of `PN_POOL_PATH` whenever a test run needs both pools at once, so one env var can't be misread as the other's file.

### Stand provenance (`common::preflight`)

`common::preflight::run_preflight` returns an error unless the network under test matches the contract manifest produced when those contracts were compiled: it compares the deployed code hashes, the semantic hash of every ABI compiled into these binaries, and the state of the pre-baked notes against that manifest. It reads two environment variables, neither of which has a default — unset or empty is an error, not a skipped check:

- `E2E_MANIFEST` — path to the contract manifest JSON. TVC paths recorded inside the manifest are resolved relative to that file's own directory, since manifest and images ship together.
- `DEXDO_SHA` — the dodex commit the run is against; it must equal the manifest's own `dodex_sha`, or the manifest belongs to some other build.

The manifest itself is produced by two scripts in [`tests/e2e/`](../tests/e2e/), run against an acki-nacki checkout before `generate_zerostate.py`: `stage_contracts.sh` replaces the zerostate generator's versioned contract directories wholesale with a freshly compiled, allowlisted set of the 13 DEX/airegistry contracts from the current dodex checkout, and `gen_manifest.py` hashes exactly those staged artifacts into `dex_contracts_manifest.json`.

The notes it validates are read from the same seed file the allocator uses (`E2E_SEED_NOTES` / `PN_POOL_PATH`, above).

It asserts how the stand was **generated**, so it has to run before anything touches the chain. A stand that has already served a wave fails it legitimately — deploying any fresh note credits `RootPN._deployedValues` beyond what the seed file accounts for, and api-e2e activity on the pool's head slice moves note balances — and that is not a provenance defect. The failure text says so too.

### Conservation scenario (`proof_money`)

`proof_money::proof_money_lifecycle_local` drives one prediction market through its whole life — deploy, stake, freeze, split, trade, resolve, claim, self-destruct — and asserts exact per-currency conservation after every phase. It is `#[ignore]`d and runs against a from-scratch local stand: it calls `run_preflight` first, so it inherits that check's freshly-generated-zerostate precondition.

It reads everything the two sections above list, plus `E2E_RUN_ID` — the ledger generation the run belongs to. The test panics immediately if it is unset.

**Every attempt needs its own generation.** A panic anywhere in the scenario drops its three leases, and `Drop` quarantines each note rather than returning it to the pool, so the notes this run drew from are used up. Re-running under the same `E2E_RUN_ID` then fails at `rent`, which reports no free note left along with what the pool holds. Changing the variable alone is not enough either: `Allocator::new` only *opens* an existing generation, and an id the ledger does not carry fails with `StaleRun`. Start a new one with the bootstrapper before each attempt:

```sh
cargo run --manifest-path sdk/Cargo.toml --bin ledger-bootstrap -- \
  --dir /path/to/seed/dir --run-id <fresh id> --manifest /path/to/manifest.json
```

Two things make it exclusive of everything else on the stand:

- it holds `b0.lock` (in the seed file's own directory) in **exclusive** mode for its whole duration, so it blocks until every scenario holding that lock in shared mode has finished, and they block while it runs;
- it rents three notes — one `PN-DEP` deployer and two `PN-TRD` traders — which on an unprofiled pool is the whole rentable tail at the default `E2E_SDK_TAIL_COUNT` of 3.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
E2E_MANIFEST=/path/to/manifest.json DEXDO_SHA=<dodex commit> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=proof_money::proof_money_lifecycle_local)'
```

### Multiprocess ledger race (`ledger_race`)

`ledger_race` proves the ledger's cross-process locking under genuine contention: it
spawns four real OS processes (not threads) that re-execute the test binary itself as
workers, each repeatedly renting, releasing, and — every fifth successful cycle —
quarantining notes from a small pool it registers up front. Not `#[ignore]`d and needs
no chain, no seed file, and no environment variables — it runs against a throwaway
temp directory:

```sh
cargo nextest run --manifest-path sdk/Cargo.toml -E 'test(ledger_race)'
```

The same file also has a worker-from-a-stale-generation test, confirming a process left
running from a generation the ledger has since moved past (the same situation a leftover
process from a previous CI run would be in) gets `LedgerError::StaleRun` rather than
corrupting the ledger.

### Parallel market setup (`parallel_setup`)

`parallel_setup_a` and `parallel_setup_b` each deploy a market independently against the
same live stand at the same time, and assert their outcomes never coincide — different
leased note, different nonce, different oracle address, different PMP address. Both are
`#[ignore]`d and must be selected together; run alone, either side blocks on the ledger's
`ready` rendezvous until it times out, since its peer never starts:

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only -E 'test(parallel_setup)'
```

It takes `ChainLockGuard::shared`, not `proof_money`'s exclusive hold — this is the case
shared mode exists for, two chain-mutating scenarios running at once. Because each side
deploys a real market and leaves it live, neither note can be returned clean; both end
the run quarantined under an explicit reason instead.

### USDC release (`usdc_release`)

`usdc_release_local` withdraws a note's whole USDC balance and asserts, exactly, that the
note's `_balance[3]` and physical SHELL pool are gone, that `RootPN._deployedValues[3]` and
RootPN's own `currencies[3]` each fall by the withdrawn amount, and that the destination
gains it. Custody of a third token type is exercised by the stand's own fixture; release is
not exercised by anything else, and it is the half that fails quietly.

The destination is a second leased note. `RootPN.withdrawTokens` transfers without a
`dest_dapp_id`, so the recipient has to live in RootPN's dApp, and a leased note is the one
account no concurrent process may touch — which is what allows an equality instead of a
lower bound.

It needs a pool baked with a `PN-USDC` group, takes `ChainLockGuard::shared`, and runs no
preflight: every assertion is a delta against a baseline read moments earlier, so unlike
`proof_money` it neither needs a pristine stand nor spends one.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=usdc_release::usdc_release_local)'
```

The source note is quarantined afterwards and never rented again: `withdrawTokens` latches
`_hasWithdrawn`, and every DEX operation on a note refuses once it is set.

### Resting and cancelling (`resting_orders`)

`non_crossing_orders_rest_and_cancel_local` puts a seller's ask and a buyer's bid, below it,
on the same book and asserts that neither moves: `OrderBook.Order.amount` is the *remaining*
size, so finding the full amount still there is a direct statement that nothing matched. It
then cancels both and asserts that every reading returns to its pre-placement value — the
buyer's free collateral and escrow, and the seller's outcome tokens.

The two orders belong to different notes on purpose. One note could hold both sides, but then
a self-trade guard rather than the prices could be what keeps them apart, and the scenario
would claim more than it tested.

It brings its market up with `common::market::deploy_ephemeral_market`, and spends the deployer and both trader notes it takes, which is what the pool is sized for. Like `usdc_release` it takes `ChainLockGuard::shared` and runs no
preflight.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=resting_orders::non_crossing_orders_rest_and_cancel_local)'
```

### Orders that must not rest (`market_orders`)

`orders_that_must_not_rest_never_rest_local` walks one market through three phases, each
reaching `OrderBook`'s never-rest branch from a different side: a market buy over a resting ask
(`amount` in quote, remainder returned verbatim), a market sell over a resting bid (`amount` in
base, remainder returned through the collateral conversion), and an IOC order into a book with
nothing on either side. Each asserts the same shape — the sender's owner index empty afterwards
and its `_lockedInOrders` exactly what it was before. Escrow is the half that matters: an order
that rested would hold collateral, one that vanished without refunding would spend it.

The fills are asserted in tokens, never against a re-derivation of the contract's fee
arithmetic. Phase 1 additionally asserts the buyer paid something, since a market order that
matched nothing would satisfy the escrow reading too.

Three phases share one market and one pair of notes deliberately: they test one branch from
three sides, and a scenario each would cost three notes and a pipeline step apiece for no extra
coverage.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \\
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \\
E2E_RUN_ID=<generation> \\
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \\
    -E 'test(=market_orders::orders_that_must_not_rest_never_rest_local)'
```

### Matching priority (`matching_ladder`)

`a_taker_walks_levels_best_first_and_a_level_in_arrival_order_local` rests three asks — two at
0.60 and one at 0.70 — and buys 30 at 0.70, enough to clear one and part of the next. Exactly
one outcome is correct: the first 0.60 ask is gone, the second has 10 left, and the 0.70 ask is
untouched. `Order.amount` is the remaining size, so that middle reading carries both the
partial fill and the ordering — a book serving the newest order in a level first would have
emptied the other one instead.

The three asks come from one note on purpose: the level FIFO holds orders in arrival order
regardless of owner, so one maker exercises it exactly as two would, at half the notes. Each is
confirmed on the book before the next is sent, since arrival order is the property under test.

Not covered here: `buyerRefund` on price improvement (visible only as "spent less than locked",
which is weaker than an equality), conservation across both legs (that is `proof_money`'s
Σ-check), self-matching, and IOC into an empty side.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=matching_ladder::a_taker_walks_levels_best_first_and_a_level_in_arrival_order_local)'
```

### Shutdown with resting orders (`shutdown_orders`)

`a_drain_refunds_resting_orders_and_hands_over_protocol_fees_local` rests an ask and a bid below it — not
crossing, so both survive — and resolves the market. Resolving shuts the book down, and the
drain has to refund what it cancels: the bid's collateral returns to the taker's `_balance`
with `_lockedInOrders` back where it started, the ask's outcome tokens return to the maker's
stake record, and both notes' `_openOrderCount` falls to zero. `proof_money` reaches
`resultStart` with an empty book, so none of that was exercised.

The drain also hands the book's protocol fees to RootPN — the only way they ever get there,
since the book holds them until it dies — and the owner then withdraws them with
`withdrawProtocolFees`, which being owner-only also checks that the stand's RootPN is owned by
the key its zerostate claims. One filled trade precedes the resting pair so that there are fees
at all; without it both fee assertions would hold for a book that never earned anything.

The failure it guards is invisible after the fact: the book is destroyed in the same message
that reports the drain complete, so the orders are gone from every index and the escrow behind
them would simply be missing from notes with no record of having lost it.

`claim` before the drain (`ERR_ORDERBOOK_NOT_SHUTDOWN`) is deliberately **not** asserted. The
guard leaves no trace a caller can read, so "rejected" and "never arrived" are the same
observation, and a check that re-read unchanged balances would pass whether or not the gate
exists. The module says so, and says why the gate matters anyway.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=shutdown_orders::a_drain_refunds_resting_orders_and_hands_over_protocol_fees_local)'
```

### A price above par (`price_above_par`)

`a_trade_above_par_costs_more_than_the_tokens_it_buys_local` trades one outcome token at 1.5
collateral — half again what it can ever redeem for. Prices are basis points against
`FULL_PERCENT = 10000` and the contracts put no upper bound on them: the book checks the tick
multiple and the minimum notional and nothing else.

Every other scenario prices below par, where a buy's collateral cost (`amount * price / 10000`)
is smaller than the token count. Above par that inequality flips, so the test is that the buyer
pays **at least** the notional it offered: a cap at par anywhere in the pricing would show up as
the buyer paying no more than the token count. A bound rather than an equality, because the
exact figure includes the contract's fee and restating that would check the implementation
against itself. The seller's side is asserted the same way, on the account that received it.

It keeps its own suite: if a price above par breaks the book, the damage should not be tangled
up in another scenario's assertions.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=price_above_par::a_trade_above_par_costs_more_than_the_tokens_it_buys_local)'
```

### Bounce recovery (`bounce_recovery`)

`a_bounced_operation_gives_the_money_back_and_unlocks_the_note_local` sends two operations to
counterparties that do not exist — an `initTransfer` to an undeployed note and a `setStake`
against an underived market — and asserts the note gets its collateral back both times. Every
note operation is fire-and-forget: it debits itself, sets `_busy` to the counterparty and
sends, so `onBounce` is the only thing between the owner and a note that is both poorer and
permanently locked.

Each phase carries its own discriminator, because "the balance is unchanged" is equally true of
an operation that was refused before `tvm.accept()` and left no trace. The transfer's is
`_hasTransferred`, latched on acceptance and never cleared; the stake's is the `_stakes` record
a bounced stake leaves behind. The second also proves the first: `setStake` refuses outright
when `_busy` is set, so a record existing at all means the transfer's bounce really did unlock
the note.

Needs no market, no deployer and one note — the cheapest suite on the stand. The note is spent:
`_hasTransferred` is dirty for good by the pool sweep, correctly, since a note that has moved
value out is not interchangeable with a fresh one.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=bounce_recovery::a_bounced_operation_gives_the_money_back_and_unlocks_the_note_local)'
```

### Cancelled event (`cancelled_event`)

`a_cancelled_event_refunds_every_stake_and_closes_the_market_local` covers the other way a market
can end: the oracles decide the event cannot be settled, and every stake has to come back instead
of anyone being paid. Cancelling while the staking window is still open is the easy case and the
live suite already covers it. This one cancels **after the freeze**, when an `OrderBook` exists
and holds collateral, which is where the three gates are: `_isCancelled` alone triggers the
order-book shutdown, `PMP.cancelStake` refuses until the book has finished draining, and
`PrivateNote.cancelStake` refuses again until the note's own open-order counter has come back to
zero — the book reports done when it has finished *sending* the cancels, which is not when the
note has finished receiving them.

The closing claim is made before the closing call, not after: with every other staker refunded,
the market's unclaimed balance must equal exactly what its creator is still owed. Anything above
that leaves as residual to the creator on self-destruct, where a balance check could no longer
tell it apart from the refund itself — and a check made after the account disappears races the
residual transfer, which can still be in flight.

Takes one deployer note and one trader note, and is the one scenario that unwinds everything it
does, so both have a real chance of passing the sweep and returning to the pool. The spec is
sized as though they will not.

```sh
E2E_NETWORK_ENDPOINT=http://127.0.0.1:8888 \
E2E_SEED_NOTES=/path/to/dex_test_notes.keys.json \
E2E_RUN_ID=<generation> \
  cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
    -E 'test(=cancelled_event::a_cancelled_event_refunds_every_stake_and_closes_the_market_local)'
```
