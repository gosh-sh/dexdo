// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Salvo hoop that enforces the per-request wall-clock budget configured
// via `ServerSection.request_timeout_ms`. Wrapped on every route so a
// handler that hangs past the budget surfaces a 504 / -1007 instead of
// stalling the worker indefinitely.
//
// Why this exists: `dodex_chain::Dex::place_order` carries its own
// `place_order_timeout_ms` (chain-side budget). The HTTP hoop is the
// safety net for everything else — gateway hangs, lock contention, a
// future deadlock in the read-model adapter — and the comment on
// `request_timeout_ms` in `config/api.local.yaml` documents the 5 s
// slack over the chain timeout that protects against the "chain
// landed, client got 504, lost clientOrderId" race.

use dodex_domain::DomainError;
use salvo::prelude::*;
use tracing::error;
use tracing::warn;

use crate::ApiError;
use crate::AppState;

/// Wrap the rest of the handler chain in `tokio::time::timeout`. On
/// elapsed: short-circuit with the spec `-1007 / 504` error body and
/// drop the in-flight handler future. A `Duration::ZERO` budget — the
/// implicit default `AppState::new` ships — disables the hoop so tests
/// that don't exercise it stay terse.
#[handler]
pub async fn enforce_request_timeout(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    // Fail closed if state is missing — silent fallback would tie
    // behaviour to router mount order. Matches the same pattern in
    // `auth_hoop::authenticate`.
    let budget = match depot.obtain::<AppState>() {
        Ok(state) => state.request_timeout,
        Err(err) => {
            error!(?err, "request_timeout hoop: AppState missing from depot");
            ApiError::from(DomainError::Unexpected).render(res);
            ctrl.skip_rest();
            return;
        }
    };

    if budget.is_zero() {
        ctrl.call_next(req, depot, res).await;
        return;
    }

    // The "chain landed after we 504" race is bounded by
    // `chain.place_order_timeout_ms` < `request_timeout_ms`: when this
    // branch fires the chain sender's own timeout has already returned.
    tokio::select! {
        biased;
        _ = ctrl.call_next(req, depot, res) => {}
        _ = tokio::time::sleep(budget) => {
            warn!(
                budget_ms = budget.as_millis() as u64,
                method = %req.method(),
                path = %req.uri().path(),
                "request_timeout hoop tripped",
            );
            ApiError::from(DomainError::RequestTimeout).render(res);
            ctrl.skip_rest();
        }
    }
}

#[cfg(test)]
mod tests {
    use salvo::http::StatusCode;
    use salvo::prelude::*;
    use salvo::test::ResponseExt;
    use salvo::test::TestClient;
    use salvo::Service;

    use super::enforce_request_timeout;

    #[handler]
    async fn ok_handler() -> &'static str {
        "ok"
    }

    #[tokio::test]
    async fn depot_miss_renders_500() {
        // Pin the fail-closed behaviour: mounting the hoop without
        // `inject(state)` upstream must surface as 500 / -1000, not
        // the old silent `Duration::ZERO` passthrough that ties
        // request handling to router mount order.
        let router = Router::new().hoop(enforce_request_timeout).goal(ok_handler);
        let service = Service::new(router);

        let mut resp = TestClient::get("http://test/").send(&service).await;
        assert_eq!(resp.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
        let body: serde_json::Value = resp.take_json().await.expect("error body");
        assert_eq!(body["code"], -1000);
    }
}
