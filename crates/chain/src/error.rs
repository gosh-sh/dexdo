// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Thin enum over the kit's own error types. Each `Dex` method either
// forwards a `KitError` from a contract handle, surfaces a
// `ClientError` from `ClientContext::new`, or — only for the DTO
// adapters in `dto.rs` — a local `Decode` failure when the contract
// returned a shape we couldn't parse. No re-wrapping of kit fields:
// callers can match on the variant and read kit types directly.

use std::fmt;

use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::tvm_client::error::ClientError;
use serde::Deserialize;

pub type ChainResult<T> = Result<T, ChainError>;

#[derive(Debug)]
pub enum ChainError {
    /// Contract handle method failed. The kit's typed error — module,
    /// code, message, optional `tvm_error` payload — is exposed as-is.
    Kit(KitError),
    /// `ClientContext` setup failed (bad config, OOM, etc.). The
    /// `Dex::from_endpoints` helper is the only producer.
    Client(ClientError),
    /// Local failure decoding a contract return value into a typed DTO
    /// (e.g. parallel-array length mismatch from `getOrdersByOwner`).
    Decode(String),
}

impl ChainError {
    /// Pull the TVM `require(...)` exit code out of the underlying
    /// failure if there is one. Returns `None` for transport, decode,
    /// or non-TVM failures — the caller maps those to a generic
    /// "unexpected" surface.
    pub fn tvm_exit_code(&self) -> Option<u32> {
        let client_err = match self {
            ChainError::Kit(kit) => kit.tvm_error.as_ref()?,
            ChainError::Client(c) => c,
            ChainError::Decode(_) => return None,
        };
        let data: TvmData = serde_json::from_value(client_err.data().clone()).ok()?;
        data.node_error?.extensions?.details?.exit_code
    }
}

impl From<KitError> for ChainError {
    fn from(e: KitError) -> Self {
        ChainError::Kit(e)
    }
}

impl From<ClientError> for ChainError {
    fn from(e: ClientError) -> Self {
        ChainError::Client(e)
    }
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainError::Kit(e) => write!(f, "{}", e.message),
            ChainError::Client(e) => write!(f, "{}", e.message()),
            ChainError::Decode(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ChainError {}

#[derive(Debug, Deserialize)]
struct TvmData {
    #[serde(default)]
    node_error: Option<TvmNodeError>,
}

#[derive(Debug, Deserialize)]
struct TvmNodeError {
    #[serde(default)]
    extensions: Option<TvmExtensions>,
}

#[derive(Debug, Deserialize)]
struct TvmExtensions {
    #[serde(default)]
    details: Option<TvmDetails>,
}

#[derive(Debug, Deserialize)]
struct TvmDetails {
    #[serde(default)]
    exit_code: Option<u32>,
}

#[cfg(test)]
mod tests {
    use ackinacki_kit::contracts::error::KitErrorCode;
    use ackinacki_kit::contracts::error::KitModule;

    use super::*;

    fn tvm_with_exit(code: u32) -> ClientError {
        ClientError::new(
            414,
            "test",
            serde_json::json!({
                "node_error": { "extensions": { "details": { "exit_code": code } } }
            }),
        )
    }

    #[test]
    fn tvm_exit_code_unwraps_kit_with_tvm_payload() {
        let kit = KitError {
            module: KitModule::Account,
            code: KitErrorCode::None,
            message: "Send message".into(),
            tvm_error: Some(tvm_with_exit(121)),
        };
        let err = ChainError::Kit(kit);
        assert_eq!(err.tvm_exit_code(), Some(121));
    }

    #[test]
    fn tvm_exit_code_is_none_when_kit_carries_no_tvm() {
        let kit = KitError {
            module: KitModule::Account,
            code: KitErrorCode::AccountIsNotActive,
            message: "nope".into(),
            tvm_error: None,
        };
        let err = ChainError::Kit(kit);
        assert_eq!(err.tvm_exit_code(), None);
    }

    #[test]
    fn tvm_exit_code_handles_direct_client_error() {
        let err = ChainError::Client(tvm_with_exit(102));
        assert_eq!(err.tvm_exit_code(), Some(102));
    }

    #[test]
    fn tvm_exit_code_is_none_when_data_payload_does_not_carry_exit_code() {
        let ce = ClientError::new(601, "query failed", serde_json::json!({"hint": "other"}));
        let err = ChainError::Client(ce);
        assert_eq!(err.tvm_exit_code(), None);
    }

    #[test]
    fn tvm_exit_code_is_none_for_decode() {
        let err = ChainError::Decode("parse failed".into());
        assert_eq!(err.tvm_exit_code(), None);
    }

    #[test]
    fn display_uses_underlying_message() {
        let kit = KitError {
            module: KitModule::Account,
            code: KitErrorCode::None,
            message: "kit boom".into(),
            tvm_error: None,
        };
        assert_eq!(format!("{}", ChainError::Kit(kit)), "kit boom");
        assert_eq!(format!("{}", ChainError::Decode("nope".into())), "nope");
    }
}
