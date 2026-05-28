// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Production implementation of `ChainOrderSender` that dispatches
// `PrivateNote.placeOrder`, `PrivateNote.cancelOrder`,
// `PrivateNote.placeBatch`, and `PrivateNote.cancelBatch` through the
// `dodex_chain::Dex` facade. The wrapper does nothing except translate
// the application-layer payloads into the chain ABI's
// `ParamsOfPlaceOrder` / `ParamsOfCancelOrder` / `ParamsOfPlaceBatch` /
// `ParamsOfCancelBatch` and forward the call — ABI encoding, signing,
// and gateway transport all live in `ackinacki_kit`.
//
// See `docs/tech-specs/write-api.md §Chain submission` for the layering
// contract this implements.

use std::time::Duration;

use ackinacki_kit::contracts::dex::order_book::OrderBookOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSplitFullSet;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use async_trait::async_trait;
use dodex_application::BatchOrderPayloadItem;
use dodex_application::CancelBatchOrderPayload;
use dodex_application::CancelOrderPayload;
use dodex_application::ChainOrderSender;
use dodex_application::NewBatchOrderPayload;
use dodex_application::NewOrderPayload;
use dodex_application::SplitFullSetPayload;
use dodex_chain::ChainError;
use dodex_chain::Dex;
use dodex_domain::DomainError;
use dodex_domain::SensitiveBytes;
use num_bigint::BigUint;
use tokio::time::timeout;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;
use zeroize::Zeroizing;

pub struct DexChainSender {
    dex: Dex,
    place_order_timeout: Duration,
    cancel_order_timeout: Duration,
    place_batch_timeout: Duration,
    cancel_batch_timeout: Duration,
    split_full_set_timeout: Duration,
}

impl DexChainSender {
    /// Connect to the given gateway endpoints. Constructed once at API
    /// startup; production wiring already wraps the sender in
    /// `Arc<dyn ChainOrderSender>` at the `AppState` boundary, so the
    /// inner `Dex` does not need its own `Arc`.
    ///
    /// Each timeout bounds the per-call wait for one chain entry point —
    /// `dodex_chain::Dex` itself has no per-request deadline, so a
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
        split_full_set_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let dex = Dex::from_endpoints(endpoints)
            .map_err(|err| anyhow::anyhow!("Dex::from_endpoints failed: {err:?}"))?;
        Ok(Self {
            dex,
            place_order_timeout,
            cancel_order_timeout,
            place_batch_timeout,
            cancel_batch_timeout,
            split_full_set_timeout,
        })
    }
}

#[async_trait]
impl ChainOrderSender for DexChainSender {
    async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError> {
        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        // Defence-in-depth: the application layer caps both at
        // `u64::MAX`, so reaching the error arm means the gate was
        // bypassed. Log loudly and fail closed.
        let amount = payload.amount_raw.parse::<u128>().map_err(|err| {
            error!(?err, raw = %payload.amount_raw, "amount_raw exceeds uint128");
            DomainError::Unexpected
        })?;
        let client_order_id = payload.client_order_id.parse::<u128>().map_err(|err| {
            error!(?err, raw = %payload.client_order_id, "client_order_id is not uint128");
            DomainError::Unexpected
        })?;

        // Move payload's correlation fields out before they flow into
        // `params` (which `dex.place_order` consumes); the post-await
        // `ChainCallContext` then borrows them so timeout / unmapped-
        // error logs carry the same `pn`/`event_id` as the audit lines
        // for this dispatch.
        let pn_address = payload.pn_address;
        let event_id = payload.event_id;
        let params = ParamsOfPlaceOrder {
            event_id: event_id.clone(),
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            outcome_id: payload.outcome_id,
            is_buy: payload.is_buy,
            price: payload.price_raw,
            amount,
            flags: payload.flags,
            // MVP: neither field is exposed by api-spec; constants per
            // kit integration test convention. See `docs/tech-specs/
            // write-api.md §Chain submission` for the rationale.
            min_amount: 0,
            epoch_id: 0,
            client_order_id,
        };

        debug!(
            pn = %pn_address,
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

        let call = self.dex.place_order(&pn_address, params, signer);
        let outcome = timeout(self.place_order_timeout, call).await;
        let ctx = ChainCallContext {
            entry_point: "place_order",
            pn: &pn_address,
            event_id: &event_id,
            order_count: 1,
        };
        classify_chain_outcome(outcome, self.place_order_timeout.as_millis() as u64, &ctx)
    }

    async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError> {
        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        let pn_address = payload.pn_address;
        let event_id = payload.event_id;
        let params = ParamsOfCancelOrder {
            event_id: event_id.clone(),
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            // The application layer caps `order_id` at u64::MAX; the
            // chain ABI is uint128 but the `serde_json::json!` path
            // panics on values above u64::MAX (same constraint as
            // `clientOrderId`). Upcast is loss-less.
            order_id: payload.order_id as u128,
        };

        debug!(
            pn = %pn_address,
            event_id = %params.event_id,
            oracle_list_hash = %params.oracle_list_hash,
            token_type = params.token_type,
            order_id = params.order_id,
            "cancel_order params",
        );

        let call = self.dex.cancel_order(&pn_address, params, signer);
        let outcome = timeout(self.cancel_order_timeout, call).await;
        let ctx = ChainCallContext {
            entry_point: "cancel_order",
            pn: &pn_address,
            event_id: &event_id,
            order_count: 1,
        };
        classify_chain_outcome(outcome, self.cancel_order_timeout.as_millis() as u64, &ctx)
    }

    async fn submit_batch_order(&self, payload: NewBatchOrderPayload) -> Result<(), DomainError> {
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
        // from the gateway side anyway. Emit the audit line before any
        // fallible step (mirroring `cancel_batch_order`) so a key-shape
        // failure (`accounts.pn_seckey_enc` rotation drift) surfacing
        // inside `build_signer`, or a defence-in-depth `encode_batch_item`
        // bypass on amount/coid parse, still leaves ops the
        // clientOrderIds the caller intended.
        let client_order_ids: Vec<&str> =
            payload.orders.iter().map(|o| o.client_order_id.as_str()).collect();
        info!(
            entry_point = "place_batch",
            pn = %payload.pn_address,
            event_id = %payload.event_id,
            oracle_list_hash = %payload.oracle_list_hash,
            token_type = payload.token_type,
            order_count = payload.orders.len(),
            ?client_order_ids,
            "submitting place_batch",
        );

        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        // Consume `payload.orders` by value so per-item `price_raw`
        // moves into the chain payload instead of being cloned. Stash
        // correlation fields up front so the post-await
        // `ChainCallContext` can borrow them after `params` consumes
        // their originals.
        let order_count = payload.orders.len();
        let pn_address = payload.pn_address;
        let event_id = payload.event_id;
        let mut orders = Vec::with_capacity(order_count);
        for (item_index, item) in payload.orders.into_iter().enumerate() {
            orders.push(encode_batch_item(item, item_index)?);
        }

        let params = ParamsOfPlaceBatch {
            event_id: event_id.clone(),
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            orders,
        };

        let call = self.dex.place_batch(&pn_address, params, signer);
        let outcome = timeout(self.place_batch_timeout, call).await;
        let ctx = ChainCallContext {
            entry_point: "place_batch",
            pn: &pn_address,
            event_id: &event_id,
            order_count,
        };
        classify_chain_outcome(outcome, self.place_batch_timeout.as_millis() as u64, &ctx)
    }

    async fn cancel_batch_order(
        &self,
        payload: CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
        // `order_ids` is the primary correlation key (chain-assigned,
        // known to caller) and `client_order_ids` mirrors the
        // resolution so an ops incident triaged by customer-visible
        // coid can be greppped without joining `live_orders` back to
        // recover them. Both must appear here because
        // `classify_chain_outcome` does not carry them on its
        // error/timeout logs — emit at `info!` so the production
        // default filter keeps the line. Audit emits before any
        // fallible step so a key-shape failure
        // (`accounts.pn_seckey_enc` rotation drift) surfacing inside
        // `build_signer` still leaves ops the ids the caller intended.
        let order_ids: Vec<u64> = payload.items.iter().map(|i| i.order_id).collect();
        let client_order_ids: Vec<Option<&str>> =
            payload.items.iter().map(|i| i.client_order_id.as_deref()).collect();
        info!(
            entry_point = "cancel_batch",
            pn = %payload.pn_address,
            event_id = %payload.event_id,
            oracle_list_hash = %payload.oracle_list_hash,
            token_type = payload.token_type,
            order_count = payload.items.len(),
            ?order_ids,
            ?client_order_ids,
            "submitting cancel_batch",
        );

        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        // Chain ABI is uint128[]; `order_id` is `u64` at the application
        // boundary, upcast is loss-less.
        let order_count = order_ids.len();
        let order_ids: Vec<u128> = order_ids.into_iter().map(|id| id as u128).collect();
        let pn_address = payload.pn_address;
        let event_id = payload.event_id;

        let params = ParamsOfCancelBatch {
            event_id: event_id.clone(),
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            order_ids,
        };

        let call = self.dex.cancel_batch(&pn_address, params, signer);
        let outcome = timeout(self.cancel_batch_timeout, call).await;
        let ctx = ChainCallContext {
            entry_point: "cancel_batch",
            pn: &pn_address,
            event_id: &event_id,
            order_count,
        };
        classify_chain_outcome(outcome, self.cancel_batch_timeout.as_millis() as u64, &ctx)
    }

    async fn split_full_set(&self, payload: SplitFullSetPayload) -> Result<(), DomainError> {
        let signer = build_signer(&payload.pn_pubkey, &payload.pn_seckey)?;

        // Defence-in-depth: the application layer caps `collateral_raw`
        // at `u64::MAX` (same `serde_json::json!` ceiling as
        // `placeOrder.amount` and `clientOrderId`). Reaching the error
        // arm means that gate was bypassed.
        let collateral = payload.collateral_raw.parse::<u128>().map_err(|err| {
            error!(?err, raw = %payload.collateral_raw, "collateral_raw is not uint128");
            DomainError::Unexpected
        })?;

        let pn_address = payload.pn_address;
        let event_id = payload.event_id;
        let params = ParamsOfSplitFullSet {
            event_id: event_id.clone(),
            oracle_list_hash: payload.oracle_list_hash,
            token_type: payload.token_type,
            collateral,
        };

        debug!(
            pn = %pn_address,
            event_id = %params.event_id,
            oracle_list_hash = %params.oracle_list_hash,
            token_type = params.token_type,
            collateral = params.collateral,
            "split_full_set params",
        );

        let call = self.dex.split_full_set(&pn_address, params, signer);
        let outcome = timeout(self.split_full_set_timeout, call).await;
        let ctx = ChainCallContext {
            entry_point: "split_full_set",
            pn: &pn_address,
            event_id: &event_id,
            // splitFullSet is a market-level op, not an order op;
            // `order_count` has no meaning for it. Carry `1` so the
            // unified audit shape stays consistent with the other
            // single-call entry points.
            order_count: 1,
        };
        classify_chain_outcome(outcome, self.split_full_set_timeout.as_millis() as u64, &ctx)
    }
}

/// Translate one application-layer `BatchOrderPayloadItem` into the
/// chain `OrderBookOrder` tuple. The `amount_raw` / `client_order_id`
/// parses are defence-in-depth: `validate_and_encode_order_item`
/// (application crate) already enforces both stay within `u64::MAX`
/// per the kit / `serde_json` ceiling, so reaching the error
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
/// `owner_public_key_hex` / `owner_secret_key_hex` shape kit
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
        // 500 the caller can do nothing about. The first 12 chars of
        // the decimal string is enough to grep one PN's failures
        // apart from broader infra noise (e.g. KEK rotation drift
        // hitting one row) — pn_pubkey is public per the ACK
        // protocol, and the prefix is too short to reconstruct the
        // full key.
        let pn_prefix: String = pn_pubkey.chars().take(12).collect();
        error!(pn_pubkey_prefix = %pn_prefix, "trading PN pubkey is not a valid uint256 decimal");
        DomainError::MarketInconsistent
    })?;
    // Pass a fresh clone to `Signer` — the upstream `KeyPair.secret`
    // is `String` without `Zeroize`. Our `Zeroizing<String>` local
    // scrubs on drop regardless of how we exit.
    let keys = KeyPair { public, secret: (*secret_hex).clone() };
    Ok(Signer::Keys { keys })
}

/// Correlation context threaded into every timeout / unmapped-error
/// log so ops can match a chain failure back to the `info!` audit
/// line each dispatcher emits before calling out. Without these
/// fields, the elapsed/unmapped logs collapsed every in-flight chain
/// call of one kind into a single undifferentiated event.
#[derive(Debug, Clone, Copy)]
struct ChainCallContext<'a> {
    entry_point: &'static str,
    pn: &'a str,
    event_id: &'a str,
    /// `1` for single-order entry points; `payload.orders.len()` /
    /// `payload.order_ids.len()` for batch entry points.
    order_count: usize,
}

/// Translate the `(timeout, chain-call)` result chain into the typed
/// `DomainError` surface. Lifted out of the per-method dispatchers so
/// the three arms can be exercised by unit tests against real
/// `ChainError` values — the HTTP integration tests fake
/// `ChainOrderSender` entirely and would not catch a future refactor
/// that breaks the wiring between `map_chain_error` and the chain
/// dispatch. `ctx` carries `entry_point` + `pn`/`event_id`/`order_count`
/// so the timeout and unmapped-error logs self-correlate with the audit
/// line each caller emits before dispatch.
fn classify_chain_outcome<T>(
    outcome: Result<Result<T, ChainError>, tokio::time::error::Elapsed>,
    timeout_ms: u64,
    ctx: &ChainCallContext<'_>,
) -> Result<(), DomainError> {
    match outcome {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(app_err)) => Err(map_chain_error(&app_err, ctx)),
        Err(_elapsed) => {
            // Gateway did not respond within the configured budget
            // (`chain.place_order_timeout_ms`,
            // `chain.cancel_order_timeout_ms`,
            // `chain.place_batch_timeout_ms`,
            // `chain.cancel_batch_timeout_ms`). Most often a
            // network partition or a gateway-side deadlock. Surface as
            // `RequestTimeout` (504 / -1007) so the client receives the
            // same "retry with the same id" contract as the HTTP
            // request-timeout hoop — see
            // `docs/tech-specs/write-api.md §Failure surface`.
            error!(
                entry_point = ctx.entry_point,
                pn = ctx.pn,
                event_id = ctx.event_id,
                order_count = ctx.order_count,
                timeout_ms,
                "chain call timed out before gateway responded",
            );
            Err(DomainError::RequestTimeout)
        }
    }
}

/// Translate a `ChainError` into a typed `DomainError`. The underlying
/// `dodex_chain::Dex::{place_order, cancel_order}` calls already wait
/// for the chain to finish executing the corresponding PN method, so a
/// kit error carrying a `tvm_error` payload here gives us the exact
/// `require(...)` exit code the chain raised. Mapping it to a specific
/// `DomainError` lets the HTTP caller distinguish "balance
/// insufficient" from "PN busy" from "transport blew up" without
/// polling `/api/v1/orders` for absence.
///
/// Unrecognised chain codes and non-TVM failures (gateway disconnect,
/// malformed reply, etc.) collapse to `Unexpected` so a new chain
/// error variant cannot silently surface as a misleading 400. See
/// [`docs/tech-specs/write-api.md` §Failure surface] for the per-code
/// contract this enforces.
fn map_chain_error(err: &ChainError, ctx: &ChainCallContext<'_>) -> DomainError {
    if let Some(exit) = err.tvm_exit_code()
        && let Ok(code) = u16::try_from(exit)
        && let Some(mapped) = map_tvm_exit_code(code)
    {
        // Destructure to re-emit the `KitError` / `ClientError` payload
        // as separate keyed fields rather than one Debug blob — ops
        // queries that filter on `err_module` / `err_kit_code` keep
        // working across the enum boundary. `ChainError::Decode` cannot
        // reach here: `tvm_exit_code` returns `None` for it.
        match err {
            ChainError::Kit(k) => warn!(
                entry_point = ctx.entry_point,
                pn = ctx.pn,
                event_id = ctx.event_id,
                order_count = ctx.order_count,
                exit_code = code,
                err_module = ?k.module,
                err_kit_code = ?k.code,
                err_message = %k.message,
                tvm_err = ?k.tvm_error,
                domain_error = ?mapped,
                "chain rejected request",
            ),
            ChainError::Client(c) => warn!(
                entry_point = ctx.entry_point,
                pn = ctx.pn,
                event_id = ctx.event_id,
                order_count = ctx.order_count,
                exit_code = code,
                err_client_code = c.code(),
                err_message = c.message(),
                domain_error = ?mapped,
                "chain rejected request",
            ),
            ChainError::Decode(_) => {
                unreachable!("ChainError::Decode has no tvm_exit_code; mapped branch unreachable",)
            }
        }
        return mapped;
    }
    // Unmapped path: full diagnostic at error level since this is
    // either a transport failure or a chain code we have not learned
    // to handle yet.
    error!(
        entry_point = ctx.entry_point,
        pn = ctx.pn,
        event_id = ctx.event_id,
        order_count = ctx.order_count,
        ?err,
        "chain call failed (unmapped)",
    );
    DomainError::Unexpected
}

/// `PrivateNote` exit codes from `contracts/modifiers/errors.sol`,
/// shared across the four entry points (`placeOrder`, `cancelOrder`,
/// `placeBatch`, `cancelBatch`) the wrapper dispatches — the
/// originating entry point is carried in `ChainCallContext.entry_point`
/// for the unmapped-code log site. Only codes the trading path can
/// plausibly raise are mapped; everything else returns `None` so
/// `map_chain_error` can fall through to `Unexpected` (and log the
/// raw error for later triage).
fn map_tvm_exit_code(code: u16) -> Option<DomainError> {
    match code {
        // ERR_LOW_VALUE: insufficient `_balance[tokenType]` for BUY,
        // insufficient `stake.amount[outcomeId]` for SELL, or
        // `amount <= 0` (the last is normally caught by our local
        // precision check, but the contract guards it too).
        102 => Some(DomainError::OrderValidationFailed),
        // ERR_NOTE_BUSY: another PN-level op (any of placeOrder /
        // cancelOrder / placeBatch / cancelBatch) is still holding the
        // `_busy` lock for this PN. Distinct retry semantics — 429 /
        // -2014 instead of -2010.
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
    use dodex_application::CancelBatchPayloadItem;

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

    fn tvm_exit(code: u16) -> ChainError {
        use ackinacki_kit::contracts::error::DexModule;
        use ackinacki_kit::contracts::error::KitError;
        use ackinacki_kit::contracts::error::KitErrorCode;
        use ackinacki_kit::contracts::error::KitModule;
        use ackinacki_kit::tvm_client::error::ClientError;
        // PN exit codes (102/121/129/130/142/150/151/160/161/162/163/164/168)
        // are raised by `PrivateNote`; module matches the originating
        // contract so a reader is not misled when reviewing the test.
        ChainError::Kit(KitError {
            module: KitModule::Dex(DexModule::PrivateNote),
            code: KitErrorCode::None,
            message: "Send message".into(),
            tvm_error: Some(ClientError::new(
                414,
                "test",
                serde_json::json!({
                    "node_error": { "extensions": { "details": { "exit_code": code as u32 } } }
                }),
            )),
        })
    }

    fn ctx_single(entry_point: &'static str) -> ChainCallContext<'static> {
        ChainCallContext { entry_point, pn: "0:test-pn", event_id: "0xevent", order_count: 1 }
    }

    fn ctx_batch(entry_point: &'static str, order_count: usize) -> ChainCallContext<'static> {
        ChainCallContext { entry_point, pn: "0:test-pn", event_id: "0xevent", order_count }
    }

    #[test]
    fn maps_known_pn_validation_codes() {
        // The full table from `map_tvm_exit_code` — locking each row
        // down so a future "small refactor" cannot silently remap a
        // known reject onto the wrong HTTP class. Codes come from
        // `contracts/modifiers/errors.sol`.
        let single = ctx_single("place_order");
        let batch = ctx_batch("place_batch", 3);
        assert_eq!(map_chain_error(&tvm_exit(102), &single), DomainError::OrderValidationFailed);
        assert_eq!(map_chain_error(&tvm_exit(121), &single), DomainError::OrderPnBusy);
        assert_eq!(map_chain_error(&tvm_exit(129), &batch), DomainError::InvalidParameter);
        assert_eq!(map_chain_error(&tvm_exit(130), &single), DomainError::MarketInconsistent);
        assert_eq!(map_chain_error(&tvm_exit(142), &single), DomainError::OrderValidationFailed);
        assert_eq!(map_chain_error(&tvm_exit(150), &single), DomainError::OrderValidationFailed);
        assert_eq!(map_chain_error(&tvm_exit(151), &single), DomainError::OrderValidationFailed);
        assert_eq!(map_chain_error(&tvm_exit(160), &single), DomainError::OrderValidationFailed);
        assert_eq!(map_chain_error(&tvm_exit(161), &batch), DomainError::MarketInconsistent);
        assert_eq!(map_chain_error(&tvm_exit(162), &batch), DomainError::MarketInconsistent);
        assert_eq!(map_chain_error(&tvm_exit(163), &single), DomainError::PrecisionExceeded);
        assert_eq!(map_chain_error(&tvm_exit(164), &single), DomainError::PrecisionExceeded);
        assert_eq!(map_chain_error(&tvm_exit(168), &batch), DomainError::OrderValidationFailed);
    }

    #[test]
    fn unknown_exit_code_falls_through_to_unexpected() {
        // A chain code we haven't learned to handle MUST NOT degrade
        // to a misleading 400 — the operator needs to see it in logs
        // and decide whether to extend the mapping.
        assert_eq!(
            map_chain_error(&tvm_exit(9999), &ctx_single("place_order")),
            DomainError::Unexpected
        );
    }

    #[test]
    fn kit_error_without_tvm_payload_is_unexpected() {
        // Transport / decode kit failures that did not bubble up from
        // a contract revert carry no exit code; they must not be
        // misinterpreted as a known TVM exit.
        use ackinacki_kit::contracts::error::DexModule;
        use ackinacki_kit::contracts::error::KitError;
        use ackinacki_kit::contracts::error::KitErrorCode;
        use ackinacki_kit::contracts::error::KitModule;
        let ctx = ctx_single("place_order");
        let err = ChainError::Kit(KitError {
            module: KitModule::Dex(DexModule::PrivateNote),
            code: KitErrorCode::AccountIsNotActive,
            message: "transport blew up".into(),
            tvm_error: None,
        });
        assert_eq!(map_chain_error(&err, &ctx), DomainError::Unexpected);
    }

    #[test]
    fn tvm_payload_missing_exit_code_is_unexpected() {
        // The `data()` JSON exists but does not carry an `exit_code`
        // — could happen if the gateway shape ever changes. Must not
        // panic; must fall through cleanly.
        use ackinacki_kit::contracts::error::DexModule;
        use ackinacki_kit::contracts::error::KitError;
        use ackinacki_kit::contracts::error::KitErrorCode;
        use ackinacki_kit::contracts::error::KitModule;
        use ackinacki_kit::tvm_client::error::ClientError;
        let err = ChainError::Kit(KitError {
            module: KitModule::Dex(DexModule::PrivateNote),
            code: KitErrorCode::None,
            message: "boom".into(),
            tvm_error: Some(ClientError::new(
                414,
                "test",
                serde_json::json!({ "some_other_shape": true }),
            )),
        });
        assert_eq!(map_chain_error(&err, &ctx_single("place_order")), DomainError::Unexpected);
    }

    #[test]
    fn decode_error_is_unexpected() {
        // Local DTO parse failures (parallel-array mismatch on
        // getOrdersByOwner, etc.) carry no exit code to map.
        let err = ChainError::Decode("parse failed".into());
        assert_eq!(map_chain_error(&err, &ctx_single("place_order")), DomainError::Unexpected);
    }

    #[test]
    fn classify_chain_outcome_ok_passes_through() {
        let outcome: Result<Result<(), ChainError>, tokio::time::error::Elapsed> = Ok(Ok(()));
        assert!(classify_chain_outcome(outcome, 1_000, &ctx_single("place_order")).is_ok());
    }

    #[test]
    fn classify_chain_outcome_pipes_app_error_through_map_chain_error() {
        // Regression: HTTP-level tests fake `ChainOrderSender` whole,
        // which means a future refactor that drops the
        // `map_chain_error` call inside the chain dispatchers would
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
        let ctx = ctx_single("place_order");
        for (code, expected) in cases {
            let outcome: Result<Result<(), ChainError>, tokio::time::error::Elapsed> =
                Ok(Err(tvm_exit(code)));
            let result = classify_chain_outcome(outcome, 1_000, &ctx);
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
            std::future::pending::<Result<(), ChainError>>(),
        )
        .await;
        assert_eq!(
            classify_chain_outcome(elapsed, 30_000, &ctx_single("place_order")),
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
        let sender = DexChainSender::new(
            vec!["http://invalid.example.test".to_string()],
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        )
        .expect("DexChainSender::new");

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

    #[tokio::test]
    async fn cancel_batch_malformed_pn_pubkey_surfaces_as_market_inconsistent() {
        // Sibling of `malformed_pn_pubkey_surfaces_as_market_inconsistent`
        // for the `cancel_batch_order` entry. Pin: `build_signer`
        // failure surfaces as `MarketInconsistent`, same fail-closed
        // shape as the other three entry points — protects against a
        // future refactor that propagated the decode error as
        // `Unexpected` and leaked into 500s. The audit-before-signer
        // ordering is enforced by code review of `cancel_batch_order`
        // itself, not by this test.
        let sender = DexChainSender::new(
            vec!["http://invalid.example.test".to_string()],
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        )
        .expect("DexChainSender::new");

        let payload = CancelBatchOrderPayload {
            pn_address: "0:pn".into(),
            pn_pubkey: "not-a-decimal".into(),
            pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            event_id: "1".into(),
            oracle_list_hash: "1".into(),
            token_type: 3,
            items: vec![
                CancelBatchPayloadItem { order_id: 111, client_order_id: Some("coid-1".into()) },
                CancelBatchPayloadItem { order_id: 222, client_order_id: None },
                CancelBatchPayloadItem { order_id: 333, client_order_id: Some("coid-3".into()) },
            ],
        };

        let err = sender.cancel_batch_order(payload).await.expect_err("decode must fail closed");
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn encode_batch_item_amount_above_u128_surfaces_as_unexpected() {
        // Defence-in-depth pin: `validate_and_encode_order_item` in the
        // application crate caps `amount_raw` at `u64::MAX` today, so
        // `encode_batch_item` only sees values that parse as u128
        // trivially. If that upstream gate is ever relaxed, this guard
        // is the last thing between a wider integer and a panic / wrong
        // chain payload. Pin both the rejection (DomainError::Unexpected)
        // and the field that trips it (amount_raw) so the test names
        // the contract a future refactor would have to consciously
        // change.
        let item = BatchOrderPayloadItem {
            outcome_id: 1,
            is_buy: true,
            price_raw: "615".into(),
            // u128::MAX + 1 — the smallest value the u128 parse must reject.
            amount_raw: "340282366920938463463374607431768211456".into(),
            flags: 0,
            client_order_id: "42".into(),
        };
        let err = encode_batch_item(item, 0).expect_err("amount > u128::MAX must fail closed");
        assert_eq!(err, DomainError::Unexpected);
    }

    #[test]
    fn encode_batch_item_client_order_id_above_u128_surfaces_as_unexpected() {
        // Same defence as the amount test, on the `client_order_id`
        // arm. Both arms are reachable independently; covering both
        // means a future refactor that drops one of the two guards
        // cannot regress silently.
        let item = BatchOrderPayloadItem {
            outcome_id: 1,
            is_buy: true,
            price_raw: "615".into(),
            amount_raw: "1500000".into(),
            // u128::MAX + 1.
            client_order_id: "340282366920938463463374607431768211456".into(),
            flags: 0,
        };
        let err =
            encode_batch_item(item, 0).expect_err("client_order_id > u128::MAX must fail closed");
        assert_eq!(err, DomainError::Unexpected);
    }
}
