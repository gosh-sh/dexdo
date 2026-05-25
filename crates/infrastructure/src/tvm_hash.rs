// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Off-chain replication of `tvm.hash(abi.encode(eventId, oracleListHash,
// tokenType))` — the key used by PrivateNote._stakes. The on-chain
// computation lives at PrivateNote.sol (search "tvm.hash(abi.encode").
// Off-chain we re-pack the same (uint256, uint256, uint32) tuple via
// tvm_abi and take the cell's representation hash.

use anyhow::Context;
use num_bigint::BigUint;
use tvm_abi::contract::AbiVersion;
use tvm_abi::token::Tokenizer;
use tvm_abi::Param;
use tvm_abi::ParamType;
use tvm_abi::TokenValue;

/// Off-chain `tvm.hash(abi.encode(eventId, oracleListHash, tokenType))`.
/// Returns a `0x`-prefixed lowercase hex string (32 bytes / 64 hex chars
/// after the prefix) to match the on-chain `uint256.toHexString` shape.
pub fn stake_hash(
    event_id: &BigUint,
    oracle_list_hash: &BigUint,
    token_type: u32,
) -> anyhow::Result<String> {
    // PrivateNote.abi.json is ABI v2.4. The AbiVersion governs how
    // tightly tuples pack; we mirror the PN's own version so the
    // off-chain cell is byte-identical to the on-chain one.
    let abi = AbiVersion { major: 2, minor: 4 };

    let params = vec![
        Param { name: "event_id".into(), kind: ParamType::Uint(256) },
        Param { name: "oracle_list_hash".into(), kind: ParamType::Uint(256) },
        Param { name: "token_type".into(), kind: ParamType::Uint(32) },
    ];
    let json = serde_json::json!({
        "event_id": event_id.to_string(),
        "oracle_list_hash": oracle_list_hash.to_string(),
        "token_type": token_type.to_string(),
    });
    let tokens = Tokenizer::tokenize_all_params(&params, &json)
        .map_err(|err| anyhow::anyhow!("tokenize stake-hash params: {err}"))?;
    let builder = TokenValue::pack_values_into_chain(&tokens, vec![], &abi)
        .map_err(|err| anyhow::anyhow!("pack stake-hash chain: {err}"))?;
    let cell = builder.into_cell().context("builder into cell")?;
    let hex = cell.repr_hash().as_hex_string();
    Ok(format!("0x{hex}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_inputs() {
        let e = BigUint::from(42u32);
        let o = BigUint::from(24u32);
        let h1 = stake_hash(&e, &o, 1).unwrap();
        let h2 = stake_hash(&e, &o, 1).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn differs_when_any_input_changes() {
        let e1 = BigUint::from(42u32);
        let e2 = BigUint::from(43u32);
        let o = BigUint::from(24u32);
        assert_ne!(stake_hash(&e1, &o, 1).unwrap(), stake_hash(&e2, &o, 1).unwrap());
        assert_ne!(
            stake_hash(&e1, &o, 1).unwrap(),
            stake_hash(&e1, &BigUint::from(25u32), 1).unwrap()
        );
        assert_ne!(stake_hash(&e1, &o, 1).unwrap(), stake_hash(&e1, &o, 2).unwrap());
    }

    #[test]
    fn shape_is_0x_prefixed_64_hex_chars() {
        let h = stake_hash(&BigUint::from(1u32), &BigUint::from(2u32), 3).unwrap();
        assert!(h.starts_with("0x"), "got: {h}");
        assert_eq!(h.len(), 2 + 64, "got: {h}");
        assert!(h.chars().skip(2).all(|c| c.is_ascii_hexdigit()));
    }

    /// Recorded vector — must not change unless `tvm_abi` semantics
    /// intentionally shift. If this test fails after a dependency
    /// bump, do NOT just update the expected hash: re-verify against
    /// a real PN's `_stakes` map keys (the integration suite in
    /// crates/infrastructure/tests/balances.rs + the API e2e test
    /// drive this in production).
    #[test]
    fn pinned_vector_does_not_drift() {
        let e = BigUint::from(0x42u32);
        let o = BigUint::from(0x24u32);
        let h = stake_hash(&e, &o, 1).unwrap();
        // First-run value: copy from running `cargo test pinned_vector_does_not_drift`
        // and paste here. Replace the placeholder once on a clean build.
        let pinned = "0xb9165587c603af7c59d0fc5db123a8cab4d08ece1419f2049c3cdbd9b3a0f6d8";
        assert_eq!(h, pinned);
    }
}
