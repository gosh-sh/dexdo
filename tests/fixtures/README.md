# `tests/fixtures/` — `[SHELLNET-TESTKEYS]`

End-to-end test fixtures consumed by `services/api/tests/e2e_order.rs`
and `services/api/tests/e2e_cancel_order.rs`.

## Files

- `test_pns.json` — **plaintext `owner_secret_key_hex` for FOUR
  shellnet-only throwaway trading PNs**.

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
that threshold; top up via `bee-engine-private/bee_dex`'s `mint_pn_pool`
when balances drop.

See the `SECURITY NOTE` block at the top of
[`services/api/tests/e2e_order.rs`](../../services/api/tests/e2e_order.rs)
for the canonical version of these constraints.
