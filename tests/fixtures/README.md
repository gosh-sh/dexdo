# `tests/fixtures/` — `[SHELLNET-TESTKEYS]`

End-to-end test fixtures consumed by the api crate's `e2e_*.rs`
integration tests.

## Files

- `test_pns.json` — **git-ignored, local-only**: plaintext
  `owner_secret_key_hex` for shellnet-only throwaway trading PNs. Not
  checked in (it carries secret keys); generate it with the
  `mint_pn_pool` binary and drop it here before running the e2e suite.
  See [`PRIVATE_NOTE_POOLS.md`](../../PRIVATE_NOTE_POOLS.md).

## PN slot ownership

Every e2e test that calls `TestPnPool::slot(idx)` claims an exclusive
slot for the duration of its run. Each chain op (`deployPMP`, `setStake`,
`splitFullSet`, `placeOrder`, `cancelOrder`, …) takes the PN's
`_busy` lock, so two tests sharing the same slot would race each other
to `ERR_NOTE_BUSY`. Assignments:

| slot | purpose               |
| ---- | --------------------- |
| 0    | POST `/order`         |
| 1    | DELETE `/order`       |
| 2    | POST `/batchOrders`   |
| 3    | DELETE `/batchOrders` |
| 4    | POST `/buyFullSet`    |

A new e2e test that needs its own deployer-PN takes the next free slot
and adds the row here; top up the pool via `mint_pn_pool` when adding
slots beyond those listed above.

## Run the e2e suite single-threaded

```sh
cargo nextest run -p dodex-api --run-ignored only --test-threads 1
```

Slot ownership keeps the **PN-level** `_busy` lock contention-free
across tests, but the e2e setup in
[`services/api/tests/common/deploy_market.rs`](../../services/api/tests/common/deploy_market.rs)
also talks to **shellnet-global** contracts that every test shares:

- `RootOracle` at `0:1515…` — every test calls `deployOracle` against
  the same singleton.
- The deploy's `OracleEventList` — every test calls `addEvent` against
  it.

Both contracts serialise their own write paths on chain. Running the
e2e binaries in parallel (`--test-threads N` for `N > 1`, or the
nextest default which is "one binary per CPU") leads several
`deployOracle` / `addEvent` external messages to land on the same
target inside one shard-time slot; the loser exits the compute phase
with `exit_code 52` or `101` and the whole deploy unwinds. Failures
look like:

```text
deploy ephemeral market: deploy_oracle: Kit(KitError { tvm_error: …
  exit_code: Number(52), … address: "0:1515151515151515151515151515151515151515151515151515151515151515" })
```

`--test-threads 1` makes the suite run sequentially. Each test spends
~50 s blocked on `stake_end` plus ~30 s round-tripping its write
path; per-test budget itself is fine, only across-tests parallelism
is poisoned by shared chain state.

CI does not run `--ignored` tests, so this constraint only affects
manual runs.

## `[SHELLNET-TESTKEYS]` — read before reusing

The plaintext seckeys in `test_pns.json` are intentional and safe **only**
because:

- shellnet is a public devnet — the seckeys hold test NACKL only;
- the PNs are not used anywhere except these e2e tests;
- anyone with shellnet access can already replicate them.

**Do NOT** repurpose this fixture format for any non-devnet network.
New environments (stage, prod) must keep seckeys in the secret store per
[`docs/tech-specs/auth.md`](../../docs/tech-specs/auth.md), NOT in a
checked-in JSON file. If you grep for `[SHELLNET-TESTKEYS]` and land
here from a PR that ships a stage/prod fixture in the same shape, that
PR is broken — split the secret out before merge.

## OrderBook fixture: per-run deploy, not on-disk

The e2e tests deploy a **fresh** PMP + OrderBook on shellnet at the
start of every run via `services/api/tests/common/deploy_market.rs`
(default lifetime 10 min). Nothing about the market is checked in —
the OrderBook lives for one test run and ages out on its own.

Each deploy spends ~300 NACKL of collateral on the deployer-PN (2 ×
100 NACKL initial stakes + 2 × 0.2 NACKL regular stakes + 100 NACKL
split collateral). The `test_pns.json` pool must hold PNs funded above
that threshold; top up via the in-tree `mint_pn_pool` binary
(`sdk/src/bin/mint_pn_pool.rs`) when balances drop.

See the `SECURITY NOTE` block at the top of
[`services/api/tests/e2e_order.rs`](../../services/api/tests/e2e_order.rs)
for the canonical version of these constraints.
