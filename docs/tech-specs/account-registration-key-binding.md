# Registration key binding

`POST /api/v1/accounts` is public, so before minting a credential the use
case proves the caller controls the note — otherwise anyone who learns a
deployed `pnAddress` / `pnDih` (both readable on-chain) could register it
with an arbitrary key. The bogus credential could not trade (the note
rejects its signatures), but the row would occupy the note's unique
constraint and block the real owner until an operator cleared it.

## What the check does

[`RegisterAccountUseCase`](../../crates/application/src/lib.rs), after the
deployment probe and before any write:

1. Derives the ed25519 public key from the submitted `pnSeckeyHex`
   (`derive_ed25519_pubkey_hex`, standard ed25519 — pinned to the RFC 8032
   vector in a unit test).
2. Reads the note's on-chain owner key via
   `PnStateReader::owner_pubkey`, which decodes the `_ephemeralPubkey`
   storage field straight from the account BOC
   (`tvm_runner::decode_account_fields` → `decode_storage_fields`).
   `PrivateNote` exposes no getter for it, so this reads the data cell
   directly. The owner key is `_ephemeralPubkey` — not `tvm.pubkey()` /
   `_pubkey`, which is 0 because PNs are deployed internally by RootPN — set
   in the constructor and rotated by `changeOwner`; reading the live field
   means a `changeOwner` is reflected automatically.
3. Mismatch ⇒ `DomainError::KeyDoesNotOwnNote` (-2016, HTTP 400). The
   registry is never called, so no row is written.

## Coverage

Unit tests cover the derivation (RFC 8032 vector + malformed input), the
use-case branch (matching key registers; wrong key → -2016, no write), and
the `_ephemeralPubkey` parse. The end-to-end link — that the decoded
`_ephemeralPubkey` of a real shellnet note equals `derive(its seckey)` — is
exercised by the `#[ignore]d`
[`e2e_register_account.rs`](../../services/api/tests/e2e_register_account.rs)
(`register_deployed_pn_against_shellnet`), which registers a live note and
must get 200.

## Stored public key

Before the on-chain binding, the use case also checks the submitted
`pnPubkeyHex` is the key the submitted `pnSeckeyHex` derives — comparing as
uint256 values, so case, an optional `0x` prefix, and leading-zero width do
not matter (`uint256_hex_eq`). A mismatch is `InvalidParameter` (-1130), and
the registry is never called. The stored `accounts.pn_pubkey` is later paired
with the sealed seckey to build the chain signer (`KeyPair { public, secret }`);
a pair whose public is not the seckey's key can never sign, so without this
check a correct seckey plus a wrong `pnPubkeyHex` would mint a 200-OK
credential that could never trade. A registration that succeeds therefore has
a public key consistent with both its seckey and the note's on-chain owner.

The unit test `pubkey_seckey_mismatch_is_rejected_and_skips_write` and the
HTTP test `register_pubkey_seckey_mismatch_returns_1130` pin the reject path
(no write); `uint256_hex_eq_*` pin the canonical comparison.
