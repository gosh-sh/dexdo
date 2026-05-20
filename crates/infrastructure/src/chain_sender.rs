// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Production implementation of `ChainOrderSender` that dispatches
// `PrivateNote.placeOrder`, `PrivateNote.cancelOrder`, and
// `PrivateNote.placeBatch` through `bee_dex::Dex`. The wrapper does
// nothing except translate the application-layer payloads into the
// chain ABI's `ParamsOfPlaceOrder` / `ParamsOfCancelOrder` /
// `ParamsOfPlaceBatch` and forward the call — ABI encoding, signing,
// and gateway transport all live in `bee_dex`.
//
// See `docs/tech-specs/write-api.md §Chain submission` for the layering
// contract this implements.

use std::time::Duration;

use ackinacki_kit::contracts::dex::order_book::OrderBookOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceOrder;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use async_trait::async_trait;
use bee_dex::errors::AppError;
use bee_dex::Dex;
use dodex_application::BatchOrderPayloadItem;
use dodex_application::CancelBatchOrderPayload;
use dodex_application::CancelOrderPayload;
use dodex_application::ChainOrderSender;
use dodex_application::NewBatchOrderPayload;
use dodex_application::NewOrderPayload;
use dodex_domain::DomainError;
use dodex_domain::SensitiveBytes;
use num_bigint::BigUint;
use tokio::time::timeout;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use zeroize::Zeroizing;

pub struct BeeDexChainSender {
    dex: Dex,
    place_order_timeout: Duration,
    cancel_order_timeout: Duration,
    place_batch_timeout: Duration,
    cancel_batch_timeout: Duration,
}

impl BeeDexChainSender {
    /// Connect to the given gateway endpoints. Constructed once at API
    /// startup; production wiring already wraps the sender in
    /// `Arc<dyn ChainOrderSender>` at the `AppState` boundary, so the
    /// inner `Dex` does not need its own `Arc`.
    ///
    /// Each timeout bounds the per-call wait for one chain entry point —
    /// `bee_dex::Dex` itself has no per-request deadline, so a
    /// partitioned or hung gateway would otherwise stall the HTTP
    /// worker indefinitely. Elapsed surfaces as
    /// `DomainError::RequestTimeout` → 504 / -1007 via
    /// `classify_chain_outcome`, matching the retry-with-same-id
    /// contract of the HTTP request_timeout hoop.
    pub fn new(
        endpoints: Vec<String>,
        place_order_timeout: Duration,
        cancel_order_timeout: Duration,
        place_batch_timeout: Duration,
        cancel_batch_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let dex = Dex::new(endpoints)
            .map_err(|err| anyhow::anyhow!("bee_dex::Dex::new failed: {err:?}"))?;
        Ok(Self {
            dex,
            place_order_timeout,
            cancel_order_timeout,
            place_batch_timeout,
            cancel_batch_timeout,
        })
    }
}

#[async_trait]
impl ChainOrderSender for BeeDexChainSender {
    async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError> {
        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        // Defence-in-depth: the application layer caps both at
        // `u64::MAX`, so reaching the error arm means the gate was
        // bypassed. Log loudly and fail closed.
        let amount = payload.amount_raw.parse::<u128>().map_err(|err| {
            error!(?err, raw = %payload.amount_raw, "amount_raw is not uint128");
            DomainError::Unexpected
        })?;
        let client_order_id = payload.client_order_id.parse::<u128>().map_err(|err| {
            error!(?err, raw = %payload.client_order_id, "client_order_id is not uint128");
            DomainError::Unexpected
        })?;

        let params = ParamsOfPlaceOrder {
            event_id: payload.event_id,
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            outcome_id: payload.outcome_id,
            is_buy: payload.is_buy,
            price: payload.price_raw,
            amount,
            flags: payload.flags,
            // MVP: neither field is exposed by api-spec; constants per
            // bee_dex integration test convention. See `docs/tech-specs/
            // write-api.md §Chain submission` for the rationale.
            min_amount: 0,
            epoch_id: 0,
            client_order_id,
        };

        debug!(
            pn = %payload.pn_address,
            event_id = %params.event_id,
            oracle_list_hash = %params.oracle_list_hash,
            token_type = params.token_type,
            outcome_id = params.outcome_id,
            is_buy = params.is_buy,
            price = %params.price,
            amount = params.amount,
            flags = params.flags,
            client_order_id = params.client_order_id,
            "place_order params",
        );

        let call = self.dex.place_order(&payload.pn_address, params, signer);
        let outcome = timeout(self.place_order_timeout, call).await;
        classify_chain_outcome(outcome, self.place_order_timeout.as_millis() as u64, "place_order")
    }

    async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError> {
        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        let params = ParamsOfCancelOrder {
            event_id: payload.event_id,
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            // The application layer caps `order_id` at u64::MAX; the
            // chain ABI is uint128 but the `serde_json::json!` path
            // panics on values above u64::MAX (same constraint as
            // `clientOrderId`). Upcast is loss-less.
            order_id: payload.order_id as u128,
        };

        debug!(
            pn = %payload.pn_address,
            event_id = %params.event_id,
            oracle_list_hash = %params.oracle_list_hash,
            token_type = params.token_type,
            order_id = params.order_id,
            "cancel_order params",
        );

        let call = self.dex.cancel_order(&payload.pn_address, params, signer);
        let outcome = timeout(self.cancel_order_timeout, call).await;
        classify_chain_outcome(
            outcome,
            self.cancel_order_timeout.as_millis() as u64,
            "cancel_order",
        )
    }

    async fn submit_batch_order(&self, payload: NewBatchOrderPayload) -> Result<(), DomainError> {
        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        // Consume `payload.orders` by value so per-item `price_raw`
        // moves into the chain payload instead of being cloned.
        let pn_address = payload.pn_address;
        let mut orders = Vec::with_capacity(payload.orders.len());
        for (item_index, item) in payload.orders.into_iter().enumerate() {
            orders.push(encode_batch_item(item, item_index)?);
        }

        let params = ParamsOfPlaceBatch {
            event_id: payload.event_id,
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            orders,
        };

        // `client_order_ids` is the audit trail when a place_batch
        // never returns (`RequestTimeout`) or the gateway raises an
        // unmapped error: without it, ops cannot reconcile which coids
        // the chain may or may not have committed against the PN's
        // `_clientOrderIds` map. `classify_chain_outcome` does not
        // carry coids on its error/timeout logs, so this line is the
        // only place they appear — emit at `info!` so the production
        // default filter keeps it. Per-order price/amount are not
        // logged: they would balloon the line for a large batch and
        // the chain encodes them in the external message recoverable
        // from the gateway side anyway.
        let client_order_ids: Vec<u128> = params.orders.iter().map(|o| o.client_order_id).collect();
        info!(
            entry_point = "place_batch",
            pn = %pn_address,
            event_id = %params.event_id,
            oracle_list_hash = %params.oracle_list_hash,
            token_type = params.token_type,
            order_count = params.orders.len(),
            ?client_order_ids,
            "submitting place_batch",
        );

        let call = self.dex.place_batch(&pn_address, params, signer);
        let outcome = timeout(self.place_batch_timeout, call).await;
        classify_chain_outcome(outcome, self.place_batch_timeout.as_millis() as u64, "place_batch")
    }

    async fn cancel_batch_order(
        &self,
        payload: CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        // The application layer caps each `order_id` at u64::MAX; the
        // chain ABI is uint128[]. Upcast is loss-less. Same constraint
        // as the single-order cancel path.
        let order_ids: Vec<u128> = payload.order_ids.iter().map(|&id| id as u128).collect();

        let params = ParamsOfCancelBatch {
            event_id: payload.event_id,
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            order_ids,
        };

        debug!(
            pn = %payload.pn_address,
            event_id = %params.event_id,
            oracle_list_hash = %params.oracle_list_hash,
            token_type = params.token_type,
            order_count = params.order_ids.len(),
            "cancel_batch params",
        );

        let call = self.dex.cancel_batch(&payload.pn_address, params, signer);
        let outcome = timeout(self.cancel_batch_timeout, call).await;
        classify_chain_outcome(
            outcome,
            self.cancel_batch_timeout.as_millis() as u64,
            "cancel_batch",
        )
    }
}

/// Translate one application-layer `BatchOrderPayloadItem` into the
/// chain `OrderBookOrder` tuple. The `amount_raw` / `client_order_id`
/// parses are defence-in-depth: `validate_and_encode_order_item`
/// (application crate) already enforces both stay within `u64::MAX`
/// per the `bee_dex` / `serde_json` ceiling, so reaching the error
/// arm means that upstream invariant was bypassed.
fn encode_batch_item(
    item: BatchOrderPayloadItem,
    item_index: usize,
) -> Result<OrderBookOrder, DomainError> {
    let amount = item.amount_raw.parse::<u128>().map_err(|err| {
        error!(item_index, ?err, raw = %item.amount_raw, "amount_raw is not uint128");
        DomainError::Unexpected
    })?;
    let client_order_id = item.client_order_id.parse::<u128>().map_err(|err| {
        error!(item_index, ?err, raw = %item.client_order_id, "client_order_id is not uint128");
        DomainError::Unexpected
    })?;
    Ok(OrderBookOrder {
        outcome_id: item.outcome_id,
        is_buy: item.is_buy,
        flags: item.flags,
        price: item.price_raw,
        amount,
        // Matches the single-order path — neither field is exposed by
        // api-spec and both are pinned to 0 at the chain boundary.
        min_amount: 0,
        epoch_id: 0,
        client_order_id,
    })
}

/// Build a `Signer::Keys` from the application-layer (`pn_pubkey`,
/// `pn_seckey`) pair. Shared by `submit_order` and `cancel_order`.
///
/// `tvm_client::crypto::KeyPair` takes hex strings (matches the
/// `owner_public_key_hex` / `owner_secret_key_hex` shape that bee_dex
/// integration tests use). Our DB stores the pubkey as decimal
/// `numeric(78,0)` and the seckey as encrypted bytes — both get
/// converted at the boundary, not deeper.
///
/// Defence-in-depth: `KeyPair.secret` is `String` upstream with no
/// `Zeroize` impl, so the copy handed to `Signer::Keys` freed-unzeroed
/// when the signer drops after the chain call returns. We can't close
/// that path without changes in `ackinacki-kit::tvm_client::crypto`.
/// What we DO control is keeping the pre-clone copy under
/// `Zeroizing<String>` here — once this helper returns, the only
/// residue is the upstream clone (briefly, until Signer drops). The
/// caller's `SensitiveBytes` still zeroes on drop per `crates/domain`,
/// and nothing in this module logs the hex.
fn build_signer(pn_pubkey: &str, pn_seckey: &SensitiveBytes) -> Result<Signer, DomainError> {
    let secret_hex = Zeroizing::new(hex::encode(pn_seckey.as_slice()));
    let public = decimal_uint256_to_hex(pn_pubkey).map_err(|_| {
        // `accounts.pn_pubkey` is operator-seeded data, not a runtime
        // input — the seed/migration writes it as a decimal uint256.
        // A failure here means the row was corrupted post-write
        // (manual edit, partial restore, schema drift), so the right
        // surface is `MarketInconsistent` (503 / -1500): retrying
        // won't help, but it's a service-state issue, not the generic
        // 500 the caller can do nothing about. Log without the
        // decimal value — pn_pubkey is public per the ACK protocol,
        // but the raw string is recoverable from the `accounts` row
        // if ops actually needs it.
        error!("trading PN pubkey is not a valid uint256 decimal");
        DomainError::MarketInconsistent
    })?;
    // Pass a fresh clone to `Signer` — the upstream `KeyPair.secret`
    // is `String` without `Zeroize`. Our `Zeroizing<String>` local
    // scrubs on drop regardless of how we exit.
    let keys = KeyPair { public, secret: (*secret_hex).clone() };
    Ok(Signer::Keys { keys })
}

/// Translate the `(timeout, chain-call)` result chain into the typed
/// `DomainError` surface. Lifted out of the per-method dispatchers so
/// the three arms can be exercised by unit tests against real
/// `bee_dex::AppError` values — the HTTP integration tests fake
/// `ChainOrderSender` entirely and would not catch a future refactor
/// that breaks the wiring between `map_bee_dex_error` and the chain
/// dispatch. `entry_point` is included in the timeout / unmapped-error
/// logs so ops can disambiguate place / cancel / place_batch without
/// correlation.
fn classify_chain_outcome<T>(
    outcome: Result<Result<T, AppError>, tokio::time::error::Elapsed>,
    timeout_ms: u64,
    entry_point: &'static str,
) -> Result<(), DomainError> {
    match outcome {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(app_err)) => Err(map_bee_dex_error(&app_err, entry_point)),
        Err(_elapsed) => {
            // Gateway did not respond within the configured budget
            // (`chain.place_order_timeout_ms`,
            // `chain.cancel_order_timeout_ms`,
            // `chain.place_batch_timeout_ms`). Most often a
            // network partition or a gateway-side deadlock. Surface as
            // `RequestTimeout` (504 / -1007) so the client receives the
            // same "retry with the same id" contract as the HTTP
            // request-timeout hoop — see
            // `docs/tech-specs/write-api.md §Failure surface`.
            error!(
                entry_point,
                timeout_ms, "bee_dex chain call timed out before gateway responded",
            );
            Err(DomainError::RequestTimeout)
        }
    }
}

/// Translate a `bee_dex` `AppError` into a typed `DomainError`. The
/// underlying `bee_dex::Dex::{place_order, cancel_order}` calls already
/// wait for the chain to finish executing the corresponding PN method,
/// so a `tvm_exit` error here carries the exact `require(...)` exit
/// code the chain raised. Mapping it to a specific `DomainError` lets
/// the HTTP caller distinguish "balance insufficient" from "PN busy"
/// from "transport blew up" without polling `/api/v1/orders` for
/// absence.
///
/// Unrecognized chain codes and non-`tvm_exit` failures (gateway
/// disconnect, malformed reply, etc.) collapse to `Unexpected` so a
/// new chain error variant cannot silently surface as a misleading
/// 400. See [`docs/tech-specs/write-api.md` §Failure surface] for the
/// per-code contract this enforces.
fn map_bee_dex_error(err: &AppError, entry_point: &'static str) -> DomainError {
    if err.kind.as_deref() == Some("tvm_exit")
        && let Some(code_str) = err.error_code.as_deref()
        && let Ok(code) = code_str.parse::<u16>()
        && let Some(mapped) = map_tvm_exit_code(code)
    {
        warn!(
            entry_point,
            exit_code = code,
            err_message = %err.message,
            err_details = ?err.details,
            err_module = ?err.module,
            err_kind = ?err.kind,
            tvm_error = ?err.tvm_error,
            domain_error = ?mapped,
            "chain rejected request",
        );
        return mapped;
    }
    // Unmapped path: full diagnostic at error level since this is
    // either a transport failure or a chain code we have not learned
    // to handle yet.
    error!(entry_point, ?err, "bee_dex chain call failed (unmapped)");
    DomainError::Unexpected
}

/// `PrivateNote.placeOrder` exit codes from
/// `contracts/modifiers/errors.sol`. Only the codes the trading path
/// can plausibly raise are mapped; everything else returns `None` so
/// `map_bee_dex_error` can fall through to `Unexpected` (and log the
/// raw error for later triage).
fn map_tvm_exit_code(code: u16) -> Option<DomainError> {
    match code {
        // ERR_LOW_VALUE: insufficient `_balance[tokenType]` for BUY,
        // insufficient `stake.amount[outcomeId]` for SELL, or
        // `amount <= 0` (the last is normally caught by our local
        // precision check, but the contract guards it too).
        102 => Some(DomainError::OrderValidationFailed),
        // ERR_NOTE_BUSY: another `placeOrder` is in flight for this PN.
        // Distinct retry semantics — 429 / -2014 instead of -2010.
        121 => Some(DomainError::OrderPnBusy),
        // ERR_INVALID_PARAMS: applies to both `placeOrder` and
        // `placeBatch`. The PN guards two client-shape invariants
        // under this code:
        //   1. `clientOrderId` collision — the chain holds a single
        //      per-PN `_clientOrderIds` map covering BOTH still-live
        //      coids from earlier `placeOrder`s and sentinels for an
        //      in-flight `placeBatch`. So 129 fires for intra-batch
        //      duplicates AND for a coid the caller already used in
        //      a prior placement that has not closed yet.
        //   2. `minAmount != 0` on any MARKET order (the chain's
        //      MARKET path does not support `minAmount`). Our wire
        //      payload always sends `minAmount = 0` for every order,
        //      so practically only (1) reaches us.
        129 => Some(DomainError::InvalidParameter),
        // ERR_INVALID_OUTCOME_ID: the `outcomeId` we pulled from
        // `market_outcomes` does not exist on the on-chain PMP. That
        // is a read-model integrity bug, not a client bug — fail
        // closed with 503 so ops sees it.
        130 => Some(DomainError::MarketInconsistent),
        // ERR_STAKE_NOT_EXISTS: SELL against a PN that never ran
        // `splitFullSet` for this `(eventId, oracleListHash,
        // tokenType)`. Genuine "order would fail validation".
        142 => Some(DomainError::OrderValidationFailed),
        // ERR_DEBT_NON_ZERO / ERR_INVALID_STATE: trading PN is in a
        // state that disallows any new `placeOrder` (has pending
        // debt, has been withdrawn). Surface as the validation error
        // family — operator action is needed on the PN itself.
        150 | 151 => Some(DomainError::OrderValidationFailed),
        // ERR_ORDER_TOO_SMALL: notional below the on-chain minimum
        // for this tokenType. Our local validation pre-checks against
        // `market_outcomes.min_notional`, so this implies the chain
        // and the read-model disagree on the minimum — still surface
        // as a validation error for the client.
        160 => Some(DomainError::OrderValidationFailed),
        // ERR_BATCH_TOO_LARGE / ERR_EMPTY_BATCH: chain-side
        // defence-in-depth against the same length checks the use
        // case pre-applies (`orders.len()` ∈ [1, `max_batch_size`]).
        // Reaching them means the local guard was bypassed — i.e.
        // the read-model's `max_batch_size` drifted from the on-chain
        // ceiling — which is a server-state inconsistency, not a
        // client error. Surface as `MarketInconsistent` (503 / -1500)
        // so ops sees it instead of confusing the client with a
        // -1130 they cannot act on.
        161 | 162 => Some(DomainError::MarketInconsistent),
        // ERR_AMOUNT_NOT_LOT_MULTIPLE / ERR_PRICE_NOT_TICK_MULTIPLE:
        // amount or price not aligned to chain lattice. Our local
        // precision checks against `step_size` / `tick_size` should
        // catch these first; if they slip through, the read-model is
        // misaligned with the chain. Map to `PrecisionExceeded` so
        // the client gets a -1111 they can act on.
        163 | 164 => Some(DomainError::PrecisionExceeded),
        // ERR_NOTIONAL_OVERFLOW: `price * amount` overflowed uint256
        // inside `placeBatch`. Treat as a doomed validation: the
        // client must scale either side down.
        168 => Some(DomainError::OrderValidationFailed),
        _ => None,
    }
}

/// Convert a decimal-encoded `uint256` (the format
/// `accounts.pn_pubkey` stores) to a lower-case hex string padded to
/// 64 characters (32 bytes). Matches the `owner_public_key_hex` format
/// `tvm_client::crypto::KeyPair.public` expects.
fn decimal_uint256_to_hex(dec: &str) -> anyhow::Result<String> {
    let n = BigUint::parse_bytes(dec.as_bytes(), 10)
        .ok_or_else(|| anyhow::anyhow!("invalid decimal uint256: {dec}"))?;
    let hex = n.to_str_radix(16);
    Ok(format!("{:0>64}", hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_uint256_to_hex_pads_to_32_bytes() {
        // BigUint drops leading zeros; the chain ABI expects a fixed
        // 64-char hex regardless of the value's magnitude. Padding to
        // 64 keeps signatures consistent for keys with small numerical
        // values (small public keys are rare but possible in test
        // fixtures).
        let small = decimal_uint256_to_hex("1").unwrap();
        assert_eq!(small.len(), 64);
        assert_eq!(&small, "0000000000000000000000000000000000000000000000000000000000000001");

        let zero = decimal_uint256_to_hex("0").unwrap();
        assert_eq!(zero.len(), 64);
        assert!(zero.chars().all(|c| c == '0'));
    }

    #[test]
    fn decimal_uint256_to_hex_round_trips_known_keys() {
        // Round-trip a realistic 32-byte pubkey: hex → decimal → hex,
        // with the decimal half going through `decimal_uint256_to_hex`.
        // Catches any padding/endianness drift against real fixture
        // values without hard-coding a fragile decimal literal.
        let hex_in = "926d6c4c3d034f8e7cdf1d62d7ee6b49e1d920934a0d537b393a837cfe88a914";
        let n = BigUint::parse_bytes(hex_in.as_bytes(), 16).unwrap();
        let dec = n.to_str_radix(10);
        let hex_out = decimal_uint256_to_hex(&dec).unwrap();
        assert_eq!(hex_out, hex_in);
    }

    #[test]
    fn decimal_uint256_to_hex_rejects_non_decimal() {
        assert!(decimal_uint256_to_hex("0xff").is_err());
        assert!(decimal_uint256_to_hex("abc").is_err());
        assert!(decimal_uint256_to_hex("").is_err());
    }

    fn tvm_exit(code: u16) -> AppError {
        AppError {
            message: "Send message".into(),
            details: None,
            module: Some("dex".into()),
            error_code: Some(code.to_string()),
            kind: Some("tvm_exit".into()),
            tvm_error: None,
        }
    }

    #[test]
    fn maps_known_pn_validation_codes() {
        // The full table from `map_tvm_exit_code` — locking each row
        // down so a future "small refactor" cannot silently remap a
        // known reject onto the wrong HTTP class. Codes come from
        // `contracts/modifiers/errors.sol`.
        assert_eq!(
            map_bee_dex_error(&tvm_exit(102), "place_order"),
            DomainError::OrderValidationFailed
        );
        assert_eq!(map_bee_dex_error(&tvm_exit(121), "place_order"), DomainError::OrderPnBusy);
        assert_eq!(map_bee_dex_error(&tvm_exit(129), "place_batch"), DomainError::InvalidParameter);
        assert_eq!(
            map_bee_dex_error(&tvm_exit(130), "place_order"),
            DomainError::MarketInconsistent
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(142), "place_order"),
            DomainError::OrderValidationFailed
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(150), "place_order"),
            DomainError::OrderValidationFailed
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(151), "place_order"),
            DomainError::OrderValidationFailed
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(160), "place_order"),
            DomainError::OrderValidationFailed
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(161), "place_batch"),
            DomainError::MarketInconsistent
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(162), "place_batch"),
            DomainError::MarketInconsistent
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(163), "place_order"),
            DomainError::PrecisionExceeded
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(164), "place_order"),
            DomainError::PrecisionExceeded
        );
        assert_eq!(
            map_bee_dex_error(&tvm_exit(168), "place_batch"),
            DomainError::OrderValidationFailed
        );
    }

    #[test]
    fn unknown_exit_code_falls_through_to_unexpected() {
        // A chain code we haven't learned to handle MUST NOT degrade
        // to a misleading 400 — the operator needs to see it in logs
        // and decide whether to extend the mapping.
        assert_eq!(map_bee_dex_error(&tvm_exit(9999), "place_order"), DomainError::Unexpected);
    }

    #[test]
    fn non_tvm_error_kinds_map_to_unexpected() {
        // Transport / gateway / decode failures have a different
        // `kind` (or none). We must not interpret an `error_code` like
        // "404" outside the `tvm_exit` context as a TVM exit code.
        let mut err = tvm_exit(102);
        err.kind = Some("transport".into());
        assert_eq!(map_bee_dex_error(&err, "place_order"), DomainError::Unexpected);

        err.kind = None;
        assert_eq!(map_bee_dex_error(&err, "place_order"), DomainError::Unexpected);
    }

    #[test]
    fn malformed_error_code_string_is_unexpected() {
        // `error_code` is `Option<String>` so a non-numeric value is
        // possible. Make sure we don't panic and fall through cleanly.
        let mut err = tvm_exit(0);
        err.error_code = Some("not-a-number".into());
        assert_eq!(map_bee_dex_error(&err, "place_order"), DomainError::Unexpected);
    }

    #[test]
    fn classify_chain_outcome_ok_passes_through() {
        let outcome: Result<Result<(), AppError>, tokio::time::error::Elapsed> = Ok(Ok(()));
        assert!(classify_chain_outcome(outcome, 1_000, "place_order").is_ok());
    }

    #[test]
    fn classify_chain_outcome_pipes_app_error_through_map_bee_dex_error() {
        // Regression: HTTP-level tests fake `ChainOrderSender` whole,
        // which means a future refactor that drops the
        // `map_bee_dex_error` call inside the chain dispatchers would
        // not fail any HTTP test. This pin exercises the wiring
        // end-to-end for the headline chain-side rejects.
        let cases = [
            (102u16, DomainError::OrderValidationFailed), // ERR_LOW_VALUE
            (121, DomainError::OrderPnBusy),              // ERR_NOTE_BUSY
            (129, DomainError::InvalidParameter),         // ERR_INVALID_PARAMS (batch-coid dupe)
            (130, DomainError::MarketInconsistent),       // ERR_INVALID_OUTCOME_ID
            (161, DomainError::MarketInconsistent),       // ERR_BATCH_TOO_LARGE
            (162, DomainError::MarketInconsistent),       // ERR_EMPTY_BATCH
            (163, DomainError::PrecisionExceeded),        // ERR_AMOUNT_NOT_LOT_MULTIPLE
            (168, DomainError::OrderValidationFailed),    // ERR_NOTIONAL_OVERFLOW
        ];
        for (code, expected) in cases {
            let outcome: Result<Result<(), AppError>, tokio::time::error::Elapsed> =
                Ok(Err(tvm_exit(code)));
            let result = classify_chain_outcome(outcome, 1_000, "place_order");
            assert_eq!(result, Err(expected), "tvm_exit({code}) mismapped");
        }
    }

    #[tokio::test]
    async fn classify_chain_outcome_elapsed_maps_to_request_timeout() {
        // `tokio::time::error::Elapsed` is non-constructible from
        // user code, so we obtain a real one by running a 0-duration
        // timeout against a future that never completes. Pins the
        // contract: gateway-side hang surfaces as `RequestTimeout`
        // (504 / -1007), same retry-with-same-id semantics as the
        // HTTP-layer request_timeout hoop.
        let elapsed = tokio::time::timeout(
            std::time::Duration::from_millis(0),
            std::future::pending::<Result<(), AppError>>(),
        )
        .await;
        assert_eq!(
            classify_chain_outcome(elapsed, 30_000, "place_order"),
            Err(DomainError::RequestTimeout),
        );
    }

    #[tokio::test]
    async fn malformed_pn_pubkey_surfaces_as_market_inconsistent() {
        // Pin: `pn_pubkey` decode runs before any network I/O, so a
        // bogus endpoint is fine. Timeout is generous (100ms) to keep
        // the test stable on loaded CI — a future refactor that moves
        // the gateway call before decode would surface here as
        // `RequestTimeout`, flagging the regression.
        let sender = BeeDexChainSender::new(
            vec!["http://invalid.example.test".to_string()],
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        )
        .expect("BeeDexChainSender::new");

        let payload = NewOrderPayload {
            pn_address: "0:pn".into(),
            pn_pubkey: "not-a-decimal".into(),
            pn_seckey: dodex_domain::SensitiveBytes::new(vec![0u8; 32]),
            event_id: "1".into(),
            oracle_list_hash: "1".into(),
            token_type: 3,
            outcome_id: 1,
            is_buy: true,
            price_raw: "615".into(),
            amount_raw: "1500000".into(),
            flags: 0,
            client_order_id: "42".into(),
        };

        let err = sender.submit_order(payload).await.expect_err("decode must fail closed");
        assert_eq!(err, DomainError::MarketInconsistent);
    }
}
