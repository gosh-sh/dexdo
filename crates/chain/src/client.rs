// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Production trader-path methods. Each one constructs the relevant
// kit contract handle on demand and forwards. Deploy/setup methods
// live in `test_helpers.rs` behind the `test-helpers` feature so the
// prod build does not carry them.

use std::sync::Arc;

use ackinacki_kit::contracts::dex::order_book::OrderBook;
use ackinacki_kit::contracts::dex::order_book::ParamsOfGetOrdersByOwner;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSplitFullSet;
use ackinacki_kit::contracts::dex::private_note::PrivateNote;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;

use super::dto::OwnedOrders;
use super::error::ChainResult;

/// Long-lived TVM client + the typed entry points the API, e2e tests,
/// and market-manager reach for.
pub struct Dex {
    pub(crate) ctx: Arc<ClientContext>,
}

impl Dex {
    /// Wrap a caller-owned `ClientContext`. Use this when the caller
    /// already has a context (e.g. `market-manager` shares one across
    /// its trader + admin paths). For the common case of "just give me
    /// a `Dex` for these endpoints", reach for `from_endpoints`.
    pub fn new(ctx: Arc<ClientContext>) -> Self {
        Self { ctx }
    }

    /// Convenience constructor: build a default-config `ClientContext`
    /// from a list of gateway endpoints and wrap it. Fails closed if
    /// the kit cannot initialise its TVM client (config error, OOM,
    /// etc.).
    pub fn from_endpoints(endpoints: Vec<String>) -> ChainResult<Self> {
        let mut config = ClientConfig::default();
        config.network.endpoints = Some(endpoints);
        let ctx = ClientContext::new(config)?;
        Ok(Self::new(Arc::new(ctx)))
    }

    // ── PrivateNote (trader write-path) ──────────────────────────────

    pub async fn place_order(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceOrder,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .place_order(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn place_batch(
        &self,
        pn_address: &str,
        params: ParamsOfPlaceBatch,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .place_batch(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn cancel_order(
        &self,
        pn_address: &str,
        params: ParamsOfCancelOrder,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .cancel_order(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn cancel_batch(
        &self,
        pn_address: &str,
        params: ParamsOfCancelBatch,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .cancel_batch(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Buy a full set of outcome tokens by depositing `collateral` of
    /// the market's quote asset into the PMP. On a market sitting in
    /// `AWAITING_FREEZE`, the first successful call also activates the
    /// OrderBook — same chain entry point as the staging market-manager
    /// uses to seed initial MM liquidity, but signed by the caller's
    /// trading PN. See `docs/tech-specs/write-api.md §POST /api/v1/buyFullSet`.
    pub async fn split_full_set(
        &self,
        pn_address: &str,
        params: ParamsOfSplitFullSet,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .split_full_set(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Best-effort cleanup entry point — same PN method as the
    /// trader-signed `cancel_order`, but signed by the deposit-owner
    /// key. Tests use it to drain leaked coids between runs.
    pub async fn cancel_order_by_client(
        &self,
        pn_address: &str,
        params: ParamsOfCancelOrderByClient,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .cancel_order_by_client(params, signer)
            .await
            .map_err(Into::into)
    }

    // ── OrderBook (read-only, test cleanup polling) ──────────────────

    pub async fn get_orders_by_owner(
        &self,
        ob_address: &str,
        deposit_identifier_hash: String,
    ) -> ChainResult<OwnedOrders> {
        let raw = OrderBook::new(self.ctx.clone(), ob_address)
            .get_orders_by_owner(ParamsOfGetOrdersByOwner { deposit_hash: deposit_identifier_hash })
            .await?;
        OwnedOrders::try_from(raw)
    }
}
