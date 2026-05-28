//! DEX proof helpers — token taxonomy, nominal denominations, and a
//! `Halo2Proof` re-export. Voucher proofs are produced by
//! `dodex_sdk::halo2::live::prove_voucher_for_event` against an event
//! that the caller (e.g. a multifactor wallet) has already emitted on
//! `RootPN`. The test suite drives that flow via Giver in
//! `tests/integration/common/voucher.rs::make_voucher_proof`.

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

pub use crate::services::halo2::live::Halo2Proof;

// ── Token types ──────────────────────────────────────────────────

/// Token types supported by the DEX contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenType {
    /// NACKL (currency_id=1, 9 decimals)
    Nackl = 1,
    /// SHELL (currency_id=2, 9 decimals)
    Shell = 2,
    /// USDC (currency_id=3, 6 decimals)
    Usdc = 3,
    /// DEX Shell Fee — gas for PN internal messages (currency_id=300, 9
    /// decimals). Matches `CURRENCIES_ID_SHELL_FEE` in
    /// contracts/dex/modifiers/modifiers.sol.
    DexShellFee = 300,
}

impl TokenType {
    pub fn id(self) -> u32 {
        self as u32
    }

    pub fn decimals(self) -> u64 {
        match self {
            TokenType::Usdc => 1_000_000,
            _ => 1_000_000_000,
        }
    }

    /// Whether this token type is subject to nominal validation on deploy.
    pub fn has_nominal_validation(self) -> bool {
        !matches!(self, TokenType::DexShellFee)
    }
}

// ── Nominals ─────────────────────────────────────────────────────

/// PrivateNote deposit nominal — a fixed denomination enforced by the contract.
/// Matches `ALLOWED_NOMINALS` in contracts/dex/modifiers/modifiers.sol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Nominal {
    /// 100 tokens
    N100 = 100,
    /// 1,000 tokens
    N1000 = 1_000,
    /// 10,000 tokens
    N10000 = 10_000,
}

impl Nominal {
    /// Raw value with decimals applied, ready for ZK proof and deploy.
    pub fn raw_value(self, token_type: TokenType) -> u64 {
        (self as u64) * token_type.decimals()
    }

    /// All allowed nominals.
    pub fn all() -> &'static [Nominal] {
        &[Nominal::N100, Nominal::N1000, Nominal::N10000]
    }
}

// ── Misc helpers ─────────────────────────────────────────────────

pub fn random_secret_key() -> String {
    let seed = format!(
        "{}:{}:dodex-sdk",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos(),
    );
    let mut bytes = Sha256::digest(seed.as_bytes()).to_vec();
    // Last byte < 0x30 keeps the 32-byte little-endian value < BN254 Fr modulus.
    bytes[31] %= 0x30;
    hex::encode(bytes)
}

pub fn hex_u256_to_dec(hex: &str) -> String {
    let hex = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X")).unwrap_or(hex);
    num_bigint::BigUint::parse_bytes(hex.as_bytes(), 16).expect("valid hex uint256").to_string()
}

pub fn pubkey_to_dec(pubkey: &str) -> String {
    let with_prefix = if pubkey.starts_with("0x") || pubkey.starts_with("0X") {
        pubkey.to_string()
    } else {
        format!("0x{pubkey}")
    };
    hex_u256_to_dec(&with_prefix)
}

/// Strip `0x`/`0X` prefix (case-insensitive); returns the original slice if
/// absent.
pub fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s)
}

/// Parse a u256 string that could be either hex (0x-prefixed) or decimal.
pub fn parse_u256(value: &str) -> num_bigint::BigUint {
    if let Some(hex) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        return num_bigint::BigUint::parse_bytes(hex.as_bytes(), 16).expect("valid hex uint256");
    }
    num_bigint::BigUint::parse_bytes(value.as_bytes(), 10).expect("valid decimal uint256")
}
