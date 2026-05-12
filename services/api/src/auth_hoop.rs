// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Salvo hoop that gates the protected subrouter. It pulls the auth
// envelope (`X-DODEX-APIKEY` header, `timestamp`/`recvWindow`/`signature`
// query parameters) and the raw body out of the request, hands them to
// the application's `Authenticator`, and either injects the resolved
// `AuthContext` into the depot or short-circuits with the spec-mandated
// `{code, msg}` 401 response.
//
// The HTTP contract this enforces lives in `docs/api-spec.md
// §Security Types`; the verification pipeline is described in
// `docs/tech-specs/auth.md §Authentication`.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dodex_application::AuthenticateRequest;
use dodex_domain::DomainError;
use salvo::prelude::*;
use tracing::error;

use crate::ApiError;
use crate::AppState;

const HEADER_API_KEY: &str = "x-dodex-apikey";

/// Authenticates one inbound request. On success the resolved
/// `AuthContext` is placed into the depot for downstream handlers; on
/// failure a 401 response with the spec error body is rendered and the
/// rest of the handler chain is skipped.
#[handler]
pub async fn authenticate(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let state = match depot.obtain::<AppState>() {
        Ok(s) => s.clone(),
        Err(err) => {
            error!(?err, "auth hoop: AppState missing from depot");
            reject(res, ctrl, DomainError::Unexpected);
            return;
        }
    };

    let request = match build_request(req).await {
        Ok(r) => r,
        Err(err) => {
            reject(res, ctrl, err);
            return;
        }
    };

    match state.authenticator.authenticate(request).await {
        Ok(ctx) => {
            depot.inject(ctx);
        }
        Err(err) => reject(res, ctrl, err),
    }
}

/// Pull every input the authenticator needs out of the Salvo request.
///
/// Missing auth-envelope fields surface as `AuthEnvelopeIncomplete`
/// (-1003) — they are part of the security layer, not endpoint
/// parameters, so the `-1102` MissingParameter code is the wrong
/// shape. Splitting the "envelope incomplete" case from the
/// `AuthRequired` (-1002) "credential not recognized" case gives a
/// misconfigured client a distinct, actionable error; both still map
/// to HTTP 401.
async fn build_request(req: &mut Request) -> Result<AuthenticateRequest, DomainError> {
    let api_key = req
        .headers()
        .get(HEADER_API_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(DomainError::AuthEnvelopeIncomplete)?
        .to_string();

    let timestamp_ms =
        req.query::<i64>("timestamp").ok_or(DomainError::AuthEnvelopeIncomplete)?;

    let signature_hex = req
        .query::<String>("signature")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(DomainError::AuthEnvelopeIncomplete)?;

    // `recvWindow` is optional per the spec; absence means "use the
    // server-side default". A non-numeric value is treated as absent
    // rather than 400-ing so a malformed client sees the same -1003
    // it gets for any other envelope issue.
    let recv_window_ms = req.query::<u64>("recvWindow");

    let raw_query_string = req.uri().query().unwrap_or("").to_string();

    // Salvo caches the parsed bytes on the request, so a downstream
    // handler that calls `req.parse_json()` gets the same buffer
    // without a second read off the wire. This preserves the spec's
    // "never re-serialize JSON" property for HMAC verification.
    let body = req.payload().await.map(|b| b.to_vec()).map_err(|err| {
        error!(?err, "auth hoop: failed to read request body");
        DomainError::Unexpected
    })?;

    Ok(AuthenticateRequest {
        api_key,
        timestamp_ms,
        recv_window_ms,
        signature_hex,
        raw_query_string,
        body,
        now_ms: now_ms(),
    })
}

fn reject(res: &mut Response, ctrl: &mut FlowCtrl, err: DomainError) {
    ApiError::from(err).render(res);
    ctrl.skip_rest();
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
