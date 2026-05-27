//! Minimal vendored slice of bee-dex's `Dex` facade — just the methods
//! market-manager uses. Lives here so we don't pull bee-dex itself in
//! (private SSH-only repo; staging build hosts won't have keys to
//! authenticate).
//!
//! Each method is a thin delegation to the corresponding kit contract
//! handle (PrivateNote / OracleEventList / Pmp / RootPn / OrderBook).
//! Same method names + argument order as bee-dex so call sites in
//! `main.rs` stay 1:1 with bee-engine's `mint_ob_pool.rs` reference.

use std::sync::Arc;

use ackinacki_kit::contracts::dex::oracle_event_list::OracleEventList;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfAddEvent;
use ackinacki_kit::contracts::dex::oracle_event_list::ResultOfGetEvents;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::pmp::Pmp;
use ackinacki_kit::contracts::dex::pmp::ResultOfGetDetails;
use ackinacki_kit::contracts::dex::private_note::ParamsOfDeployPmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSetStake;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSplitFullSet;
use ackinacki_kit::contracts::dex::private_note::PrivateNote;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPmpAddress;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::Result;

/// Same shape as bee-dex's `PmpDetails`, which is itself just an alias
/// for kit's `ResultOfGetDetails`. Re-exported so call sites read the
/// same as the bee-engine reference.
pub type PmpDetails = ResultOfGetDetails;

/// Same shape as bee-dex's `OracleEvents`. Same `events: HashMap<...>`
/// public field (kit renames its inner `_events` to `events` via serde).
pub type OracleEvents = ResultOfGetEvents;

/// Owns the long-lived TVM client and constructs short-lived kit
/// contract handles per call. Cheap to clone — only `Arc<ClientContext>`.
#[derive(Clone)]
pub struct Dex {
    context: Arc<ClientContext>,
}

impl Dex {
    pub fn new(context: Arc<ClientContext>) -> Self {
        Self { context }
    }

    // ── OracleEventList ──────────────────────────────────────────────

    pub async fn add_event(
        &self,
        event_list_address: &str,
        params: ParamsOfAddEvent,
        signer: Signer,
    ) -> Result<()> {
        OracleEventList::new(self.context.clone(), event_list_address)
            .add_event(params, signer)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("add_event: {e}"))
    }

    pub async fn get_events(&self, event_list_address: &str) -> Result<OracleEvents> {
        OracleEventList::new(self.context.clone(), event_list_address)
            .get_events()
            .await
            .map_err(|e| anyhow::anyhow!("get_events: {e}"))
    }

    // ── PrivateNote (deployer/trader ops) ────────────────────────────

    pub async fn deploy_pmp(
        &self,
        pn_address: &str,
        params: ParamsOfDeployPmp,
        signer: Signer,
    ) -> Result<()> {
        PrivateNote::new(self.context.clone(), pn_address)
            .deploy_pmp(params, signer)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("deploy_pmp: {e}"))
    }

    pub async fn set_stake(
        &self,
        pn_address: &str,
        params: ParamsOfSetStake,
        signer: Signer,
    ) -> Result<()> {
        PrivateNote::new(self.context.clone(), pn_address)
            .set_stake(params, signer)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("set_stake: {e}"))
    }

    pub async fn split_full_set(
        &self,
        pn_address: &str,
        params: ParamsOfSplitFullSet,
        signer: Signer,
    ) -> Result<()> {
        PrivateNote::new(self.context.clone(), pn_address)
            .split_full_set(params, signer)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("split_full_set: {e}"))
    }

    // ── PMP (oracle-signed ops + getters) ────────────────────────────

    pub async fn submit_set_timings(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitSetTimings,
        signer: Signer,
    ) -> Result<()> {
        Pmp::new(self.context.clone(), pmp_address)
            .submit_set_timings(params, signer)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("submit_set_timings: {e}"))
    }

    pub async fn submit_resolve(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitResolve,
        signer: Signer,
    ) -> Result<()> {
        Pmp::new(self.context.clone(), pmp_address)
            .submit_resolve(params, signer)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("submit_resolve: {e}"))
    }

    pub async fn get_pmp_details(&self, pmp_address: &str) -> Result<PmpDetails> {
        Pmp::new(self.context.clone(), pmp_address)
            .get_details()
            .await
            .map_err(|e| anyhow::anyhow!("get_pmp_details: {e}"))
    }

    pub async fn get_order_book_address(&self, pmp_address: &str) -> Result<String> {
        Pmp::new(self.context.clone(), pmp_address)
            .get_order_book_address()
            .await
            .map(|r| r.order_book_address)
            .map_err(|e| anyhow::anyhow!("get_order_book_address: {e}"))
    }

    // ── RootPN (address derivation) ──────────────────────────────────

    pub async fn get_pmp_address(
        &self,
        event_id: String,
        names: Vec<String>,
        token_type: u32,
    ) -> Result<String> {
        RootPn::new_default(self.context.clone())
            .get_pmp_address(ParamsOfGetPmpAddress { event_id, names, token_type })
            .await
            .map(|r| r.pmp_address)
            .map_err(|e| anyhow::anyhow!("get_pmp_address: {e}"))
    }
}
