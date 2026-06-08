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
| `tests/fixtures/test_pns.json` | The **e2e fixture pool**. A `pn_pool.json` placed at this path; each e2e test claims one slot. See `tests/fixtures/README.md`. | `mint_pn_pool` | `services/api/tests` → `TestPnPool::load()` |

## Format

All four share one shape (the `*_deployers` / `test_pns` names are just
intent labels). The loaders read only `notes[]`; the header fields are
metadata stamped by the generator.

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

```sh
# A pool of funded PNs (see the binary's --help for nominal/count flags):
cargo run --release --bin mint_pn_pool -- --output sdk/pn_pool.json

# For the e2e suite, generate the pool and place it where the loader reads it:
cargo run --release --bin mint_pn_pool -- --output tests/fixtures/test_pns.json
```

The binaries live in the `sdk` workspace (its own workspace, excluded
from the repo root — it pulls the heavy halo2/zk graph). Run them from
`sdk/`.

## Security

Plaintext `owner_secret_key_hex` is acceptable here **only** because
these are throwaway PNs on a public devnet holding test funds — the
`[SHELLNET-TESTKEYS]` constraint in `tests/fixtures/README.md`. Never
commit a pool, and never reuse this format for a stage/prod network;
real secrets belong in the secret store per `docs/tech-specs/auth.md`.
