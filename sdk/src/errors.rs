use std::fmt;

use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::tvm_client::error::ClientError;
use serde::Deserialize;
use serde::Serialize;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Serialize, Clone)]
pub struct AppError {
    pub message: String,
    pub details: Option<String>,
    pub module: Option<String>,
    pub error_code: Option<String>,
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvm_error: Option<serde_json::Value>,
}

impl AppError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: None,
            error_code: None,
            module: None,
            kind: None,
            tvm_error: None,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.error_code = Some(code.into());
        self
    }

    pub fn with_module(mut self, module: impl Into<String>) -> Self {
        self.module = Some(module.into());
        self
    }

    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        let context = context.into();
        self.message = format!("{context}: {}", self.message);
        self.details = Some(match self.details.take() {
            Some(details) => format!("{context}; {details}"),
            None => context,
        });
        // tvm_error preserved through context wrapping
        self
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(message: String) -> Self {
        AppError::new(message)
    }
}

impl From<&str> for AppError {
    fn from(message: &str) -> Self {
        AppError::new(message)
    }
}

impl From<hex::FromHexError> for AppError {
    fn from(e: hex::FromHexError) -> Self {
        AppError::new(e.to_string()).with_kind("hex")
    }
}

impl From<base64::DecodeError> for AppError {
    fn from(e: base64::DecodeError) -> Self {
        AppError::new(e.to_string()).with_kind("base64")
    }
}

impl From<std::string::FromUtf8Error> for AppError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        AppError::new(e.to_string()).with_kind("utf8")
    }
}

impl From<std::num::ParseIntError> for AppError {
    fn from(e: std::num::ParseIntError) -> Self {
        AppError::new(e.to_string()).with_kind("parse")
    }
}

impl From<url::ParseError> for AppError {
    fn from(e: url::ParseError) -> Self {
        AppError::new(e.to_string()).with_kind("url")
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(e: tokio::task::JoinError) -> Self {
        AppError::new(e.to_string()).with_kind("tokio")
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::new(e.to_string()).with_kind("http")
    }
}

impl From<pbkdf2::password_hash::Error> for AppError {
    fn from(e: pbkdf2::password_hash::Error) -> Self {
        AppError::new(e.to_string()).with_kind("crypto")
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::new(e.to_string()).with_kind("json")
    }
}

impl From<ClientError> for AppError {
    fn from(e: ClientError) -> Self {
        let code = e.code().to_string();
        let details = format!(
            "tvm_code={}, tvm_details={}, exit_code={:?}",
            e.code(),
            e,
            extract_exit_code(&e)
        );
        let tvm_error = serde_json::to_value(&e).ok();

        let mut err = AppError::new(e.message().to_string())
            .with_kind("tvm")
            .with_module("tvm")
            .with_code(code)
            .with_details(details);
        err.tvm_error = tvm_error;
        err
    }
}

impl From<KitError> for AppError {
    fn from(e: KitError) -> Self {
        let kit_module = module_prefix(e.module).to_string();

        let (kind, module, code) = if e.code != KitErrorCode::None {
            ("kit".to_string(), kit_module, e.code.as_i32().to_string())
        } else if let Some(te) = e.tvm_error.as_ref() {
            if let Some(exit) = extract_exit_code(te) {
                ("tvm_exit".to_string(), kit_module, exit.to_string())
            } else {
                ("tvm".to_string(), "tvm".to_string(), te.code().to_string())
            }
        } else {
            ("kit".to_string(), kit_module, e.code.as_i32().to_string())
        };

        let details =
            match &e.tvm_error {
                Some(te) => {
                    format!(
                "kit_module={:?}, kit_code={:?}, tvm_code={}, exit_code={:?}, tvm_details={}",
                e.module, e.code, te.code(), extract_exit_code(te), te
            )
                }
                None => format!("kit_module={:?}, kit_code={:?}", e.module, e.code),
            };

        let tvm_error = e.tvm_error.as_ref().and_then(|te| serde_json::to_value(te).ok());

        let mut err = AppError::new(e.message)
            .with_kind(kind)
            .with_module(module)
            .with_code(code)
            .with_details(details);
        err.tvm_error = tvm_error;
        err
    }
}

fn module_prefix(module: KitModule) -> &'static str {
    match module {
        // Relocated wrappers tag themselves `External("dex.<contract>")`; the
        // prefix (first dotted segment) is the category, e.g. "dex".
        KitModule::External(name) => name.split('.').next().unwrap_or(name),
        KitModule::MvSystem(_) => "mvsystem",
        KitModule::BkSystem(_) => "bksystem",
        KitModule::AuthService(_) => "authservice",
        KitModule::Token(_) => "token",
        KitModule::Account => "account",
        KitModule::Event => "event",
        KitModule::MvConfig => "mvconfig",
        KitModule::Giver(_) => "giver",
        KitModule::Accumulator(_) => "accumulator",
        KitModule::Exchange(_) => "exchange",
        KitModule::Multisig => "multisig",
        // `KitModule` is `#[non_exhaustive]`.
        _ => "unknown",
    }
}

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

fn extract_exit_code(err: &ClientError) -> Option<u32> {
    let data: TvmData = serde_json::from_value(err.data().clone()).ok()?;
    data.node_error?.extensions?.details?.exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_error_with_tvm_preserves_raw_value() {
        let te = ClientError::new(
            414,
            "test",
            serde_json::json!({
                "node_error": { "extensions": { "details": { "exit_code": 42 } } }
            }),
        );
        let kit = KitError {
            module: KitModule::Account,
            code: KitErrorCode::None,
            message: "Send message".into(),
            tvm_error: Some(te),
        };
        let app: AppError = kit.into();
        assert!(app.tvm_error.is_some(), "tvm_error must survive conversion");
        let v = app.tvm_error.unwrap();
        assert_eq!(v["code"], 414);
    }

    #[test]
    fn kit_error_without_tvm_leaves_field_none() {
        let kit = KitError {
            module: KitModule::Account,
            code: KitErrorCode::AccountIsNotActive,
            message: "nope".into(),
            tvm_error: None,
        };
        let app: AppError = kit.into();
        assert!(app.tvm_error.is_none());
    }

    #[test]
    fn with_context_preserves_tvm_error() {
        let mut app = AppError::new("boom").with_kind("tvm");
        app.tvm_error = Some(serde_json::json!({ "code": 1 }));
        let wrapped = app.with_context("caller");
        assert!(wrapped.tvm_error.is_some(), "with_context must preserve tvm_error");
        assert!(wrapped.message.starts_with("caller: "));
    }

    #[test]
    fn client_error_preserves_raw_value() {
        let ce =
            ClientError::new(601, "query failed", serde_json::json!({"hint": "use other field"}));
        let app: AppError = ce.into();
        assert!(app.tvm_error.is_some());
        assert_eq!(app.tvm_error.as_ref().unwrap()["code"], 601);
        assert_eq!(app.tvm_error.as_ref().unwrap()["data"]["hint"], "use other field");
    }

    #[test]
    fn serde_skips_none_tvm_error() {
        let app = AppError::new("test");
        let json = serde_json::to_value(&app).unwrap();
        assert!(json.get("tvm_error").is_none(), "None tvm_error should be skipped in JSON");
    }

    #[test]
    fn serde_includes_some_tvm_error() {
        let mut app = AppError::new("test");
        app.tvm_error = Some(serde_json::json!({"code": 42}));
        let json = serde_json::to_value(&app).unwrap();
        assert!(json.get("tvm_error").is_some());
        assert_eq!(json["tvm_error"]["code"], 42);
    }
}
