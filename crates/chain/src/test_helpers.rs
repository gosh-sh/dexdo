// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Deploy + setup methods — only compiled when the `test-helpers`
// feature is enabled. Used by:
//
//   * the api crate's e2e integration tests, which spawn an
//     ephemeral PMP + OrderBook per run before exercising the
//     trader write-path;
//   * `market-manager`, the staging tool that deploys real markets
//     on shellnet.
//
// The prod api/infrastructure build leaves `test-helpers` off so
// `Dex` exposes only the trader-path methods in `client.rs`.

use ackinacki_kit::contracts::dex::oracle::Oracle;
use ackinacki_kit::contracts::dex::oracle::ParamsOfGetEventListAddress;
use ackinacki_kit::contracts::dex::oracle_event_list::OracleEventList;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfAddEvent;
use ackinacki_kit::contracts::dex::oracle_event_list::ResultOfGetEvents;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::pmp::Pmp;
use ackinacki_kit::contracts::dex::pmp::ResultOfGetDetails as PmpKitDetails;
use ackinacki_kit::contracts::dex::private_note::ParamsOfDeployPmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSetStake;
use ackinacki_kit::contracts::dex::private_note::PrivateNote;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfDeployOracle;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfGetOracleAddress;
use ackinacki_kit::contracts::dex::root_oracle::RootOracle;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPmpAddress;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;

use super::client::Dex;
use super::error::ChainResult;

/// Shape returned by `Pmp.getDetails`. Aliased so call sites don't
/// have to reach for the kit's three-deep contract path.
pub type PmpDetails = PmpKitDetails;

/// Shape returned by `OracleEventList.getEvents`. `.events` is a
/// `HashMap<String, serde_json::Value>` keyed by event id.
pub type OracleEvents = ResultOfGetEvents;

impl Dex {
    // ── PrivateNote (deployer-side) ──────────────────────────────────

    pub async fn deploy_pmp(
        &self,
        pn_address: &str,
        params: ParamsOfDeployPmp,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .deploy_pmp(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn set_stake(
        &self,
        pn_address: &str,
        params: ParamsOfSetStake,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        PrivateNote::new(self.ctx.clone(), pn_address)
            .set_stake(params, signer)
            .await
            .map_err(Into::into)
    }

    // ── PMP (oracle-signed ops + getters) ────────────────────────────

    pub async fn submit_set_timings(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitSetTimings,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        Pmp::new(self.ctx.clone(), pmp_address)
            .submit_set_timings(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn submit_resolve(
        &self,
        pmp_address: &str,
        params: ParamsOfSubmitResolve,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        Pmp::new(self.ctx.clone(), pmp_address)
            .submit_resolve(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_pmp_details(&self, pmp_address: &str) -> ChainResult<PmpDetails> {
        Pmp::new(self.ctx.clone(), pmp_address).get_details().await.map_err(Into::into)
    }

    pub async fn get_order_book_address(&self, pmp_address: &str) -> ChainResult<String> {
        Pmp::new(self.ctx.clone(), pmp_address)
            .get_order_book_address()
            .await
            .map(|r| r.order_book_address)
            .map_err(Into::into)
    }

    // ── RootPN (address derivation) ──────────────────────────────────

    pub async fn get_pmp_address(
        &self,
        event_id: String,
        names: Vec<String>,
        token_type: u32,
    ) -> ChainResult<String> {
        RootPn::new_default(self.ctx.clone())
            .get_pmp_address(ParamsOfGetPmpAddress { event_id, names, token_type })
            .await
            .map(|r| r.pmp_address)
            .map_err(Into::into)
    }

    // ── RootOracle ───────────────────────────────────────────────────

    pub async fn deploy_oracle(
        &self,
        params: ParamsOfDeployOracle,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        RootOracle::new_default(self.ctx.clone())
            .deploy_oracle(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_oracle_address(&self, name: String) -> ChainResult<String> {
        RootOracle::new_default(self.ctx.clone())
            .get_oracle_address(ParamsOfGetOracleAddress { name })
            .await
            .map(|r| r.oracle_address)
            .map_err(Into::into)
    }

    // ── Oracle + OracleEventList ─────────────────────────────────────

    pub async fn get_event_list_address(
        &self,
        oracle_address: &str,
        params: ParamsOfGetEventListAddress,
    ) -> ChainResult<String> {
        Oracle::new(self.ctx.clone(), oracle_address)
            .get_event_list_address(params)
            .await
            .map(|r| r.address)
            .map_err(Into::into)
    }

    pub async fn add_event(
        &self,
        event_list_address: &str,
        params: ParamsOfAddEvent,
        signer: Signer,
    ) -> ChainResult<ResultOfSendMessage> {
        OracleEventList::new(self.ctx.clone(), event_list_address)
            .add_event(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_events(&self, event_list_address: &str) -> ChainResult<OracleEvents> {
        OracleEventList::new(self.ctx.clone(), event_list_address)
            .get_events()
            .await
            .map_err(Into::into)
    }
}
