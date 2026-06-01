# Deploying the seed PrivateNotes

The api seed (`auth.seed_accounts: true`, see
[deployment.md](deployment.md#generate-a-kek)) inserts a fixed set of test
accounts into Postgres — `pn_address`, `pn_pubkey`, the KEK-encrypted
`pn_seckey`, and `pn_dih`, from the `SEED_DATA` literal in
[`crates/infrastructure/src/seed.rs`](../crates/infrastructure/src/seed.rs).

**Seeding only writes database rows. It does not create anything on-chain.**
Each seeded account points at a PrivateNote (PN) contract by address; that
contract must already be deployed and funded on the target network. If it is
not, the read paths still work (the rows exist), but the trading path
(`POST /order`, `DELETE /order`, batch, `buyFullSet`) fails the moment the api
submits an external message to a note that does not exist or has no gas.

So the order of operations is:

1. Deploy and fund the PrivateNotes on-chain (this document).
2. Record each note's address, keypair, and deposit-identifier hash.
3. Put those values into `SEED_DATA` (or provision the accounts directly in the
   database — see [auth.md](tech-specs/auth.md)).
4. Run the api once with `auth.seed_accounts: true` to insert the rows, then
   turn the flag back off.

## What makes a PrivateNote usable

A note has to clear three on-chain steps before the trading path can use it:

1. **Deploy** — `RootPN.deployPrivateNote`. The note is materialized at a
   deterministic address derived from its **deposit identifier hash (DIH)**.
2. **SHELL ECC gas** — `RootPN.sendEccShellToPrivateNote` funds the note's gas
   balance in SHELL.
3. **Native top-up** — a native vmshell balance so the note can pay for its own
   internal-message execution. This comes from the **giver** (see below).

## Deploying a pool of notes: `mint_pn_pool`

The in-repo CLI
[`sdk/src/bin/mint_pn_pool.rs`](../sdk/src/bin/mint_pn_pool.rs) runs the full
per-note flow — halo2 deposit voucher → `deployPrivateNote` → halo2 SHELL
voucher → `sendEccShellToPrivateNote` → giver native top-up — and writes the
result to JSON:

```sh
cargo run --release --bin mint_pn_pool -- \
  --count 10 \
  --nominal N10000 \
  --token-type nackl \
  --endpoint shellnet.ackinacki.org \
  --output pn_pool.json
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--count` / `-n` | `5` | number of PrivateNotes to deploy |
| `--nominal` | `N10000` | per-note deposit nominal — `N100`, `N1000`, or `N10000` |
| `--token-type` / `-t` | `nackl` | deposit currency — `nackl`, `shell`, or `usdc` |
| `--endpoint` / `-e` | `shellnet.ackinacki.org` | network host |
| `--output` / `-o` | `pn_pool.json` | output path; re-running against an existing file **appends** `--count` more notes |

Deployment is sequential by design — each halo2 voucher proof commits to current
chain state and must be submitted inside its validity window, so notes cannot be
minted in parallel.

### Prerequisites

`mint_pn_pool` is a maintainer tool with real build- and run-time dependencies:

- **Private halo2 prover crates**, pulled over SSH in
  [`sdk/Cargo.toml`](../sdk/Cargo.toml). Without access to them the binary will
  not compile.
- **Halo2 artifacts on disk** — a writable prover-cache directory, plus the KZG
  SRS file `kzg_bn254_19.srs` (~64 MB). The SRS is generated automatically on
  first run if it is missing — it is reproducible from a fixed seed, so the
  generated file is identical everywhere and matches the on-chain verifier — and
  cached on disk afterward (the first run pays a one-time CPU cost). Defaults are
  `./params`, `./params/halo2_cache`, `./target/halo2_fixtures`; override with
  `PARAMS_DIR`, `HALO2_PK_CACHE`, `HALO2_FIXTURE_DIR`. See
  [`sdk/src/services/halo2/paths.rs`](../sdk/src/services/halo2/paths.rs).
- **A working giver** on the target network. Shellnet has one; a production
  network typically does not — there you deploy and fund notes by other means.
- A **release build** — the halo2 prover is CPU-bound.

If you cannot run this tool (no SSH access, no giver on your network), deploy and
fund the notes by whatever means your network provides, then record the same
four values per note (address, DIH, public key, secret key) by hand.

### Output: `pn_pool.json`

`pn_pool.json` holds one entry per note. **It contains secret keys — keep it
private and out of any shared store.** Each entry carries `address`,
`deposit_identifier_hash`, `owner_public_key_hex`, `owner_secret_key_hex`, and
the funding flags `shell_funded` / `native_funded`. The next section turns these
into seeder input.

## Putting the notes into the seeder

The seeder reads one JSON literal, `SEED_DATA`, compiled into the api binary
([`crates/infrastructure/src/seed.rs`](../crates/infrastructure/src/seed.rs)). To
seed your own notes you edit that literal and rebuild the api image. One account
looks like this:

```json
{
  "accounts": [
    {
      "label": "mm-001",
      "pn_address": "0:20e8f9…",
      "pn_pubkey_dec": "70969641…",
      "pn_seckey_hex": "483ee42a…",
      "pn_dih_dec": "41285154…",
      "api_keys": [
        {
          "api_key": "dk_live_001",
          "api_secret_hex": "1de6fc5c…",
          "permissions": ["USER_DATA", "TRADE"]
        }
      ]
    }
  ]
}
```

### Fields from the deployed note

Four fields come straight from `pn_pool.json` (or from however you deployed the
note):

| `pn_pool.json` | `SEED_DATA` | Conversion |
| --- | --- | --- |
| `address` | `pn_address` | none — copy as-is (`0:…`) |
| `deposit_identifier_hash` | `pn_dih_dec` | none — already decimal |
| `owner_secret_key_hex` | `pn_seckey_hex` | none — already hex |
| `owner_public_key_hex` | `pn_pubkey_dec` | hex → decimal (see below) |

The public key is the one field that needs converting — the pool writes it as
hex, but the seeder's `accounts.pn_pubkey` column is `numeric(78,0)`, so it wants
the decimal form:

```sh
python3 -c "print(int('<owner_public_key_hex>', 16))"
```

`label` is free-form and optional — it is only there for humans reading the DB.

### The API credentials

`api_key` and `api_secret_hex` are not produced by the deploy tool — you mint
them yourself, one or more per account:

- **`api_key`** — the public identifier the client sends in the `X-DODEX-APIKEY`
  header. Any unique string; the baked-in examples use a `dk_live_…` prefix.
  Uniqueness is enforced per active key on insert.
- **`api_secret_hex`** — the shared secret the client signs requests with
  (HMAC-SHA256). Generate 32 bytes of hex:
  ```sh
  openssl rand -hex 32
  ```
  It is stored **encrypted under the KEK** — this literal is the only cleartext
  copy, so hand it to the client and keep it safe.
- **`permissions`** — a non-empty list; each entry is exactly `USER_DATA` (read
  account and order data) or `TRADE` (place and cancel orders). Case-sensitive.

### What the seeder rejects at startup

`SEED_DATA` is validated in full before a single row is written — one bad field
makes the api refuse to start (loud on purpose; there is no partial DB state).
The checks:

- `pn_pubkey_dec` and `pn_dih_dec` — decimal non-negative integers that fit in
  256 bits.
- `pn_seckey_hex` and `api_secret_hex` — valid hex.
- every `api_keys` entry — at least one permission, each a known label.

### Running the seed

1. In your api config set `auth.seed_accounts: true` (see
   [deployment.md](deployment.md#generate-a-kek) for where the flag sits).
2. Start the api once. It applies migrations, then inserts every account and key
   in one transaction, and logs a line like:
   ```
   seeded credentials accounts_inserted=10 accounts_skipped=0 api_keys_inserted=10 api_keys_skipped=0
   ```
3. Set `auth.seed_accounts: false` and restart.

Re-running is safe: the insert is idempotent (`ON CONFLICT DO NOTHING`, keyed on
`pn_address` for accounts and on the active `api_key` for keys), so rows that
already exist are counted under `*_skipped` rather than duplicated or
overwritten. To change an already-seeded account, edit it in the database
directly — re-seeding will not update it.

> **Editing `SEED_DATA` means rebuilding.** It is a compile-time constant, not a
> file the running container reads, so changes only take effect in a freshly
> built api image. If you would rather not rebuild, provision the accounts
> straight into the `accounts` / `api_keys` tables instead — you encrypt
> `pn_seckey` and `api_secret` under the KEK yourself; see
> [auth.md](tech-specs/auth.md) for the table contract.

## Funding a PrivateNote with the giver

On a network that has a giver (such as shellnet), **the giver tops up a
PrivateNote directly by its address** — the same `pn_address` that is in
`pn_pool.json` and in `SEED_DATA`. `mint_pn_pool` does this automatically at
deploy time, but you can also top up an already-deployed note later: send SHELL
(and native gas) from the giver to the note's address.

For getting test SHELL and using the giver on shellnet, follow the Acki Nacki
guide:

> https://dev.ackinacki.com/readme/get-test-tokens-in-shellnet#get-shell

A note that runs out of gas stops being able to execute trading messages until
it is topped up again, so re-funding existing seed notes is the normal way to
keep a long-running dev/test environment working.
