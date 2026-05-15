# `tests/fixtures/` — `[SHELLNET-TESTKEYS]`

End-to-end test fixtures consumed by `services/api/tests/e2e_order.rs`.

## Files

- `test_pns.json` — **plaintext `owner_secret_key_hex` for FOUR
  shellnet-only throwaway trading PNs**.
- `ob_pool.json` — paired `OrderBook` / `PMP` pool descriptor for the
  same fixture set.

## `[SHELLNET-TESTKEYS]` — read before reusing

The plaintext seckeys in `test_pns.json` are intentional and safe **only**
because:

- shellnet is a public devnet — the seckeys hold test NACKL only;
- the PNs are not used anywhere except this e2e test;
- anyone with shellnet access can already replicate them.

**Do NOT** repurpose this fixture format for any non-devnet network.
New environments (stage, prod) must keep seckeys in the secret store per
[`docs/tech-specs/auth.md`](../../docs/tech-specs/auth.md), NOT in a
checked-in JSON file. If you grep for `[SHELLNET-TESTKEYS]` and land
here from a PR that ships a stage/prod fixture in the same shape, that
PR is broken — split the secret out before merge.

## Fixture lifetime

`ob_pool.json` is minted with a bounded trading window (~10 hours of
`freeze_unix → result_start_unix`). The test sanity-checks
`freeze_unix < now < result_start_unix` at start and fails fast with an
explicit "fixture expired" message if the window has elapsed.

Refreshed `ob_pool.json` / `test_pns.json` are committed by the project
maintainer in lockstep — test runners do **NOT** regenerate fixtures
themselves.

See the `SECURITY NOTE` block at the top of
[`services/api/tests/e2e_order.rs`](../../services/api/tests/e2e_order.rs)
for the canonical version of these constraints.
