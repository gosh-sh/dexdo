# Private-note pools

The DEX tooling and the e2e suite run against pools of **pre-deployed,
pre-funded PrivateNotes** on a network (shellnet today). Each pool is a
JSON file holding, per note, its on-chain address plus the **plaintext
owner secret key** needed to sign for it.

Because they carry secret keys and are regenerated artifacts (not
source), every pool is **git-ignored** — see `.gitignore`. This file
documents what each one is for and where it lives so a fresh clone knows
what to generate and where to drop it.

## The pools

| File | Purpose | Produced by | Consumed by |
| ---- | ------- | ----------- | ----------- |
| `sdk/pn_pool.json` | General pool of funded PrivateNotes at a fixed nominal. The default `mint_pn_pool` output. | `sdk/src/bin/mint_pn_pool.rs` | source pool for the others / ad-hoc scripts |
| `sdk/pn_pool_deployers.json` | A `pn_pool.json` used as the **market-deployer** pool. Reusing a funded PN as deployer lets `mint_ob_pool` skip minting a fresh halo2 voucher (~3 min/market). | `mint_pn_pool` | `mint_ob_pool --deployer-pn-pool <path>` |
| `sdk/ob_pool.json` | Pool of **pre-warmed OrderBook markets** (addresses + oracle/deployer keys), so a script can grab a live market without paying the ~10–15 min deploy. | `sdk/src/bin/mint_ob_pool.rs` | ad-hoc scripts |
| `tests/fixtures/seed_notes.json` | The **e2e fixture pool**, in the **seed_notes format** (not the pool shape below) — the `.seed_notes.json` sidecar `mint_pn_pool` writes. CI fetches it from S3; each e2e test claims one slot. See `tests/fixtures/README.md`. | `mint_pn_pool` (sidecar) | `services/api/tests` → `TestPnPool::load()` |
| `dex_test_notes.keys.json` (path given by `E2E_SEED_NOTES` / `PN_POOL_PATH`) | The **stand pool**, shared by the `sdk/` e2e harness and the api-e2e suite — also the seed_notes format, so any seed_notes file works. How the two divide it depends on how it was baked (see below). The file's own directory also hosts the shared ledger (`ledger.json`/`ledger.lock`) that tracks each note's lease/quarantine state across concurrent test processes. | supplied out of band, or baked into the stand's zerostate from [`tests/e2e/dex_test_notes.spec.json`](tests/e2e/dex_test_notes.spec.json) | `sdk/tests/integration` → `common::allocator::Allocator`, and `services/api/tests` → `TestPnPool::load()` |

### Dividing the stand pool

Both suites read `dex_test_notes.keys.json` and neither consults the other's
bookkeeping, so the split has to hold by construction. Which rule applies is
decided by the file itself:

- **No row carries a `profile`** — the pool is a run of identical notes (the
  zerostate generator's `DEX_TEST_NOTES_CNT` path). Only position can separate
  the suites: the sdk harness rents the last `E2E_SDK_TAIL_COUNT` entries
  (default 3), while the api-e2e suite indexes the file as a whole. The two
  therefore overlap on this path; the e2e pipeline runs them in sequence, so
  they never hold the same note at the same time.
- **Rows carry a `profile`** — the pool was baked from a
  `DEX_TEST_NOTES_SPEC`, whose groups differ in token type, balance and ECC
  seeding. Position then means nothing and `E2E_SDK_TAIL_COUNT` is ignored:
  each suite takes the rows labelled for it and nothing else. `PN-API` is the
  api-e2e suite's label; `PN-DEP`, `PN-TRD`, `PN-CONS`, `PN-CPN`, `PN-SHELL`,
  `PN-USDC`, `PN-INF` and `PN-ROT` are the sdk harness's roles, which it also
  matches against what a scenario asks for.

A suite that finds no note of its own in a profiled pool fails at load with
the pool's label census, rather than running against notes baked for someone
else. Both suites also pin the shipped spec against what they rent, so a
group dropped or renamed there fails a unit test rather than a stand run.

The e2e pipeline bakes its stand from
[`tests/e2e/dex_test_notes.spec.json`](tests/e2e/dex_test_notes.spec.json).
The group order in that file is not load-bearing — ownership is by label on
both sides — it only decides which `deposit_hash` each note gets.

## Format

The `sdk/*` pools share the shape below (the `*_deployers` name is just an
intent label); their loaders read only `notes[]`, the header fields are
metadata stamped by the generator. The e2e fixture
(`tests/fixtures/seed_notes.json`) is the exception — it uses the api
seeder's seed_notes shape, see
[`docs/seed-private-notes.md`](docs/seed-private-notes.md).

```json
{
  "endpoint": "shellnet.ackinacki.org",
  "nominal": "N10000",
  "token_type": 1,
  "notes": [
    {
      "address": "0:…",
      "deposit_identifier_hash": "…",
      "owner_public_key_hex": "…",
      "owner_secret_key_hex": "…",
      "shell_funded": true,
      "native_funded": true
    }
  ]
}
```

## Generating one

Pass the endpoint with an explicit `https://` scheme — a bare host makes the
tvm_client hit the REST `/v2/account` route over plain `http`, which times out
on shellnet:

```sh
# A pool of funded PNs (see the binary's --help for nominal/count flags):
cargo run --release --bin mint_pn_pool -- \
  --endpoint https://shellnet.ackinacki.org --output sdk/pn_pool.json
```

`mint_pn_pool` also writes a sibling `sdk/pn_pool.seed_notes.json` in the api
seeder / e2e (`seed_notes`) format — no manual conversion. That sidecar is what
the e2e fixture (`tests/fixtures/seed_notes.json`) and the api `seed_accounts`
file consume; see [`docs/seed-private-notes.md`](docs/seed-private-notes.md) for
which slice goes where.

The binaries live in the `sdk` workspace (its own workspace, excluded
from the repo root — it pulls the heavy halo2/zk graph). Run them from
`sdk/`.

## Security

Plaintext `owner_secret_key_hex` is acceptable here **only** because
these are throwaway PNs on a public devnet holding test funds — the
`[SHELLNET-TESTKEYS]` constraint in `tests/fixtures/README.md`. Never
commit a pool, and never reuse this format for a stage/prod network;
real secrets belong in the secret store per `docs/tech-specs/auth.md`.
