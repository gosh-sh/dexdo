// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

//! Wire-facing enum schemas for the OpenAPI document.
//!
//! `dodex-domain` stays free of web-framework dependencies, so its
//! enums cannot derive `ToSchema`. These mirrors exist so the generated
//! spec carries named enum components (`$ref`-able from DTO fields and
//! query parameters) instead of bare `string`. Each mirror serializes
//! to exactly the wire value of the matching domain `as_str()` — the
//! round-trip tests below pin that equivalence variant by variant.

use salvo_oapi::ToSchema;
use serde::Serialize;

/// Market phase. A market is in exactly one of nine phases; see
/// docs/api-spec.md §Market Status for the lifecycle.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum MarketStatus {
    Pending,
    Upcoming,
    Staking,
    AwaitingFreeze,
    Trading,
    Resolving,
    Resolved,
    Cancelled,
    Expired,
}

impl From<dodex_domain::MarketStatus> for MarketStatus {
    fn from(value: dodex_domain::MarketStatus) -> Self {
        use dodex_domain::MarketStatus as D;
        match value {
            D::Pending => Self::Pending,
            D::Upcoming => Self::Upcoming,
            D::Staking => Self::Staking,
            D::AwaitingFreeze => Self::AwaitingFreeze,
            D::Trading => Self::Trading,
            D::Resolving => Self::Resolving,
            D::Resolved => Self::Resolved,
            D::Cancelled => Self::Cancelled,
            D::Expired => Self::Expired,
        }
    }
}

/// Order lifecycle status. `PENDING_NEW` / `PENDING_CANCEL` are
/// write-side acceptance states: they appear only in synchronous
/// POST/DELETE responses, never in `GET /api/v1/prediction/orders` rows.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum OrderStatus {
    PendingNew,
    New,
    PartiallyFilled,
    PendingCancel,
    Filled,
    Canceled,
    Rejected,
}

impl From<dodex_domain::OrderStatus> for OrderStatus {
    fn from(value: dodex_domain::OrderStatus) -> Self {
        use dodex_domain::OrderStatus as D;
        match value {
            D::PendingNew => Self::PendingNew,
            D::New => Self::New,
            D::PartiallyFilled => Self::PartiallyFilled,
            D::PendingCancel => Self::PendingCancel,
            D::Filled => Self::Filled,
            D::Canceled => Self::Canceled,
            D::Rejected => Self::Rejected,
        }
    }
}

/// Order side.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum OrderSide {
    Buy,
    Sell,
}

impl From<dodex_domain::OrderSide> for OrderSide {
    fn from(value: dodex_domain::OrderSide) -> Self {
        use dodex_domain::OrderSide as D;
        match value {
            D::Buy => Self::Buy,
            D::Sell => Self::Sell,
        }
    }
}

/// Order type. Defaults to `LIMIT` when omitted on order placement.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum OrderType {
    Limit,
    Market,
}

impl From<dodex_domain::OrderType> for OrderType {
    fn from(value: dodex_domain::OrderType) -> Self {
        use dodex_domain::OrderType as D;
        match value {
            D::Limit => Self::Limit,
            D::Market => Self::Market,
        }
    }
}

/// Time-in-force. Applies to `LIMIT` orders only; defaults to `GTC`
/// when omitted on order placement.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
    PostOnly,
}

impl From<dodex_domain::TimeInForce> for TimeInForce {
    fn from(value: dodex_domain::TimeInForce) -> Self {
        use dodex_domain::TimeInForce as D;
        match value {
            D::Gtc => Self::Gtc,
            D::Ioc => Self::Ioc,
            D::Fok => Self::Fok,
            D::PostOnly => Self::PostOnly,
        }
    }
}

/// How a market ended. Mirrors the terminal subset of `MarketStatus`.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum TerminalKind {
    Resolved,
    Cancelled,
    Expired,
}

impl From<dodex_domain::TerminalKind> for TerminalKind {
    fn from(value: dodex_domain::TerminalKind) -> Self {
        use dodex_domain::TerminalKind as D;
        match value {
            D::Resolved => Self::Resolved,
            D::Cancelled => Self::Cancelled,
            D::Expired => Self::Expired,
        }
    }
}

/// Why a market was cancelled. Present only when `terminal.kind` is
/// `CANCELLED`; `null` otherwise.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum CancelReason {
    PmpRejectedByOracle,
    EventCancelled,
}

impl From<dodex_domain::CancelReason> for CancelReason {
    fn from(value: dodex_domain::CancelReason) -> Self {
        use dodex_domain::CancelReason as D;
        match value {
            D::PmpRejectedByOracle => Self::PmpRejectedByOracle,
            D::EventCancelled => Self::EventCancelled,
        }
    }
}

/// Sort order for `GET /api/v1/prediction/markets`: `resultStart` ascending
/// (default) or `createdAt` descending.
// Documentation-only: referenced from `#[endpoint(parameters(...))]`
// as a schema type, never constructed at runtime (the handler parses
// the raw string to keep missing-vs-invalid error fidelity).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MarketsSort {
    ResultStart,
    CreatedAt,
}

/// Order statuses accepted by the `GET /api/v1/prediction/orders` `status` filter.
/// Excludes the write-side acceptance states `PENDING_NEW` /
/// `PENDING_CANCEL`, which never appear in stored rows.
// Documentation-only, same rationale as `MarketsSort`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum QueryableOrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

#[cfg(test)]
mod wire_value_tests {
    use super::*;

    fn wire<T: Serialize>(value: T) -> String {
        serde_json::to_value(value).expect("serializes").as_str().expect("string").to_string()
    }

    // Each mirror must serialize to the exact `as_str()` of the domain
    // variant it was converted from — the spec and the runtime wire
    // bytes must agree or the enum schemas would document values the
    // server never emits.

    #[test]
    fn market_status_matches_domain_as_str() {
        use dodex_domain::MarketStatus as D;
        for d in [
            D::Pending,
            D::Upcoming,
            D::Staking,
            D::AwaitingFreeze,
            D::Trading,
            D::Resolving,
            D::Resolved,
            D::Cancelled,
            D::Expired,
        ] {
            assert_eq!(wire(MarketStatus::from(d)), d.as_str());
        }
    }

    #[test]
    fn order_status_matches_domain_as_str() {
        use dodex_domain::OrderStatus as D;
        for d in [
            D::PendingNew,
            D::New,
            D::PartiallyFilled,
            D::PendingCancel,
            D::Filled,
            D::Canceled,
            D::Rejected,
        ] {
            assert_eq!(wire(OrderStatus::from(d)), d.as_str());
        }
    }

    #[test]
    fn order_side_matches_domain_as_str() {
        use dodex_domain::OrderSide as D;
        for d in [D::Buy, D::Sell] {
            assert_eq!(wire(OrderSide::from(d)), d.as_str());
        }
    }

    #[test]
    fn order_type_matches_domain_as_str() {
        use dodex_domain::OrderType as D;
        for d in [D::Limit, D::Market] {
            assert_eq!(wire(OrderType::from(d)), d.as_str());
        }
    }

    #[test]
    fn time_in_force_matches_domain_as_str() {
        use dodex_domain::TimeInForce as D;
        for d in [D::Gtc, D::Ioc, D::Fok, D::PostOnly] {
            assert_eq!(wire(TimeInForce::from(d)), d.as_str());
        }
    }

    #[test]
    fn terminal_kind_matches_domain_serialization() {
        use dodex_domain::TerminalKind as D;
        for d in [D::Resolved, D::Cancelled, D::Expired] {
            let domain_wire = serde_json::to_value(d).expect("serializes");
            assert_eq!(serde_json::to_value(TerminalKind::from(d)).expect("ok"), domain_wire);
        }
    }

    #[test]
    fn cancel_reason_matches_domain_as_str() {
        use dodex_domain::CancelReason as D;
        for d in [D::PmpRejectedByOracle, D::EventCancelled] {
            assert_eq!(wire(CancelReason::from(d)), d.as_str());
        }
    }

    #[test]
    fn queryable_order_status_matches_filter_allow_list() {
        // The five values `OrderStatusFilter::from_csv` accepts, pinned
        // so the documented filter enum cannot drift from the parser.
        for (variant, expected) in [
            (QueryableOrderStatus::New, "NEW"),
            (QueryableOrderStatus::PartiallyFilled, "PARTIALLY_FILLED"),
            (QueryableOrderStatus::Filled, "FILLED"),
            (QueryableOrderStatus::Canceled, "CANCELED"),
            (QueryableOrderStatus::Rejected, "REJECTED"),
        ] {
            assert_eq!(wire(variant), expected);
            dodex_application::OrderStatusFilter::from_csv(Some(expected))
                .expect("filter accepts the documented value");
        }
    }

    #[test]
    fn markets_sort_matches_handler_allow_list() {
        assert_eq!(wire(MarketsSort::ResultStart), "resultStart");
        assert_eq!(wire(MarketsSort::CreatedAt), "createdAt");
    }
}
