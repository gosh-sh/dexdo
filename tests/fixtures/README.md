# `tests/fixtures/` — `[SHELLNET-TESTKEYS]`

End-to-end test fixtures consumed by the api crate's `e2e_*.rs`
integration tests.

## Files

- `seed_notes.json` — **git-ignored, not in the repo**: the e2e
  Private-Note pool in the **seed_notes format** — the same shape the api
  seeder reads (`pn_address`, `pn_pubkey_hex`, `pn_seckey_hex`,
  `pn_dih_hex`, plus ignored `tokenType` / `value`). It carries plaintext
  secret keys for shellnet-only throwaway trading PNs, so it is never
  committed. `TestPnPool::load()` reads it from here (override the path
  with the `E2E_SEED_NOTES` env var). Provide it before an e2e run:
  - **CI** fetches it from the `E2E_NOTES_URL` secret — see
    [`.github/workflows/e2e-shellnet.yml`](../../.github/workflows/e2e-shellnet.yml).
  - **Locally**, drop your own copy here. Produce notes with `mint_pn_pool`
    (pool format) and convert them to seed_notes format — see
    [`docs/seed-private-notes.md`](../../docs/seed-private-notes.md) and
    [`PRIVATE_NOTE_POOLS.md`](../../PRIVATE_NOTE_POOLS.md).

## PN slot ownership

Every e2e test that calls `TestPnPool::slot(idx)` claims an exclusive
slot for the duration of its run. Each chain op (`deployPMP`, `setStake`,
`splitFullSet`, `placeOrder`, `cancelOrder`, …) takes the PN's `_busy`
lock, so two tests sharing the same slot would race each other to
`ERR_NOTE_BUSY`. Assignments:

| slot | purpose               |
| ---- | --------------------- |
| 0    | POST `/order`         |
| 1    | DELETE `/order`       |
| 2    | POST `/batchOrders`   |
| 3    | DELETE `/batchOrders` |
| 4    | POST `/buyFullSet`    |

A new e2e test that needs its own deployer-PN takes the next free slot
and adds the row here; grow the pool (and the S3 copy) accordingly.

## Run the e2e suite single-threaded

```sh
cargo nextest run -p dodex-api --run-ignored only --test-threads 1
```

Slot ownership keeps the **PN-level** `_busy` lock contention-free across
tests, but the e2e setup in
[`services/api/tests/common/deploy_market.rs`](../../services/api/tests/common/deploy_market.rs)
also talks to **shellnet-global** singletons that every test shares:

- `RootOracle` at `0:1515…` — every test calls `deployOracle` against the
  same singleton.
- `RootPn` — every test routes `deployPMP` / `get_pmp_address` through it.
- The deploy's `OracleEventList` — every test calls `addEvent` against it.

External messages serialise **per account** on chain. Run the e2e binaries
in parallel (`--test-threads N` for `N > 1`, or the nextest default of one
binary per CPU) and several `deployOracle` / `addEvent` messages land on
the same target inside one shard-time slot; the loser exits the compute
phase with `exit_code 52` or `101` and the whole deploy unwinds:

```text
deploy ephemeral market: deploy_oracle: Kit(KitError { tvm_error: …
  exit_code: Number(52), … address: "0:1515151515151515151515151515151515151515151515151515151515151515" })
```

A distinct PN per slot does **not** fix this — the contention is the shared
root singletons, not the PN. `--test-threads 1` runs the suite
sequentially. Each test spends ~50 s blocked on `stake_end` plus ~30 s
round-tripping its write path; the per-test budget is fine, only
across-tests parallelism is poisoned by shared chain state.

The PR `tests` job skips these (nextest does not run `#[ignore]` tests by
default). They run on their own schedule — nightly and on demand,
single-threaded — via
[`.github/workflows/e2e-shellnet.yml`](../../.github/workflows/e2e-shellnet.yml).

## `[SHELLNET-TESTKEYS]` — read before reusing

The plaintext seckeys in `seed_notes.json` are intentional and safe **only**
because:

- shellnet is a public devnet — the seckeys hold test NACKL only;
- the PNs are not used anywhere except these e2e tests;
- anyone with shellnet access can already replicate them.

**Do NOT** repurpose this for any non-devnet network. Stage/prod keep
seckeys in the secret store per
[`docs/tech-specs/auth.md`](../../docs/tech-specs/auth.md), delivered to the
running container out of band (see
[`docs/seed-private-notes.md`](../../docs/seed-private-notes.md)), never as
a committed JSON file. If you grep for `[SHELLNET-TESTKEYS]` and land here
from a PR that commits a stage/prod notes file, that PR is broken — keep
the secret out of git.

## OrderBook fixture: per-run deploy, not on-disk

The e2e tests deploy a **fresh** PMP + OrderBook on shellnet at the start of
every run via `services/api/tests/common/deploy_market.rs` (default lifetime
10 min). Nothing about the market is checked in — the OrderBook lives for one
test run and ages out on its own.

Each deploy spends ~300 NACKL of collateral on the deployer-PN (2 × 100 NACKL
initial stakes + 2 × 0.2 NACKL regular stakes + 100 NACKL split collateral).
The notes in `seed_notes.json` must hold balances above that threshold; top
them up via `mint_pn_pool` (`sdk/src/bin/mint_pn_pool.rs`) and refresh the S3
copy when balances drop.

See the `SECURITY NOTE` block at the top of
[`services/api/tests/e2e_order.rs`](../../services/api/tests/e2e_order.rs)
for the canonical version of these constraints.
