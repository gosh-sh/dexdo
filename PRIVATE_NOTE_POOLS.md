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
  each suite takes the rows labelled for it and nothing else. `PN-API` and
  `PN-INF` are the api-e2e suite's labels — the first for the trader path, the
  second for the inference binaries, which need SHELL rather than NACKL because
  an inference buy is paid out of `_balance[CURRENCIES_ID_SHELL]`. `PN-DEP`,
  `PN-TRD`, `PN-CONS`, `PN-CPN`, `PN-SHELL`, `PN-USDC` and `PN-ROT` are the sdk
  harness's roles, which it also matches against what a scenario asks for.

  **`PN-INF` was for a while claimed by both sides, and is not any more.** The
  sdk harness listed `PnProfile::Inf → "PN-INF"` in its own `ALL`, so its
  allocator read those rows as leasable, while the api-e2e inference binaries
  index the same rows by fixed position (`TestPnPool::load_inference()`).
  Nothing ever leased one — no scenario asks for that role — so the overlap was
  latent, and it was invisible before wave 5 because the stand spec declared no
  `PN-INF` group at all and the rows did not exist. Creating the group is what
  brought the two claims together.
  
  The role was removed from `PnProfile` on 2026-08-24, which is the whole fix:
  ownership is decided by `from_seed_label` returning `Some`
  (`sdk_owned_indices`), so a label the enum does not name is a label the
  harness cannot lease. This is the mechanism the type was built with — its
  doc comment already said a `None` here means "a note reserved for another
  suite" — and `PN-INF` simply had not been routed through it. Two unit tests
  pin the result: `an_unowned_label_is_not_a_profile_of_this_harness` asserts
  the label reads as foreign, and `every_label_in_the_stand_spec_belongs_to_a_suite`
  now names both api-e2e labels so a spec group owned by nobody still fails.
  
  The direction was not arbitrary. The inference surface lives entirely in
  `dodex_chain::test_helpers` and `dodex_sdk` carries none of it, so an sdk
  inference scenario cannot be written at all — the role named rows for a
  scenario that cannot exist. Renaming the api suite's label instead would have
  touched the spec, the shellnet workflow, both guards and the loader, to the
  same end.

**`PN-CONS`, `PN-CPN` and `PN-ROT` are seeded but not yet rented.** The sdk
harness has named those roles since the allocator was written; the stand spec
did not declare them until 2026-08-24, so the three were labels with no rows —
the mirror of the `PN-INF` problem above and the louder half of it: `rent`
answers a missing group with the pool's census, not with a note somebody else
owns. They are seeded now so a scenario that wants one finds it rather than
discovering the gap on the stand. The stand confirms it: pipeline #299
(2026-08-24) baked all nine groups at their declared counts (`wrote 125 note
keys`) and `sdk_proof` passed, so the generator credited `RootPN._deployedValues`
for the added rows — a group the stand bakes but the root is not credited for
fails the next `withdrawTokens` in the run, not the test that rented it.

`PN-CPN` is the only group whose `value` carries meaning. `generateCoupon`
(`PrivateNote.sol:2015-2017`) walks the whole `_balance` map and requires every
entry below `minStakeValue(tt)` — 10 000 000 for NACKL and SHELL, 10 000 for
USDC (`modifiers.sol:161-167`) — so a coupon note has to be seeded BELOW that
line, and 9 000 000 is that. Every other group carries the pool's usual
1 000 000 000 000, which is a balance no coupon could ever be minted against.

**The SHELL and USDC coupon notes the matrix asks for are NOT here, and cannot
be under this name.** The generator refuses a duplicate `profile`
(`generate_zerostate.py`: `assert profile not in seen_profiles`), and a group
carries exactly one `tokenType`, so `PN-CPN` can express coupons in one
currency only. The matrix's `7 = NACKL×5 + SHELL×1 + USDC×1` therefore needs a
naming decision first — per-currency labels, and matching roles in
`PnProfile` — which is a change to the vocabulary rather than to the spec.

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
