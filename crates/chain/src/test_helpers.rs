// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Methods compiled only with the `test-helpers` feature — deploy +
// setup entry points plus the read-only PN getters used by e2e tests
// to verify on-chain state. Used by the api crate's e2e integration
// tests, which spawn an ephemeral PMP + OrderBook per run before
// exercising the trader write-path, and poll PN getters to assert
// on-chain effects.
//
// The prod api/infrastructure build leaves `test-helpers` off so
// `Dex` exposes only the trader-path methods in `client.rs`.

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use dodex_contracts::dex::oracle::Oracle;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::OracleEventList;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::oracle_event_list::ResultOfGetEvents;
use dodex_contracts::dex::pmp::ParamsOfSubmitResolve;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::pmp::ResultOfGetDetails as PmpKitDetails;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_contracts::dex::private_note::ResultOfGetDetails as PnDetails;
use dodex_contracts::dex::private_note::ResultOfGetStakes as PnStakesRaw;
use dodex_contracts::dex::root_oracle::ParamsOfDeployOracle;
use dodex_contracts::dex::root_oracle::ParamsOfGetOracleAddress;
use dodex_contracts::dex::root_oracle::RootOracle;
use dodex_contracts::dex::root_pn::ParamsOfGetPmpAddress;
use dodex_contracts::dex::root_pn::RootPn;

use super::client::Dex;
use super::dapp::dex_contract_params;
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
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
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
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .set_stake(params, signer)
            .await
            .map_err(Into::into)
    }

    /// Read-only `PrivateNote.getDetails` accessor. Surfaces
    /// `_balance[tokenType]`, `_busy.busyAddress`, and a handful of
    /// other public PN fields without touching the read-model. The
    /// production API reads PN state via the GraphQL-backed
    /// `pn_state_reader`; this direct accessor exists so e2e tests can
    /// verify on-chain balance moves after `splitFullSet` and poll the
    /// `_busy` window between submission and the `onSplitAccepted`
    /// callback without standing up the GraphQL gateway.
    pub async fn get_private_note_details(&self, pn_address: &str) -> ChainResult<PnDetails> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .get_details()
            .await
            .map_err(Into::into)
    }

    /// Read-only `PrivateNote._stakes` accessor. Returns the raw
    /// `HashMap<String, Value>` keyed by `tvm.hash(eventId,
    /// oracleListHash, tokenType)` (0x-prefixed lowercase hex). Each
    /// value carries the StakeInfo tuple — `amount`, `debtAmount`,
    /// `couponsAmount` plus bookkeeping fields. Sibling of
    /// `get_private_note_details`, scoped to the same e2e niche.
    pub async fn get_private_note_stakes(&self, pn_address: &str) -> ChainResult<PnStakesRaw> {
        PrivateNote::new(self.ctx.clone(), dex_contract_params(pn_address))
            .get_stakes()
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
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
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
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
            .submit_resolve(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_pmp_details(&self, pmp_address: &str) -> ChainResult<PmpDetails> {
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
            .get_details()
            .await
            .map_err(Into::into)
    }

    pub async fn get_order_book_address(&self, pmp_address: &str) -> ChainResult<String> {
        Pmp::new(self.ctx.clone(), dex_contract_params(pmp_address))
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
        RootPn::new(self.ctx.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS))
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
        RootOracle::new(self.ctx.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS))
            .deploy_oracle(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_oracle_address(&self, name: String) -> ChainResult<String> {
        RootOracle::new(self.ctx.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS))
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
        Oracle::new(self.ctx.clone(), dex_contract_params(oracle_address))
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
        OracleEventList::new(self.ctx.clone(), dex_contract_params(event_list_address))
            .add_event(params, signer)
            .await
            .map_err(Into::into)
    }

    pub async fn get_events(&self, event_list_address: &str) -> ChainResult<OracleEvents> {
        OracleEventList::new(self.ctx.clone(), dex_contract_params(event_list_address))
            .get_events()
            .await
            .map_err(Into::into)
    }
}
