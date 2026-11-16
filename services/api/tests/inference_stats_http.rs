// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP contract of GET /api/v1/inference/stats, driven through the production
// router against a stub repository with canned blocks and a pinned clock. How the
// blocks are computed from the tape is covered where the statements live
// (crates/infrastructure); what these tests pin is the wire shape, parameter
// validation, error codes and caching.

mod common;

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use dodex_api::testkit::ClockMs;
use dodex_api::testkit::SharedInferenceRepo;
use dodex_application::InferenceMarketsRequest;
use dodex_application::InferenceOrdersPage;
use dodex_application::InferenceOrdersQuery;
use dodex_application::InferenceReadRepository;
use dodex_domain::DomainError;
use dodex_domain::InferenceDepthSnapshot;
use dodex_domain::InferenceMarketsPage;
use dodex_domain::InferenceStatsDay;
use dodex_domain::InferenceStatsHistory;
use dodex_domain::InferenceStatsPeriod;
use dodex_domain::InferenceStatsPreviousPeriod;
use dodex_domain::InferenceStatsTotals;
use dodex_domain::InferenceStatsVolume24h;
use dodex_domain::Trade;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde_json::json;
use serde_json::Value;

/// 2026-10-23T12:00:00Z: the history block's cutoff.
const HISTORY_AS_OF_MS: i64 = 1_792_756_800_000;
/// One minute later; the volume block's cutoff.
const VOLUME_AS_OF_MS: i64 = 1_792_756_860_000;
/// 2026-10-23T12:01:00Z: the cache clock, on the same UTC date as the history block.
const CLOCK_MS: i64 = 1_792_756_860_000;

fn totals(ticks: &str, quote: &str, fills: u64) -> InferenceStatsTotals {
    InferenceStatsTotals { ticks: ticks.into(), quote: quote.into(), fills }
}

#[derive(Default)]
struct StubStatsRepo {
    history_calls: Mutex<Vec<InferenceStatsPeriod>>,
    volume_calls: Mutex<u32>,
    fail_history: Mutex<bool>,
    fail_volume: Mutex<bool>,
}

impl StubStatsRepo {
    fn set_fail(&self, history: bool, volume: bool) {
        *self.fail_history.lock().unwrap() = history;
        *self.fail_volume.lock().unwrap() = volume;
    }

    fn history_calls(&self) -> Vec<InferenceStatsPeriod> {
        self.history_calls.lock().unwrap().clone()
    }

    fn volume_calls(&self) -> u32 {
        *self.volume_calls.lock().unwrap()
    }
}

fn day_label(offset_from_last: i64) -> String {
    // Days ending 2026-10-23; October has 31 days and the series never reaches
    // before 2026-09-24 here, so plain arithmetic on (month, day) is enough.
    let mut day = 23 - offset_from_last;
    let mut month = 10;
    if day < 1 {
        month = 9;
        day += 30;
    }
    format!("2026-{month:02}-{day:02}")
}

#[async_trait]
impl InferenceReadRepository for StubStatsRepo {
    async fn list_inference_markets(
        &self,
        _: &InferenceMarketsRequest,
    ) -> Result<InferenceMarketsPage, anyhow::Error> {
        unimplemented!()
    }

    async fn get_inference_depth(
        &self,
        _: &str,
        _: u16,
        _: i64,
    ) -> Result<InferenceDepthSnapshot, anyhow::Error> {
        unimplemented!()
    }

    async fn list_inference_orders(
        &self,
        _: &InferenceOrdersQuery,
    ) -> Result<InferenceOrdersPage, anyhow::Error> {
        unimplemented!()
    }

    async fn list_inference_trades(&self, _: &str, _: u16) -> Result<Vec<Trade>, anyhow::Error> {
        unimplemented!()
    }

    async fn inference_stats_history(
        &self,
        period: InferenceStatsPeriod,
        _as_of_ms: i64,
    ) -> Result<InferenceStatsHistory, anyhow::Error> {
        self.history_calls.lock().unwrap().push(period);
        if *self.fail_history.lock().unwrap() {
            return Err(anyhow::anyhow!(DomainError::MarketInconsistent));
        }
        let days: i64 = match period {
            InferenceStatsPeriod::Days14 => 14,
            InferenceStatsPeriod::Days30 => 30,
            InferenceStatsPeriod::All => 2,
        };
        let last = totals("9", "108", 3);
        let per_day = (0..days)
            .rev()
            .map(|offset| InferenceStatsDay {
                day: day_label(offset),
                totals: if offset == 0 { last.clone() } else { totals("0", "0", 0) },
            })
            .collect();
        let previous_period = match period {
            InferenceStatsPeriod::All => None,
            _ => Some(InferenceStatsPreviousPeriod {
                from: "2026-09-26".into(),
                to: "2026-10-10".into(),
                totals: totals("5", "7.5", 2),
            }),
        };
        Ok(InferenceStatsHistory {
            as_of_ms: HISTORY_AS_OF_MS,
            per_day,
            totals: last,
            previous_period,
        })
    }

    async fn inference_stats_volume_24h(
        &self,
        _as_of_ms: i64,
    ) -> Result<InferenceStatsVolume24h, anyhow::Error> {
        *self.volume_calls.lock().unwrap() += 1;
        if *self.fail_volume.lock().unwrap() {
            return Err(anyhow::anyhow!(DomainError::MarketInconsistent));
        }
        Ok(InferenceStatsVolume24h { as_of_ms: VOLUME_AS_OF_MS, totals: totals("12", "132", 4) })
    }
}

async fn service_over(repo: &Arc<StubStatsRepo>) -> Option<Service> {
    let shared: SharedInferenceRepo = repo.clone();
    let clock: ClockMs = Arc::new(|| CLOCK_MS);
    common::setup_with_inference_repo(shared, clock).await.map(|(service, _pool)| service)
}

async fn get(service: &Service, query: &str) -> (Option<StatusCode>, Value) {
    let mut resp =
        TestClient::get(format!("http://test/api/v1/inference/stats{query}")).send(service).await;
    let status = resp.status_code;
    (status, resp.take_json().await.expect("json body"))
}

#[tokio::test]
async fn default_request_is_the_14_day_block() {
    let repo = Arc::new(StubStatsRepo::default());
    let Some(service) = service_over(&repo).await else { return };

    let (status, body) = get(&service, "").await;

    assert_eq!(status, Some(StatusCode::OK));
    assert_eq!(repo.history_calls(), vec![InferenceStatsPeriod::Days14]);
    assert_eq!(body["period"], "14d");
    assert_eq!(body["asOf"], 1792756800000_i64);
    assert_eq!(body["perDay"].as_array().unwrap().len(), 14);
    assert_eq!(
        body["perDay"][0],
        json!({"day": "2026-10-10", "ticks": "0", "quote": "0", "fills": "0"})
    );
    assert_eq!(
        body["perDay"][13],
        json!({"day": "2026-10-23", "ticks": "9", "quote": "108", "fills": "3"})
    );
    assert_eq!(body["totals"], json!({"ticks": "9", "quote": "108", "fills": "3"}));
    assert_eq!(
        body["previousPeriod"],
        json!({"from": "2026-09-26", "to": "2026-10-10",
            "totals": {"ticks": "5", "quote": "7.5", "fills": "2"}})
    );
    assert_eq!(
        body["volume24h"],
        json!({"ticks": "12", "quote": "132", "fills": "4", "asOf": 1792756860000_i64})
    );
}

#[tokio::test]
async fn period_30d_and_all_reach_the_repository() {
    let repo = Arc::new(StubStatsRepo::default());
    let Some(service) = service_over(&repo).await else { return };

    let (status, body) = get(&service, "?period=30d").await;
    assert_eq!(status, Some(StatusCode::OK));
    assert_eq!(body["period"], "30d");
    assert_eq!(body["perDay"].as_array().unwrap().len(), 30);
    assert_eq!(repo.history_calls(), vec![InferenceStatsPeriod::Days30]);

    let (status, body) = get(&service, "?period=all").await;
    assert_eq!(status, Some(StatusCode::OK));
    assert_eq!(body["period"], "all");
    assert_eq!(repo.history_calls(), vec![InferenceStatsPeriod::Days30, InferenceStatsPeriod::All]);
    assert!(body.as_object().unwrap().contains_key("previousPeriod"));
    assert!(body["previousPeriod"].is_null());
}

#[tokio::test]
async fn unsupported_period_is_minus_1130() {
    let repo = Arc::new(StubStatsRepo::default());
    let Some(service) = service_over(&repo).await else { return };

    for q in ["7d", "ALL", "14D", "14d&period=30d"] {
        let (status, body) = get(&service, &format!("?period={q}")).await;
        assert_eq!(status, Some(StatusCode::BAD_REQUEST), "period={q}");
        assert_eq!(body["code"], -1130, "period={q}");
    }
    assert!(repo.history_calls().is_empty());
    assert_eq!(repo.volume_calls(), 0);
}

#[tokio::test]
async fn blank_period_is_minus_1102() {
    let repo = Arc::new(StubStatsRepo::default());
    let Some(service) = service_over(&repo).await else { return };

    let (status, body) = get(&service, "?period=").await;

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(body["code"], -1102);
    assert!(repo.history_calls().is_empty());
    assert_eq!(repo.volume_calls(), 0);
}

#[tokio::test]
async fn a_failed_calculation_is_an_error_never_a_partial_200() {
    // One block failing on its own: the block that succeeded must not leak into the answer.
    // A fresh service per case, so the succeeded block's cache slot does not carry over.
    for (fail_history, fail_volume, succeeded) in
        [(false, true, "perDay"), (true, false, "volume24h")]
    {
        let repo = Arc::new(StubStatsRepo::default());
        let Some(service) = service_over(&repo).await else { return };

        repo.set_fail(fail_history, fail_volume);
        let (status, body) = get(&service, "").await;
        assert_eq!(status, Some(StatusCode::SERVICE_UNAVAILABLE), "{succeeded} succeeded");
        assert_eq!(body["code"], -1500, "{succeeded} succeeded");
        assert!(body.get(succeeded).is_none(), "{succeeded} succeeded");

        repo.set_fail(false, false);
        let (status, _body) = get(&service, "").await;
        assert_eq!(status, Some(StatusCode::OK), "the failure was not cached");
    }
}

#[tokio::test]
async fn second_request_is_served_from_the_cache() {
    let repo = Arc::new(StubStatsRepo::default());
    let Some(service) = service_over(&repo).await else { return };

    let (_, first) = get(&service, "").await;
    let (_, second) = get(&service, "").await;

    assert_eq!(first, second);
    assert_eq!(repo.history_calls().len(), 1);
    assert_eq!(repo.volume_calls(), 1);
}
