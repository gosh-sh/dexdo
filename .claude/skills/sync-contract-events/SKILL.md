---
name: sync-contract-events
description: Use when the smart contracts have been updated — a contracts sync commit arrived (e.g. "feat(contracts): v4.0.16") or fresh artifacts landed in the acki-nacki repo, when the indexer logs "no handler for event type" / an unknown event type, or when decode errors or undecodable raw_events rows (event_type IS NULL) appear after a contract upgrade.
---

# Sync Contract Events

Audit what the backend must change after a contract update. Two distinct event identities drive everything:

- **ABI signature id** (32-bit hash of the event name + input types) — how the decoder recognizes a body. Built at **compile time**: `crates/infrastructure/src/decoder.rs` embeds the ABIs via `include_str!` (`DEX_ABIS`: RootOracle, Oracle, OracleEventList, PMP, OrderBook, RootPN, PrivateNote, Nullifier; `INFERENCE_ABIS`: InferenceOrderBook, TokenContract, RootModel). RootModel is in that list for a load-bearing reason — it resolves the `ContractDeployed` id collision with `TokenContract` via `add_route`. Three ABIs ship in the repo and are NOT loaded — `SuperRoot`, `ModelRegistry` and `Multisig`; events from those contracts decode to nothing and are invisible to every count below. Changing an `.abi.json` requires rebuild + redeploy.
- **EVENT_ID constant** (`contracts/dex/modifiers/modifiers.sol`, e.g. `OB_QUEUED = 159`) — the external `dst` each event is emitted to (`:{id:064x}`). Used by the ingest ignore-filter (`config::ignored_event_dsts`) and by decoder dst routes for colliding signature ids.

Two scripts ship with this skill: `diff-events.sh` (ABI event sets, Step 1) and
`diff-signatures.sh` (Solidity signatures **with modifiers**, Step 1b). Run both — an
ABI diff cannot see a modifier, a guard, or a changed constant.

Contract updates arrive as artifact syncs from the acki-nacki repo (compiled dirs like `/home/sauin/devel/gosh-sh/acki-nacki/contracts/<ver>_compiled/dex/` — `.sol` + `.tvc` + `.abi.json`) copied into `contracts/dex/` and `contracts/airegistry/`.

## Step 1 — Diff event sets (updated ABIs vs repo)

```bash
.claude/skills/sync-contract-events/diff-events.sh <new-artifacts-dir>   # e.g. .../acki-nacki/contracts/<ver>_compiled/dex
```

Prints `NEW` / `REMOVED` / `CHANGED` (input types differ) per `Kind.Event`, comparing against `contracts/` by default. If the sync commit already landed, diff against the previous commit instead: run the script with `old-contracts-dir` pointing at a `git worktree` of the pre-sync revision, or eyeball `git diff HEAD~1 -- 'contracts/**/*.abi.json'`.

**Rename detection:** the script matches exact `.name`, so a pure event rename (e.g. `VoucherGenerated`→`voucherGenerated`, `PMPRejected`→`PMPCancelled`) shows up as an unrelated NEW + REMOVED pair. A NEW+REMOVED pair with similar names or the same EVENT_ID in `modifiers.sol` is one rename — handle it as a unit: port the projector arm and update the SDK typed enum together, don't treat the halves independently.

Manual equivalent (one tree):

```bash
for f in contracts/dex/*.abi.json contracts/airegistry/*.abi.json; do
  kind=$(basename "$f" .abi.json)
  jq -r --arg k "$kind" '(.events // [])[] | $k + "." + .name + "(" + ([.inputs[].type] | join(",")) + ")"' "$f"
done | sort
```

## Step 1b — Diff the `.sol` sources (MANDATORY, not optional)

Steps 1 and 4 compare ABIs. An ABI carries names and types — it does **not** carry
modifiers, owner gates, state mutability, `require` guards, constants, or which
branch a function takes. A sync can change all of those while every ABI diff stays
clean. This has now bitten twice (v4.0.29 and v4.0.32), so do it every time.

```bash
# 1. Function signatures INCLUDING modifiers — catches gates and `view`/`pure`.
.claude/skills/sync-contract-events/diff-signatures.sh <sync-commit>   # default baseline: <sync-commit>~1

# 2. New/changed guards, constants and error codes.
git diff <sync-commit>~1 <sync-commit> -- 'contracts/**/*.sol' \
  | grep -E '^\+' | grep -E 'require\(|revert|constant |ERR_'

# 3. Per-file eyeball of anything with a big line delta.
git diff --stat <sync-commit>~1 <sync-commit> -- 'contracts/**/*.sol'
```

What each has actually caught:

| Change | Why the ABI diff missed it | Found in |
|---|---|---|
| `PrivateNote.changeOwner` gained `onlyOwnerPubkey(_ephemeralPubkey)` — it had **no owner gate** before | modifiers are not in the ABI | v4.0.32 |
| `OracleEventList.resolveRange` / `onWeeklyMedian` → `view`; `PrivateNote.onInferencePlaced` / `onInferenceFilled` → `view` (dropping `accept` for an explicit `tvm.accept()`) | mutability is not in the ABI | v4.0.32 |
| `OrderBook.placeBatch` stopped **truncating** an over-long batch and now reverts `ERR_BATCH_TOO_LARGE` | same signature, opposite failure mode | v4.0.32 |
| `require(bounds[0] > 0)` in `addRangeEvent`; `require(ttl != 0 && ttl <= MAX_SELL_TTL)` in `postSellOffer` | new validation, unchanged signature | v4.0.32 |
| `RootPN` code-hash / depth constants for `TokenContract` + `RootModel` changed | constants are not in the ABI | v4.0.32 |
| `_recordTrade` moved out of matching into `reportFinalized`; `fundSellerBond` gated on `_sellerNote` | call-site move, unchanged ABI | v4.0.29 |

Two traps when reading the result:

- **`view` in TVM Solidity means "does not write state" — it does NOT forbid `emit` or
  outbound messages.** `resolveRange` still sends `requestWeeklyMedian`, and the note-side
  mirrors still emit `InferenceOrderPlacedConfirmed` / `InferenceFilledConfirmed`. Read the
  body before treating a `view` marking as a break.
- **A new `require` is a write-side problem, not a read-side one.** It cannot break the
  indexer; it breaks whatever we *send*. Check the SDK wrapper and any e2e fixture that
  supplies the newly-constrained value, and record the rule in the wrapper's doc comment —
  that is where the next person will violate it. (`ttl: 0` cost a full 33-minute e2e run
  because "0 = good-till-cancel" held for a BUY deadline but not for a SELL ttl.)

Also sweep for wrappers calling functions that no longer exist — the shape tests pin
parameter sets, not that a function still exists, so such a wrapper compiles and only
fails on chain:

```bash
grep -oE '"[a-z][A-Za-z0-9]+"' crates/contracts/src/<area>/<contract>.rs | tr -d '"' | sort -u > /tmp/w
jq -r '.functions[].name' contracts/<area>/<Contract>.abi.json | sort -u > /tmp/a
comm -23 /tmp/w /tmp/a   # wrapper-side names absent from the ABI
```

False positives: jq field names in tests (`functions`, `inputs`, `name`) and deliberate
absence pins (`private_note.rs` asserts `streamLock` / `streamUnlock` are gone).

## Step 2 — Enumerate what the code knows

The decoder auto-knows whatever is in the embedded ABIs, so the gap to audit is ABI vs **projector arms** and **ignore tables**:

```bash
# DEX projector arms (full event_type match)
sed -n '/match event.event_type.as_str/,/ProjectionOutcome::Unknown/p' crates/infrastructure/src/projectors.rs \
  | sed 's/=>.*//' | grep -oE '"[A-Za-z.]+"' | tr -d '"' | sort -u
# Inference / TokenContract arms. These two dispatch on an ENUM, not on strings, so
# grep for the variants — a string-grep here picks up comment text and apply_close()
# arguments instead. And it is `match kind`: the one `match suffix` left in
# inference_projectors.rs is the orphan-repair dispatch, so a grep for that prints
# four unrelated names while looking like a complete answer.
# Expect 9 and 15 — the same numbers as the decoder's per-ABI id counts.
for f in crates/infrastructure/src/inference_projectors.rs crates/infrastructure/src/token_contract_projectors.rs; do
  echo "== $f"; awk '/match kind \{/,/^    \}$/' "$f" | grep -oE 'E::[A-Za-z]+' | sort -u
done
# Config-ignorable set + its dst ids
sed -n '/pub const IGNORABLE_EVENT_TYPES/,/^];/p; /pub const IGNORABLE_EVENT_IDS/,/^];/p' crates/infrastructure/src/config.rs
```

Compare with Step 1 output. Also check `grep -nE 'constant [A-Z_]+ = [0-9]+' contracts/dex/modifiers/modifiers.sol` for new/renumbered EVENT_ID constants. Note: a dex-only artifact sync may not include `modifiers.sol` — dst-id changes can arrive in a separate sync; fall back to the repo's current copy and flag any id you cannot confirm from the delivered artifacts.

## Step 3 — Decide per NEW event: project or ignore

An event with no projector arm decodes fine but hits `ProjectionOutcome::Unknown`: the indexer warns `"reprojection has no handler for event type"` (first sighting; repeats go to the noise log) and **marks the row processed immediately** — it is never retried, and stage prunes processed `raw_events` after 3 days. Decide promptly.

**Project it** (the event must change the read model):

1. `migrations/` — new numbered `NNNN_*.sql` if new tables/columns (applied by `sqlx::migrate!`, `crates/infrastructure/src/database.rs`); document in `docs/tech-specs/data-schema.md`.
2. Add the match arm + `apply_*` fn: `crates/infrastructure/src/projectors.rs` (DEX), `inference_projectors.rs` (InferenceOrderBook), `token_contract_projectors.rs` (TokenContract).
3. Read side if the API serves it: `crates/infrastructure/src/rows.rs` + the read repo (`postgres_repo.rs` / `inference_read_repo.rs`).
4. Rows already marked processed as Unknown are not replayed automatically — backfill manually if history matters.

**Ignore it** (a genuine no-op: nothing to persist, not metric-critical). Three places, in this order:

1. `crates/infrastructure/src/projectors.rs` — add it to a no-op arm returning `Ok(ProjectionOutcome::Applied)`.
2. `crates/infrastructure/src/config.rs` — append to `IGNORABLE_EVENT_TYPES` **and** `IGNORABLE_EVENT_IDS` (id = the `makeAddrExtern` EVENT_ID from `modifiers.sol`, not the ABI signature id). Unit tests pin names-equality and ids-vs-`modifiers.sol`; `crates/infrastructure/tests/ignorable_event_types_projector_noop.rs` pins the no-op arm.
3. Config `ignored_event_types` lists (optional — only to shed ingest volume): `config/indexer.local.yaml`, `config/indexer.stage.supabase.yaml`, `deploy/ansible/roles/dexdo/templates/indexer.yaml.j2`.

Never ignore `OrderBook.OrderPlaced` / `OrderBook.PartialFill` (`METRIC_CRITICAL_EVENT_TYPES` — their raw_events rows back OTLP counters).

## Step 4 — CHANGED shape / REMOVED events

Detect field-level changes (the script compares types only; renamed fields hide):

```bash
diff <(jq '.events[] | select(.name=="OrderPlaced").inputs' contracts/dex/OrderBook.abi.json) \
     <(jq '.events[] | select(.name=="OrderPlaced").inputs' <new-dir>/OrderBook.abi.json)
```

What breaks, by change kind:

| Change | Symptom | Fix |
|---|---|---|
| Event renamed or input **types** changed, repo ABI stale | Signature id unknown to old binary → `DecodeOutcome::UnknownId` → row stored with `event_type IS NULL`, never projected, silent except the `decode_errors`/undecodable counters. Check via staging-diag L2: `select count(*) from raw_events where event_type is null;` | Sync the ABI, rebuild, redeploy. Undecodable rows stay undecoded (no auto re-decode). |
| Field **renamed** (ABI synced, projector stale) | Projector `.get("oldField")` misses → deterministic projection error → row retried forever via per-row savepoints → L2 backlog grows | Update the `apply_*` fn field names. |
| New ABI's signature id **collides** with an existing one | `AmbiguousCollision` warn, row left undecoded | Add a dst route in `Decoder::new` (`add_route` with the modifiers.sol EVENT_ID). |
| Event **removed** / no longer emitted | Nothing breaks; dead arms and enum variants linger | Optional cleanup; precedent: `OB_EPOCH_SETTLED = 145` kept in `modifiers.sol`, intentionally omitted from `order_book_events.rs`. |
| **METRIC_CRITICAL event removed** (e.g. `OrderBook.PartialFill`) | The OTLP counter silently goes dark — no error anywhere; `metrics_refresh.rs` pins and `METRIC_CRITICAL_EVENT_TYPES` still reference it | A deliberate product decision, not silent cleanup: retire or replace the metric, then unwind `METRIC_CRITICAL_EVENT_TYPES` + `services/indexer/src/metrics_refresh.rs` in lockstep. |

## Touchpoints

| File | What changes there |
|---|---|
| `contracts/{dex,airegistry}/*.{abi.json,tvc,sol}` | Artifact sync from acki-nacki compiled dir (rebuild required — ABIs are `include_str!`). Audit the `.sol` diff too — see [Step 1b](#step-1b--diff-the-sol-sources-mandatory-not-optional) |
| `contracts/dex/modifiers/modifiers.sol` | EVENT_ID constants (external dst ids) — may arrive with the sync or in a separate one (see Step 2 note) |
| `crates/infrastructure/src/decoder.rs` | New contract → add to `DEX_ABIS`/`INFERENCE_ABIS`; colliding id → `add_route`; two tests pin the count (decoder.rs:296 and :398, both `== 82` today) — update both literals, but note they are ONE fact, not two: `known_events()` and `unique_event_ids()` both return `self.event_index.len()`, so neither can disagree with the other and passing both proves only that the number was updated |
| `crates/infrastructure/src/projectors.rs` (+ `inference_projectors.rs`, `token_contract_projectors.rs`) | Match arm per event: `apply_*` or no-op `Applied` |
| `crates/infrastructure/src/config.rs` | `IGNORABLE_EVENT_TYPES` + `IGNORABLE_EVENT_IDS` (+ `METRIC_CRITICAL_EVENT_TYPES` if a new event backs a metric) |
| `config/indexer.local.yaml`, `config/indexer.stage.supabase.yaml`, `deploy/ansible/roles/dexdo/templates/indexer.yaml.j2` | `ignored_event_types` entries |
| `migrations/NNNN_*.sql`, `docs/tech-specs/data-schema.md` | New read-model tables/columns |
| `crates/infrastructure/src/rows.rs`, `postgres_repo.rs` / `inference_read_repo.rs` | Read-model rows/queries if the API serves the new data |
| `crates/contracts/src/{dex,airegistry}/*_events.rs` | Typed EVENT_ID enums + event structs consumed by the SDK (`sdk/src/services/order_book_event.rs`, `history.rs`) — update only if the SDK needs the event |
| `crates/infrastructure/tests/ignorable_event_types_projector_noop.rs`, `services/indexer/src/metrics_refresh.rs` (tests) | Pins for the ignorable / metric-critical sets |

## Common mistakes

- **Yaml first, code never**: adding a type to `ignored_event_types` that is not in `IGNORABLE_EVENT_TYPES` fails indexer **startup** — `IndexerConfig::validate` is an allow-list (rejects metric-critical, state-changing types, and typos). Code (`config.rs` + projector no-op arm) first, config second. (Ignore the stale comment in `config/indexer.stage.supabase.yaml` claiming only metric-critical types are rejected — the code is an allow-list.)
- **Half-registering an ignorable**: `IGNORABLE_EVENT_TYPES` without the matching `IGNORABLE_EVENT_IDS` entry or projector no-op arm — config.rs unit tests and `ignorable_event_types_projector_noop.rs` fail. The ID comes from `modifiers.sol`, not from the ABI signature.
- **Forgetting the decoder count pins**: any event add/remove breaks the two `decoder.rs` tests asserting `== 82` — update both literals and the arithmetic comments next to them (the two calls return the same value, so they fail and pass together).
- **Expecting a running service to see new ABIs**: `include_str!` — the fix ships with a rebuild + redeploy, not a config reload.
- **Assuming unhandled events queue up**: `Unknown` rows are marked processed on first sight and pruned on stage after 3 days; a projector added later needs an explicit backfill.
