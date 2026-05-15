// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Salvo hoop that enforces the per-request wall-clock budget configured
// via `ServerSection.request_timeout_ms`. Wrapped on every route so a
// handler that hangs past the budget surfaces a 504 / -1007 instead of
// stalling the worker indefinitely.
//
// Why this exists: `bee_dex::Dex::place_order` carries its own
// `place_order_timeout_ms` (chain-side budget). The HTTP hoop is the
// safety net for everything else — gateway hangs, lock contention, a
// future deadlock in the read-model adapter — and the comment on
// `request_timeout_ms` in `config/api.local.yaml` documents the 5 s
// slack over the chain timeout that protects against the "chain
// landed, client got 504, lost clientOrderId" race.

use std::time::Duration;

use dodex_domain::DomainError;
use salvo::prelude::*;
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
    let budget = match depot.obtain::<AppState>() {
        Ok(state) => state.request_timeout,
        Err(_) => Duration::ZERO,
    };

    if budget.is_zero() {
        ctrl.call_next(req, depot, res).await;
        return;
    }

    tokio::select! {
        biased;
        _ = ctrl.call_next(req, depot, res) => {}
        _ = tokio::time::sleep(budget) => {
            // Cancelling `call_next` drops the in-flight handler future.
            // For `POST /api/v1/order` the practical race ("chain
            // submission still in flight after we 504") is bounded by
            // `chain.place_order_timeout_ms` being strictly less than
            // the request budget (api.local.yaml ships 30s vs 35s); the
            // chain-sender's own timeout has already fired by the time
            // this branch runs, so dropping its future does not leave
            // an unanswered `placeOrder` outstanding on the gateway.
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
