//! Sweep verdict for a `PrivateNote` account returning to the shared test
//! pool: judges whether its decoded storage is genuinely back to a neutral
//! state (no locks held, no stakes outstanding, no pending operation half
//! finished) before another scenario is allowed to reuse it.
//!
//! Two lists close the judgment against the actual ABI:
//!
//! - [`SWEEP_MUST_BE_EMPTY_OR_ZERO`] — every stateful field (nonces, locks,
//!   stakes, debt, coupons, order-book bookkeeping) that must read back as
//!   `null`, an all-zero numeric string (decimal `"0"` or a zero-padded
//!   `uint256` hex string), `0`, or an empty map, array, or string.
//! - [`SWEEP_MUST_BE_FALSE`] — the boolean-typed fields among them; `false`
//!   isn't covered by the "zero" notion above, so they're judged separately.
//!
//! [`sweep_verdict`] is fail-closed: a name from either list that is
//! *absent* from the decoded JSON is Dirty, not Clean. An absent key means
//! the decode didn't produce what was expected (wrong ABI, changed layout),
//! and defaulting that to "clean" would hand out an account whose state was
//! never actually inspected.
//!
//! The completeness test in `tests` below (`sweep_lists_cover_private_note_abi_fields`)
//! pins both lists — plus a third, allowed-non-sweep list local to that
//! test — against every field the `PrivateNote` ABI actually declares.
//! Without it, a contract upgrade that adds a new stateful field would rot
//! this module silently: `sweep_verdict` would simply never look at the new
//! field, a dirty account would start passing as clean, and the damage
//! would surface as an unrelated test failing much later, in a different
//! scenario, for reasons nobody could trace back here. The test forces a
//! human classification decision instead: it fails the moment the ABI's
//! field set no longer matches what's listed, and stays failing until
//! someone decides, by name, which bucket the new field belongs to.
//!
//! The account-pool allocator that consumes this verdict (`Allocator`,
//! `LeasedPn`, rent/taint/release) lands in a later change, in this same
//! file.

use serde_json::Value;

/// Storage fields that must be empty/zero for a note to be safe to hand back
/// into the pool: nonces and locks for whatever operation was in flight, and
/// every stateful ledger field (stakes, debt, coupons, order-book locks and
/// reservations). Closed against the ground-truth `PrivateNote` ABI on this
/// branch — see `sweep_lists_cover_private_note_abi_fields`.
pub const SWEEP_MUST_BE_EMPTY_OR_ZERO: &[&str] = &[
    "_busy",
    // Nonce of the operation currently IN FLIGHT — must be 0 at rest.
    // Contrast `_opNonce` (allowed list below), the monotonic all-time
    // counter, which stays nonzero on a reused note by design.
    "_busyOpNonce",
    "_pendingBatchBuyLock",
    "_pendingBatchTokenType",
    "_pendingBatchStakeHash",
    "_pendingBatchSells",
    "_pendingBatchClientOrderIds",
    "_pendingPlaceBuyLock",
    "_pendingPlaceBuyTokenType",
    "_pendingPlaceClientOrderId",
    "_pendingTransferAmount",
    "_pendingTransferTokenType",
    "_stakes",
    "_debt",
    "_couponsValue",
    "_lockedInOrders",
    "_orderLocks",
    "_orderFeeReserves",
    "_openOrderCount",
    "_openOrdersByEvent",
    "_clientOrderIds",
    "_streamLocks",
    "_disputeLocks",
    "_streamLockCount",
    "_disputeLockCount",
];

/// Boolean-typed storage fields that must be exactly `false`. Split out from
/// the list above because `false` isn't an instance of the "null / zero /
/// empty" notion `sweep_verdict` uses for everything else.
pub const SWEEP_MUST_BE_FALSE: &[&str] =
    &["_pendingBatchActive", "_pendingForfeit", "_hasWithdrawn", "_hasTransferred"];

/// 10 SHELL — enough headroom for roughly ten `deployPMP` calls. The
/// allocator quarantines a note as `ShellDepleted` once its ECC balance
/// drops below this.
#[allow(dead_code)] // consumed by the allocator's shell-budget check, added in a later change
pub const MIN_DEPLOY_SHELL: u128 = 10_000_000_000;

/// Verdict for one note's decoded storage.
#[derive(Debug, PartialEq, Eq)]
pub enum SweepVerdict {
    Clean,
    Dirty { fields: Vec<String> },
}

/// `null`, `0`, an empty object/array/string, or a numeric string that is
/// entirely zero digits.
///
/// The last case exists because `tvm_abi`'s detokenizer does not encode every
/// integer width the same way: a `uint256` decodes to a zero-padded hex
/// string (`"0x" + 64 hex digits`, per `detokenize_big_uint`), while every
/// narrower unsigned width decodes to a plain decimal string. Matching only
/// the literal `"0"` would accept a narrow-width zero but reject a genuine
/// `uint256` zero (`"0x000…000"`), which would make every real note
/// permanently Dirty on any `uint256` field in the sweep list — this has to
/// stay general (strip an optional `0x`, then check the remainder is
/// non-empty and all `'0'`) so the next `uint256` field added to the sweep
/// list is handled automatically, not by special-casing a field name here.
fn is_empty_or_zero(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => {
            let digits = s.strip_prefix("0x").unwrap_or(s);
            s.is_empty() || (!digits.is_empty() && digits.chars().all(|c| c == '0'))
        }
        // Defensive fallback only: the decoder emits strings for every
        // numeric ABI field (see above), so a JSON number here would mean an
        // unexpected decode shape, not the primary path.
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        Value::Bool(_) => false,
    }
}

/// Verdict over decoded storage fields (the JSON `ChainReader::storage_fields`
/// produces). Fail-closed: a name from either sweep list that is missing
/// from `fields` entirely counts as Dirty, exactly like a present-but-dirty
/// value — never as an implicit, acceptable null.
pub fn sweep_verdict(fields: &Value) -> SweepVerdict {
    let mut dirty = Vec::new();

    for name in SWEEP_MUST_BE_EMPTY_OR_ZERO {
        let clean = matches!(fields.get(name), Some(v) if is_empty_or_zero(v));
        if !clean {
            dirty.push((*name).to_string());
        }
    }
    for name in SWEEP_MUST_BE_FALSE {
        let clean = matches!(fields.get(name), Some(Value::Bool(false)));
        if !clean {
            dirty.push((*name).to_string());
        }
    }

    if dirty.is_empty() {
        SweepVerdict::Clean
    } else {
        SweepVerdict::Dirty { fields: dirty }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_flags_nonempty_stakes_and_true_flags() {
        // Boolean fields are exercised with a real false/true, not just
        // presence/absence.
        let v = serde_json::json!({ "_stakes": {"0":"5"}, "_hasWithdrawn": true,
            "_pendingBatchActive": false, "_pendingForfeit": false, "_busy": null, "_debt": "0" });
        match sweep_verdict(&v) {
            SweepVerdict::Dirty { fields } => {
                assert!(fields.contains(&"_stakes".to_string()));
                assert!(fields.contains(&"_hasWithdrawn".to_string()));
            }
            _ => panic!("должен быть Dirty"),
        }
    }

    /// The ABI declares each field's ("fields" section) name and Solidity
    /// type; this reads that map from the same ABI file the completeness
    /// test parses.
    fn private_note_abi_types() -> std::collections::HashMap<String, String> {
        let abi: serde_json::Value =
            serde_json::from_str(include_str!("../../../../contracts/dex/PrivateNote.abi.json"))
                .unwrap();
        abi["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (f["name"].as_str().unwrap().to_string(), f["type"].as_str().unwrap().to_string())
            })
            .collect()
    }

    /// The JSON shape `tvm_abi`'s detokenizer actually produces for a
    /// zero/empty value of the given ABI type — NOT a guess. `uint256`
    /// encodes to a zero-padded hex string, every narrower unsigned width to
    /// a plain decimal string, maps/arrays to an empty object/array,
    /// `optional(..)` to `null` when absent, `bool` to `false`. Panics on an
    /// ABI type this mapping doesn't cover yet, so a future field of a new
    /// shape fails loudly here instead of silently building a test fixture
    /// the real decoder could never produce.
    fn clean_value_for_abi_type(abi_type: &str) -> serde_json::Value {
        if abi_type.ends_with("[]") {
            serde_json::json!([])
        } else if abi_type.starts_with("map(") {
            serde_json::json!({})
        } else if abi_type.starts_with("optional(") {
            serde_json::Value::Null
        } else if abi_type == "bool" {
            serde_json::json!(false)
        } else if abi_type == "uint256" {
            serde_json::json!(format!("0x{}", "0".repeat(64)))
        } else if abi_type.starts_with("uint") {
            serde_json::json!("0")
        } else {
            panic!("clean_value_for_abi_type: unhandled ABI type `{abi_type}` — extend the mapping")
        }
    }

    #[test]
    fn sweep_clean_note_passes() {
        // Built from the ABI's actual field types (via `clean_value_for_abi_type`),
        // not from a guessed shape — this is what caught the original bug: a
        // hardcoded `null` for every field can never surface a `uint256`
        // field's real hex-zero encoding.
        let types = private_note_abi_types();
        let mut m = serde_json::Map::new();
        for f in SWEEP_MUST_BE_EMPTY_OR_ZERO.iter().chain(SWEEP_MUST_BE_FALSE) {
            let ty = types.get(*f).unwrap_or_else(|| panic!("field {f} missing from ABI"));
            m.insert((*f).into(), clean_value_for_abi_type(ty));
        }
        assert!(matches!(sweep_verdict(&serde_json::Value::Object(m)), SweepVerdict::Clean));
    }

    #[test]
    fn sweep_uint256_hex_zero_is_clean() {
        // `_pendingBatchStakeHash` is the sweep list's one `uint256` field.
        // Regression guard for the bug `sweep_clean_note_passes` could not
        // catch on its own: a lone all-zero hex string must be judged clean,
        // the same as the decimal `"0"` the narrower widths use.
        assert!(is_empty_or_zero(&serde_json::json!(format!("0x{}", "0".repeat(64)))));
        assert!(!is_empty_or_zero(&serde_json::json!(format!("0x{}1", "0".repeat(63)))));
    }

    #[test]
    fn sweep_bool_field_rejects_non_boolean_shapes() {
        // `SWEEP_MUST_BE_FALSE` requires the JSON boolean `false` exactly —
        // a string or a numeric zero must not be accepted as a stand-in.
        let v = serde_json::json!({ "_hasWithdrawn": "false", "_hasTransferred": 0 });
        match sweep_verdict(&v) {
            SweepVerdict::Dirty { fields } => {
                assert!(fields.contains(&"_hasWithdrawn".to_string()));
                assert!(fields.contains(&"_hasTransferred".to_string()));
            }
            _ => panic!("string/number stand-ins for false must be Dirty"),
        }
    }

    #[test]
    fn sweep_missing_required_field_fails() {
        // Fail-closed: a key named by a sweep list but absent from the
        // decoded fields entirely is NOT equivalent to a clean null. Checked
        // for one field from each list — the zero list and the boolean list
        // use different clean-value checks, so a missing key has to be
        // caught on both paths, not just one.
        let v = serde_json::json!({ "_stakes": null }); // no other keys at all
        match sweep_verdict(&v) {
            SweepVerdict::Dirty { fields } => {
                assert!(fields.contains(&"_busy".to_string()));
                assert!(fields.contains(&"_hasWithdrawn".to_string()));
            }
            _ => panic!("отсутствующее обязательное поле обязано давать Dirty"),
        }
    }

    /// Every field the `PrivateNote` ABI declares must be explicitly
    /// classified: in one of the two sweep lists above, or in
    /// `ALLOWED_NON_SWEEP` here (immutable/code fields, plus gosh's
    /// replay-protection bookkeeping that mutates on every operation but
    /// doesn't block reuse). A contract upgrade that adds a new stateful
    /// field fails this test until a human decides, by name, which bucket
    /// it belongs to — the alternative is a silent hole: `sweep_verdict`
    /// would never look at the new field, and a dirty account would start
    /// passing as clean.
    #[test]
    fn sweep_lists_cover_private_note_abi_fields() {
        // Closed against the ground-truth ABI on this branch: 53 fields =
        // 25 must-be-empty-or-zero + 4 must-be-false + 24 allowed. This ABI
        // has no `_timestamp` field; instead it carries `messages` /
        // `lastMessage` — gosh replay-protection state that mutates during
        // operations but does not block reuse.
        const ALLOWED_NON_SWEEP: &[&str] = &[
            "_pubkey",
            "_constructorFlag",
            "messages",
            "lastMessage", // system bookkeeping
            "_depositIdentifierHash",
            "_ephemeralPubkey",
            "_balance",
            "_pmpCode",
            "_privateNoteCode",
            "_orderBookCode",
            "_inferenceOrderBookCode",
            "_oracleCodeHash",
            "_oracleCodeDepth",
            "_oracleEventListCodeHash",
            "_oracleEventListCodeDepth",
            "_tokenContractCodeHash",
            "_tokenContractCodeDepth",
            "_rootModelCodeHash",
            "_rootModelCodeDepth",
            "_debtTokenType",
            "_couponsTokenType",
            "_lastStreamLockChange",
            "_lastHash", // hash of the last external message (replay guard); survives operations
            "_opNonce",  // monotonic count of all operations ever run; nonzero on a reused note
        ];
        let abi_fields: Vec<String> = private_note_abi_types().into_keys().collect();
        for f in SWEEP_MUST_BE_EMPTY_OR_ZERO.iter().chain(SWEEP_MUST_BE_FALSE) {
            assert!(
                abi_fields.contains(&f.to_string()),
                "поле {f} из sweep-списка отсутствует в ABI — обновить список"
            );
        }
        for f in &abi_fields {
            let known = SWEEP_MUST_BE_EMPTY_OR_ZERO.contains(&f.as_str())
                || SWEEP_MUST_BE_FALSE.contains(&f.as_str())
                || ALLOWED_NON_SWEEP.contains(&f.as_str());
            assert!(
                known,
                "НОВОЕ stateful-поле ABI {f} не классифицировано sweep'ом — молчаливая дыра запрещена"
            );
        }
    }
}
