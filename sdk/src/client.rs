use std::sync::Arc;

use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;

use crate::errors::AppResult;
use crate::modules;

/// Configuration passed to `Dex::new`. All fields have empty/`None`
/// defaults so embedding apps can use struct-update syntax:
///
/// ```ignore
/// Dex::new(DexConfig {
///     endpoints: vec!["https://shellnet.ackinacki.org".into()],
///     api_token: Some(env!("BM_API_TOKEN").into()),
///     ..Default::default()
/// })?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct DexConfig {
    /// TVM GraphQL endpoints.
    pub endpoints: Vec<String>,
    /// Bearer token sent as `Authorization: Bearer <token>` on every
    /// TVM GraphQL request. `None` disables auth.
    pub api_token: Option<String>,
    /// Cap on outbound TVM requests per second. `None` disables
    /// rate-limiting.
    pub max_rps: Option<u32>,
}

pub(crate) struct DexContext {
    pub(crate) tvm_client: Arc<ClientContext>,
    pub(crate) rate_limiter: Option<crate::rate_limiter::RateLimiter>,
}

impl DexContext {
    pub(crate) fn new(config: DexConfig) -> AppResult<Self> {
        #[cfg(feature = "wasm")]
        console_error_panic_hook::set_once();

        let DexConfig { endpoints, api_token, max_rps } = config;

        let mut tvm_config = ClientConfig::default();
        tvm_config.network.endpoints = Some(endpoints);
        // Disable tvm_client's internal `query_graphql` re-connect loop —
        // it spins without sleep on multi-endpoint setups and turns a
        // single 502 into a sustained ~60 rps storm against the same BM.
        tvm_config.network.max_reconnect_timeout = 0;
        tvm_config.network.api_token = api_token;
        let client = ClientContext::new(tvm_config).map_err(|e| {
            crate::errors::AppError::from(e).with_context("failed to create tvm client")
        })?;

        Ok(Self {
            tvm_client: Arc::new(client),
            rate_limiter: crate::rate_limiter::RateLimiter::optional(max_rps),
        })
    }

    pub(crate) async fn acquire(&self) {
        crate::rate_limiter::maybe_acquire(self.rate_limiter.as_ref()).await;
    }
}

pub(crate) struct DexClient {
    ctx: DexContext,
}

impl DexClient {
    pub(crate) fn new(config: DexConfig) -> AppResult<Self> {
        Ok(Self { ctx: DexContext::new(config)? })
    }

    pub(crate) fn private_note(&self) -> modules::private_note::PrivateNoteModule<'_> {
        modules::private_note::PrivateNoteModule::new(&self.ctx)
    }

    pub(crate) fn pmp(&self) -> modules::pmp::PmpModule<'_> {
        modules::pmp::PmpModule::new(&self.ctx)
    }

    pub(crate) fn order_book(&self) -> modules::order_book::OrderBookModule<'_> {
        modules::order_book::OrderBookModule::new(&self.ctx)
    }

    pub(crate) fn oracle(&self) -> modules::oracle::OracleModule<'_> {
        modules::oracle::OracleModule::new(&self.ctx)
    }

    pub(crate) fn market(&self) -> modules::market::MarketModule<'_> {
        modules::market::MarketModule::new(&self.ctx)
    }
}
