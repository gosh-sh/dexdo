// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HMAC-SHA256 authentication for USER_DATA / TRADE requests. The public
// HTTP contract — header names, signature formula, error codes — lives
// in `docs/api-spec.md`. The user model and the verification pipeline
// are described in `docs/tech-specs/auth.md`.

use std::sync::Arc;

use async_trait::async_trait;
use dodex_application::AuthContext;
use dodex_application::AuthenticateRequest;
use dodex_application::Authenticator;
use dodex_application::TradingPn;
use dodex_domain::DomainError;
use dodex_domain::Permission;
use dodex_domain::SensitiveBytes;
use hmac::Hmac;
use hmac::Mac;
use sha2::Sha256;
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::config::AuthSection;
use crate::crypto;
use crate::crypto::Kek;

type HmacSha256 = Hmac<Sha256>;

/// Maximum admissible clock skew when the client's timestamp is ahead
/// of the server's `now`. Spec window is `[now - recvWindow, now + 1s]`,
/// so a client up to one second in the future is still in band.
const FORWARD_SKEW_TOLERANCE_MS: i64 = 1_000;

/// Single-row lookup result joining `api_keys` with its parent
/// `accounts` row. Both `disabled_at` predicates are part of the
/// `WHERE` so a missing row already means "no active credential".
#[derive(Debug, sqlx::FromRow)]
struct CredentialRow {
    api_key_id: i64,
    account_id: Uuid,
    api_secret_enc: Vec<u8>,
    permissions: Vec<String>,
    pn_address: String,
    pn_pubkey: String,
    pn_seckey_enc: Vec<u8>,
    pn_dih: String,
}

/// Authenticator backed by Postgres for credential storage and a shared
/// backend KEK for at-rest secret decryption.
pub struct PostgresAuthenticator {
    pool: PgPool,
    kek: Arc<Kek>,
    default_recv_window_ms: u64,
    max_recv_window_ms: u64,
}

impl PostgresAuthenticator {
    pub fn new(pool: PgPool, kek: Arc<Kek>, config: &AuthSection) -> Self {
        Self {
            pool,
            kek,
            default_recv_window_ms: config.default_recv_window_ms,
            max_recv_window_ms: config.max_recv_window_ms,
        }
    }

    async fn load_credential(&self, api_key: &str) -> Result<Option<CredentialRow>, DomainError> {
        // Single JOIN: any combination of unknown api_key, disabled api_key,
        // or disabled parent account collapses into "no row" and -1002
        // without leaking which signal was missing.
        //
        // The enum array is cast to text[] in SQL so we can decode it as
        // Vec<String> without pulling sqlx::Type into the domain crate;
        // numeric(78,0) fields likewise come back as decimal strings,
        // which is exactly the format bee-dex expects later.
        sqlx::query_as::<_, CredentialRow>(
            r#"select
                   ak.id                  as api_key_id,
                   a.id                   as account_id,
                   ak.api_secret_enc,
                   ak.permissions::text[] as permissions,
                   a.pn_address,
                   a.pn_pubkey::text      as pn_pubkey,
                   a.pn_seckey_enc,
                   a.pn_dih::text         as pn_dih
                 from api_keys ak
                 join accounts a on a.id = ak.account_id
                where ak.api_key = $1
                  and ak.disabled_at is null
                  and a.disabled_at is null"#,
        )
        .bind(api_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "api_key lookup failed");
            DomainError::Unexpected
        })
    }
}

#[async_trait]
impl Authenticator for PostgresAuthenticator {
    async fn authenticate(&self, request: AuthenticateRequest) -> Result<AuthContext, DomainError> {
        // 1. Resolve effective recvWindow. Silent clamp to the
        //    operator-configured maximum (itself bounded at the spec
        //    ceiling by config validation) — clients that overshoot get
        //    a tighter window but a coherent error if they're late.
        let recv = request
            .recv_window_ms
            .unwrap_or(self.default_recv_window_ms)
            .min(self.max_recv_window_ms);

        // 2. Active credential + custodied PN in one round-trip.
        let Some(row) = self.load_credential(&request.api_key).await? else {
            tracing::info!(
                api_key = %mask(&request.api_key),
                "auth rejected: no active credential",
            );
            return Err(DomainError::AuthRequired);
        };

        // 3. Check the timing window before computing HMAC: cheap, and
        //    a -1021 is a clearer error than the -1022 the client would
        //    otherwise see (since timestamp is part of the signed
        //    canonicalQueryString — bad clock corrupts the signature).
        if let Err(err) = check_recv_window(request.now_ms, request.timestamp_ms, recv) {
            tracing::info!(
                api_key_id = row.api_key_id,
                // `saturating_sub` keeps the log infallible on
                // adversarial `timestamp = i64::MIN` inputs.
                delta_ms = request.now_ms.saturating_sub(request.timestamp_ms),
                recv_window_ms = recv,
                "auth rejected: timestamp outside recvWindow",
            );
            return Err(err);
        }

        // 4. Decrypt the api_secret. Any failure here is server-side
        //    (rotated KEK or tampered blob), surfaced as -1000 so
        //    operators see "real" errors instead of -1022 spam. The
        //    plaintext is wrapped in `SensitiveBytes` so it gets
        //    zeroized when this stack frame unwinds.
        let secret =
            SensitiveBytes::new(crypto::open(&self.kek, &row.api_secret_enc).map_err(|e| {
                tracing::error!(
                    error = ?e,
                    api_key_id = row.api_key_id,
                    "auth failed: cannot decrypt api_secret_enc",
                );
                DomainError::Unexpected
            })?);

        // 5. Canonicalize the query and verify the signature.
        let canonical = canonical_query_string(&request.raw_query_string);
        if let Err(err) =
            verify_hmac(&canonical, &request.body, secret.as_slice(), &request.signature_hex)
        {
            tracing::warn!(
                api_key_id = row.api_key_id,
                account_id = %row.account_id,
                "auth rejected: signature mismatch",
            );
            return Err(err);
        }

        // 6. Only decrypt the trading PN key after the signature has
        //    proven the request is from the legitimate api_secret holder.
        //    Same -1000 semantics as #4 on decrypt failure. `SensitiveBytes::seckey`
        //    enforces the 32-byte ed25519 invariant at the construction
        //    boundary — a wrong-sized plaintext (corrupted row or schema
        //    drift) fails closed here rather than smuggling a short key
        //    into `BeeDexChainSender` where it would surface as an
        //    unmappable chain reject.
        let pn_seckey_plain = crypto::open(&self.kek, &row.pn_seckey_enc).map_err(|e| {
            tracing::error!(
                error = ?e,
                account_id = %row.account_id,
                "auth failed: cannot decrypt pn_seckey_enc",
            );
            DomainError::Unexpected
        })?;
        let pn_seckey = SensitiveBytes::seckey(pn_seckey_plain).inspect_err(|_err| {
            tracing::error!(
                account_id = %row.account_id,
                "auth failed: pn_seckey plaintext has wrong length",
            );
        })?;

        // 7. Parse permissions. Unknown enum labels are skipped with a
        //    warn so a stale binary still serves USER_DATA / TRADE
        //    correctly after a forward-compatible enum extension. If
        //    *every* label is unknown, the row is now effectively
        //    bricked (every `ctx.require(...)` will return -1002) —
        //    that is the actionable signal for operators, so escalate
        //    to `error!` rather than letting it hide behind a flurry
        //    of per-label `warn!` lines.
        let permissions: Vec<Permission> = row
            .permissions
            .iter()
            .filter_map(|s| match Permission::parse(s) {
                Some(p) => Some(p),
                None => {
                    tracing::warn!(
                        api_key_id = row.api_key_id,
                        unknown = %s,
                        "auth: skipping unknown permission label",
                    );
                    None
                }
            })
            .collect();
        if permissions.is_empty() && !row.permissions.is_empty() {
            tracing::error!(
                api_key_id = row.api_key_id,
                account_id = %row.account_id,
                labels = ?row.permissions,
                "auth: api_key has no recognized permissions after filtering — \
                 binary is likely stale relative to the DB enum",
            );
        }

        // 8. Fire-and-forget last_used_at bump. Observability only —
        //    DB hiccup here must not fail the request.
        let pool = self.pool.clone();
        let api_key_id = row.api_key_id;
        tokio::spawn(async move {
            if let Err(e) = sqlx::query("update api_keys set last_used_at = now() where id = $1")
                .bind(api_key_id)
                .execute(&pool)
                .await
            {
                tracing::warn!(error = ?e, api_key_id, "last_used_at bump failed");
            }
        });

        Ok(AuthContext {
            account_id: row.account_id,
            api_key_id: row.api_key_id,
            trading_pn: TradingPn {
                pn_address: row.pn_address,
                pn_pubkey: row.pn_pubkey,
                pn_dih: row.pn_dih,
                pn_seckey,
            },
            permissions,
        })
    }
}

// --- pure primitives ---

/// Build the canonical query string per `docs/api-spec.md §Signature
/// Formation`: remove the `signature` parameter, sort the remaining
/// pairs lexicographically by key, and rejoin with `&` without
/// re-encoding values. Pairs that arrived without `=` (`?foo&bar=2`)
/// are preserved as bare keys so the client's signed byte string
/// matches the server's exactly.
fn canonical_query_string(raw_query: &str) -> String {
    if raw_query.is_empty() {
        return String::new();
    }

    let mut pairs: Vec<(&str, &str, bool)> = raw_query
        .split('&')
        .filter_map(|kv| match kv.split_once('=') {
            Some(("signature", _)) => None,
            Some((k, v)) => Some((k, v, true)),
            None if kv == "signature" => None,
            None => Some((kv, "", false)),
        })
        .collect();

    pairs.sort_by(|a, b| a.0.cmp(b.0));

    pairs
        .into_iter()
        .map(|(k, v, had_eq)| if had_eq { format!("{k}={v}") } else { k.to_string() })
        .collect::<Vec<_>>()
        .join("&")
}

/// Spec validity window: `[now - recv_window_ms, now + 1s]`. The 1 s
/// forward tolerance absorbs the typical NTP-grade clock skew between
/// well-behaved clients and the server.
fn check_recv_window(
    now_ms: i64,
    timestamp_ms: i64,
    recv_window_ms: u64,
) -> Result<(), DomainError> {
    // A malicious client can send `timestamp = i64::MIN`, where
    // `now_ms - timestamp_ms` would overflow. `checked_sub` returning
    // `None` is unambiguously outside any sane recvWindow.
    let Some(delta) = now_ms.checked_sub(timestamp_ms) else {
        return Err(DomainError::TimestampOutsideRecvWindow);
    };
    let recv = recv_window_ms as i64;
    if delta < -FORWARD_SKEW_TOLERANCE_MS || delta > recv {
        return Err(DomainError::TimestampOutsideRecvWindow);
    }
    Ok(())
}

/// Verify a hex-encoded HMAC-SHA256 over `canonical_query + body` under
/// the given secret. Comparison is constant-time to keep signature
/// timing-side-channel resistance.
fn verify_hmac(
    canonical_query: &str,
    body: &[u8],
    secret: &[u8],
    signature_hex: &str,
) -> Result<(), DomainError> {
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| {
        tracing::error!("HMAC init failed for credential secret");
        DomainError::Unexpected
    })?;
    mac.update(canonical_query.as_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    let provided = match hex::decode(signature_hex.trim()) {
        Ok(b) => b,
        Err(_) => return Err(DomainError::InvalidSignature),
    };

    if provided.len() != expected.len() {
        return Err(DomainError::InvalidSignature);
    }

    if provided.ct_eq(&expected).into() {
        Ok(())
    } else {
        Err(DomainError::InvalidSignature)
    }
}

/// Masked form of an api_key for logging. Shows the first 6 and last 2
/// characters of long keys so operators can correlate without exposing
/// the full credential, and so a 12-char key still has 4 redacted
/// bytes in the middle. A non-ASCII byte sitting at either slice
/// boundary would otherwise panic the worker via the byte-indexing
/// `&api_key[..]` — defensively fall back to `***` for any value that
/// is not safe to slice at those offsets, even though our own
/// generator only ever emits ASCII hex.
fn mask(api_key: &str) -> String {
    let len = api_key.len();
    if len < 12 || !api_key.is_char_boundary(6) || !api_key.is_char_boundary(len - 2) {
        return "***".to_string();
    }
    format!("{}...{}", &api_key[..6], &api_key[len - 2..])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------- canonical_query_string -------------

    #[test]
    fn canonical_empty_query() {
        assert_eq!(canonical_query_string(""), "");
    }

    #[test]
    fn canonical_single_pair_passthrough() {
        assert_eq!(canonical_query_string("timestamp=123"), "timestamp=123");
    }

    #[test]
    fn canonical_sorts_keys_lexicographically() {
        let canon = canonical_query_string("timestamp=123&recvWindow=5000");
        assert_eq!(canon, "recvWindow=5000&timestamp=123");
    }

    #[test]
    fn canonical_removes_signature() {
        let canon = canonical_query_string("timestamp=123&signature=abc&recvWindow=5000");
        assert_eq!(canon, "recvWindow=5000&timestamp=123");
    }

    #[test]
    fn canonical_preserves_value_encoding_as_sent() {
        // Spec: values are taken byte-for-byte from the URL, never
        // re-encoded. The client signed the percent-encoded form, so
        // the server must canonicalize over the same bytes.
        let canon = canonical_query_string("status=TRADING%2CRESOLVING&timestamp=123");
        assert_eq!(canon, "status=TRADING%2CRESOLVING&timestamp=123");
    }

    #[test]
    fn canonical_preserves_duplicate_keys() {
        // Both values stay (no dedup). Sort is stable so a-pairs keep
        // their relative order.
        let canon = canonical_query_string("a=1&a=2&signature=xx");
        assert_eq!(canon, "a=1&a=2");
    }

    #[test]
    fn canonical_preserves_value_less_keys() {
        // `?foo&bar=2`: client signed `foo` (no `=`), server must keep
        // it that way so the canonical byte string matches.
        let canon = canonical_query_string("foo&bar=2&signature=xx");
        assert_eq!(canon, "bar=2&foo");
    }

    #[test]
    fn canonical_only_signature_yields_empty() {
        assert_eq!(canonical_query_string("signature=xx"), "");
    }

    // ------------- check_recv_window -------------

    #[test]
    fn recv_window_within_bounds() {
        assert!(check_recv_window(1_000, 500, 5_000).is_ok());
    }

    #[test]
    fn recv_window_at_past_boundary_is_valid() {
        // delta == recv_window: still in band per spec.
        assert!(check_recv_window(5_000, 0, 5_000).is_ok());
    }

    #[test]
    fn recv_window_one_ms_past_boundary_fails() {
        assert_eq!(
            check_recv_window(5_001, 0, 5_000),
            Err(DomainError::TimestampOutsideRecvWindow),
        );
    }

    #[test]
    fn recv_window_forward_skew_within_tolerance() {
        // Client exactly 1 s ahead: spec window includes `now + 1s`.
        assert!(check_recv_window(0, 1_000, 5_000).is_ok());
    }

    #[test]
    fn recv_window_forward_skew_beyond_tolerance() {
        assert_eq!(
            check_recv_window(0, 1_001, 5_000),
            Err(DomainError::TimestampOutsideRecvWindow),
        );
    }

    #[test]
    fn recv_window_rejects_overflowing_timestamp() {
        // Adversarial input: `now - timestamp` would overflow i64.
        // Must return TimestampOutsideRecvWindow, not panic / wrap.
        assert_eq!(
            check_recv_window(0, i64::MIN, 5_000),
            Err(DomainError::TimestampOutsideRecvWindow),
        );
        assert_eq!(
            check_recv_window(i64::MAX, -1, 5_000),
            Err(DomainError::TimestampOutsideRecvWindow),
        );
    }

    // ------------- verify_hmac -------------

    fn sign(canonical: &str, body: &[u8], secret: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(canonical.as_bytes());
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verify_accepts_correct_signature() {
        let secret = b"test-secret";
        let canon = "recvWindow=5000&timestamp=1710000000000";
        let body = br#"{"side":"BUY"}"#;
        let sig = sign(canon, body, secret);
        assert!(verify_hmac(canon, body, secret, &sig).is_ok());
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let secret = b"test-secret";
        let canon = "timestamp=1";
        let sig = sign(canon, b"original", secret);
        assert_eq!(
            verify_hmac(canon, b"tampered", secret, &sig),
            Err(DomainError::InvalidSignature),
        );
    }

    #[test]
    fn verify_rejects_tampered_canonical() {
        let secret = b"test-secret";
        let body = b"body";
        let sig = sign("timestamp=1", body, secret);
        assert_eq!(
            verify_hmac("timestamp=2", body, secret, &sig),
            Err(DomainError::InvalidSignature),
        );
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let canon = "timestamp=1";
        let body = b"body";
        let sig = sign(canon, body, b"alice-secret");
        assert_eq!(
            verify_hmac(canon, body, b"bob-secret", &sig),
            Err(DomainError::InvalidSignature),
        );
    }

    #[test]
    fn verify_rejects_invalid_hex() {
        let result = verify_hmac("", b"", b"secret", "not-hex");
        assert_eq!(result, Err(DomainError::InvalidSignature));
    }

    #[test]
    fn verify_rejects_short_signature() {
        // 32-byte HMAC-SHA256 expected; 2 hex chars decode to 1 byte.
        let result = verify_hmac("", b"", b"secret", "ab");
        assert_eq!(result, Err(DomainError::InvalidSignature));
    }

    #[test]
    fn verify_handles_empty_body() {
        let secret = b"test-secret";
        let canon = "timestamp=1";
        let sig = sign(canon, b"", secret);
        assert!(verify_hmac(canon, b"", secret, &sig).is_ok());
    }

    #[test]
    fn verify_handles_empty_canonical() {
        let secret = b"test-secret";
        let body = b"body";
        let sig = sign("", body, secret);
        assert!(verify_hmac("", body, secret, &sig).is_ok());
    }

    #[test]
    fn verify_handles_empty_secret() {
        // HMAC accepts an empty key — pathological but not a code path
        // that should crash. The auth pipeline never reaches this case
        // because the credential row always has a non-empty secret, but
        // pinning the primitive's behavior here prevents a footgun.
        let canon = "timestamp=1";
        let body = b"body";
        let sig = sign(canon, body, b"");
        assert!(verify_hmac(canon, body, b"", &sig).is_ok());
    }

    #[test]
    fn verify_tolerates_signature_whitespace() {
        let secret = b"test-secret";
        let canon = "timestamp=1";
        let body = b"body";
        let sig = sign(canon, body, secret);
        let padded = format!("  {sig}\n");
        assert!(verify_hmac(canon, body, secret, &padded).is_ok());
    }

    // ------------- mask -------------

    #[test]
    fn mask_truncates_long_key() {
        let m = mask("dk_live_abcdefghijklmnop0001");
        assert_eq!(m, "dk_liv...01");
    }

    #[test]
    fn mask_still_redacts_middle_of_minimum_length_key() {
        // 12-char keys must still have at least 4 chars redacted in
        // the middle; the earlier implementation showed the entire
        // input verbatim.
        let m = mask("dk_live_1234");
        assert_eq!(m, "dk_liv...34");
    }

    #[test]
    fn mask_redacts_short_key() {
        assert_eq!(mask("short"), "***");
    }

    #[test]
    fn mask_redacts_non_ascii_input() {
        // A malicious client could put non-ASCII bytes into
        // X-DODEX-APIKEY; the masker must not panic on byte-index
        // slicing at a multibyte boundary. Construct a value where
        // byte index 6 splits a multibyte char.
        let key = "abcdeё1234567";
        assert!(!key.is_char_boundary(6), "byte 6 must split the 'ё'");
        assert_eq!(mask(key), "***");
    }
}
