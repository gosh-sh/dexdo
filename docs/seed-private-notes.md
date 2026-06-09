# Seeding the API's trading accounts

The api seeds a set of trading accounts into Postgres on startup — `pn_address`,
`pn_pubkey`, the KEK-encrypted `pn_seckey`, `pn_dih`, and one derived API key per
account. It reads them from a **JSON notes file** whose path is given in config,
not from anything baked into the binary.

**Seeding only writes DB rows; it creates nothing on-chain.** Each account points
at a Private Note (PN) by address; that PN must already be deployed and funded on
the target network (see [Producing the notes](#producing-the-notes)). Read paths
work without it, but the trading path (`POST /order`, …) fails the moment the api
submits an external message to a PN that does not exist or is out of gas.

## Turning it on

Two `auth` fields in the api config (see
[deployment.md](deployment.md#generate-a-kek)):

```yaml
auth:
  seed_accounts: true                          # off by default
  seed_accounts_path: ./config/seed_notes_list.json # required when seed_accounts is true
```

On startup the api applies migrations, reads the file, and **upserts** every
account + key in one transaction (`ON CONFLICT DO NOTHING` — idempotent; re-runs
count under `*_skipped`, never duplicate or overwrite). A missing or malformed
file aborts startup before the first write — there is no partial state.

## The notes file

A bare JSON array, one object per PN
([`NoteEntry`](../crates/infrastructure/src/seed.rs)):

```json
[
  {
    "pn_address": "0:a554b6d3…",
    "pn_pubkey_hex": "6a00e7c5…",
    "pn_seckey_hex": "de0a6632…",
    "pn_dih_hex": "8416176d…",
    "tokenType": 1,
    "value": 10000000000000
  }
]
```

| field | source | note |
| --- | --- | --- |
| `pn_address` | deployed PN | `0:…`, copy as-is |
| `pn_pubkey_hex` | PN owner public key | hex; stored as `numeric(78,0)` |
| `pn_seckey_hex` | PN owner secret key | hex; KEK-encrypted at rest |
| `pn_dih_hex` | deposit-identifier hash | hex |
| `tokenType`, `value` | — | **ignored** — they describe what the note holds, not its identity |

**It carries secret keys — never commit it.** `pn_pubkey_hex` / `pn_dih_hex` must
fit 256 bits and `pn_seckey_hex` must be valid hex, or startup fails loudly.

## API credentials are derived, not in the file

The file holds no `api_key` / `api_secret`. For the note at array index `i` the
seeder mints:

- `api_key` = `dk_live_test_{i+1:03}` (e.g. `dk_live_test_001`)
- `api_secret` = `HMAC-SHA256(KEK, "dodex/api-secret/v1" || u32_be(i))`
- `permissions` = `[USER_DATA, TRADE]`; `label` = `test-mm-{i+1:03}`

**Why:** the secret never has to be stored anywhere. The same `(KEK, index)`
always yields the same 32 bytes, and a different environment's KEK yields a
disjoint secret. To hand a client its secret, derive it from that environment's
KEK and the note's position — see
[`crypto::derive_api_secret`](../crates/infrastructure/src/crypto.rs).

## How the file reaches the container (docker-compose / staging)

The api and indexer containers mount the host's `./config` read-only at
`/app/config` ([docker-compose.yml](../docker-compose.yml)), and
[.dockerignore](../.dockerignore) excludes `config/seed_notes*.json` from the
image build. So the secret file is **never baked into the image** — it is
delivered only at runtime through the mount.

On the target host:

1. Drop the notes file at `./config/seed_notes_list.json` (from your secret
   store / S3; it is not in the repo).
2. The env config (`config/api.stage.supabase.yaml`, assembled by CI) sets
   `auth.seed_accounts: true` and
   `auth.seed_accounts_path: ./config/seed_notes_list.json`. The path is
   relative to the container workdir `/app`, so it resolves to the mounted
   `/app/config/seed_notes_list.json` (and to the repo's `config/` for a local
   `cargo run`).
3. `docker compose -f docker-compose.yml -f docker-compose.stage.yml up` — the
   api reads both files and seeds on first start. Prod is identical with the prod
   config.

## Producing the notes

[`mint_pn_pool`](../sdk/src/bin/mint_pn_pool.rs) deploys and funds a pool of PNs
on a network that has a giver (shellnet does; mainnet does not):

```sh
cargo run --release --bin mint_pn_pool -- \
  --count 10 --nominal N10000 --token-type nackl \
  --endpoint shellnet.ackinacki.org --output pn_pool.json
```

It writes the **pool format** (`address`, `deposit_identifier_hash` as a decimal
string, `owner_public_key_hex`, `owner_secret_key_hex`, funding flags). Convert
that to the notes-file format above: rename the fields and turn
`deposit_identifier_hash` from **decimal to hex** for `pn_dih_hex`. The halo2
prover prerequisites (SRS, prover cache, release build) live in the binary's
module doc.

## Tests

- **`cargo test` (chain mocked)** seeds through the same code path from the
  committed dummy fixture
  [`services/api/tests/fixtures/seed_notes_dummy.json`](../services/api/tests/fixtures/seed_notes_dummy.json)
  — fake keys, safe to commit, because the chain is faked and `pn_seckey` is
  never used to sign.
- **e2e and staging** use real, funded notes kept out of the repo.

## Funding a Private Note with the giver

On a network with a giver (such as shellnet) the giver tops up a PN directly by
its `pn_address`. `mint_pn_pool` does this at deploy time, but an already-deployed
PN can be topped up later by sending SHELL (and native gas) from the giver to its
address — a PN out of gas stops executing trading messages until refunded.

For test SHELL and giver usage on shellnet:
<https://dev.ackinacki.com/readme/get-test-tokens-in-shellnet#get-shell>
