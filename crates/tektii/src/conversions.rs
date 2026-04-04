//! Type conversions between Tektii Engine protocol types and gateway core models.
//!
//! Engine types (simpler, simulation-focused) are translated to/from
//! gateway core types (richer, production-focused).

use chrono::{DateTime, TimeZone, Utc};
use tektii_protocol::rest as engine;
use tracing::error;

use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models as api;
use tektii_gateway_core::websocket::messages as ws;

// =============================================================================
// Timestamp Utilities
// =============================================================================

/// Convert Unix milliseconds to `DateTime<Utc>`.
///
/// Falls back to the current time if the timestamp is invalid, with an error log.
/// Invalid timestamps indicate a bug in the engine.
pub fn unix_ms_to_datetime(ms: u64) -> DateTime<Utc> {
    let secs = match i64::try_from(ms / 1000) {
        Ok(s) => s,
        Err(e) => {
            error!(
                ms,
                error = %e,
                "Timestamp overflow from engine - using current time"
            );
            return Utc::now();
        }
    };
    // Safety: ms % 1000 is at most 999, so 999 * 1_000_000 = 999_000_000 fits in u32 (max 4_294_967_295)
    #[allow(clippy::cast_possible_truncation)]
    let nanos = ((ms % 1000) * 1_000_000) as u32;

    Utc.timestamp_opt(secs, nanos).single().unwrap_or_else(|| {
        error!(ms, "Invalid timestamp from engine - using current time");
        Utc::now()
    })
}

/// Convert `DateTime<Utc>` to Unix milliseconds (test only).
#[cfg(test)]
fn datetime_to_unix_ms(dt: DateTime<Utc>) -> u64 {
    dt.timestamp_millis() as u64
}

// =============================================================================
// Enum Conversions: Engine -> Gateway
// =============================================================================

/// Convert engine Side to gateway Side.
pub const fn engine_side_to_api(side: engine::Side) -> api::Side {
    match side {
        engine::Side::Buy => api::Side::Buy,
        engine::Side::Sell => api::Side::Sell,
    }
}

/// Convert engine `PositionSide` to gateway `PositionSide`.
pub const fn engine_position_side_to_api(side: engine::PositionSide) -> api::PositionSide {
    match side {
        engine::PositionSide::Long => api::PositionSide::Long,
        engine::PositionSide::Short => api::PositionSide::Short,
    }
}

/// Convert engine `OrderType` to gateway `OrderType`.
pub const fn engine_order_type_to_api(order_type: engine::OrderType) -> api::OrderType {
    match order_type {
        engine::OrderType::Market => api::OrderType::Market,
        engine::OrderType::Limit => api::OrderType::Limit,
        engine::OrderType::Stop => api::OrderType::Stop,
        engine::OrderType::StopLimit => api::OrderType::StopLimit,
    }
}

/// Convert engine `OrderStatus` to gateway `OrderStatus`.
pub const fn engine_order_status_to_api(status: engine::OrderStatus) -> api::OrderStatus {
    match status {
        engine::OrderStatus::Open => api::OrderStatus::Open,
        engine::OrderStatus::PartiallyFilled => api::OrderStatus::PartiallyFilled,
        engine::OrderStatus::Filled => api::OrderStatus::Filled,
        engine::OrderStatus::Cancelled => api::OrderStatus::Cancelled,
        engine::OrderStatus::Rejected => api::OrderStatus::Rejected,
    }
}

/// Convert engine `RejectionReason` to gateway `RejectReason`.
pub const fn engine_reject_reason_to_api(reason: engine::RejectionReason) -> api::RejectReason {
    match reason {
        engine::RejectionReason::InsufficientMargin => api::RejectReason::InsufficientMargin,
        engine::RejectionReason::InvalidSymbol => api::RejectReason::SymbolNotTradeable,
        engine::RejectionReason::InvalidQuantity
        | engine::RejectionReason::PositionSizeLimitExceeded => api::RejectReason::InvalidQuantity,
        engine::RejectionReason::PriceOutOfBounds => api::RejectReason::InvalidPrice,
        engine::RejectionReason::PositionNotFound => api::RejectReason::Unknown,
        engine::RejectionReason::ReduceOnlyViolated => api::RejectReason::ReduceOnlyViolated,
    }
}

/// Convert engine `OrderEventType` to gateway `OrderEventType`.
pub const fn engine_order_event_type_to_api(event: engine::OrderEventType) -> ws::OrderEventType {
    match event {
        engine::OrderEventType::Created => ws::OrderEventType::OrderCreated,
        engine::OrderEventType::PartialFill => ws::OrderEventType::OrderPartiallyFilled,
        engine::OrderEventType::Filled => ws::OrderEventType::OrderFilled,
        engine::OrderEventType::Cancelled => ws::OrderEventType::OrderCancelled,
        engine::OrderEventType::Rejected => ws::OrderEventType::OrderRejected,
    }
}

// =============================================================================
// Enum Conversions: Gateway -> Engine
// =============================================================================

/// Convert gateway Side to engine Side.
pub const fn api_side_to_engine(side: api::Side) -> engine::Side {
    match side {
        api::Side::Buy => engine::Side::Buy,
        api::Side::Sell => engine::Side::Sell,
    }
}

/// Convert gateway `OrderType` to engine `OrderType`.
/// Returns error for unsupported types (`TrailingStop`).
pub fn api_order_type_to_engine(
    order_type: api::OrderType,
) -> Result<engine::OrderType, GatewayError> {
    match order_type {
        api::OrderType::Market => Ok(engine::OrderType::Market),
        api::OrderType::Limit => Ok(engine::OrderType::Limit),
        api::OrderType::Stop => Ok(engine::OrderType::Stop),
        api::OrderType::StopLimit => Ok(engine::OrderType::StopLimit),
        api::OrderType::TrailingStop => Err(GatewayError::InvalidValue {
            field: "order_type".to_string(),
            message: "TrailingStop not supported by Tektii engine".to_string(),
            provided: Some(format!("{order_type:?}")),
        }),
    }
}

// =============================================================================
// Request Conversions: Gateway -> Engine
// =============================================================================

/// Reject unsupported features for the Tektii engine.
fn reject_if_set<T>(field: &str, value: Option<&T>) -> Result<(), GatewayError> {
    if value.is_some() {
        return Err(GatewayError::InvalidValue {
            field: field.to_string(),
            message: "not supported by Tektii engine".to_string(),
            provided: Some("(set)".to_string()),
        });
    }
    Ok(())
}

/// Reject if boolean flag is true.
fn reject_if_true(field: &str, value: bool) -> Result<(), GatewayError> {
    if value {
        return Err(GatewayError::InvalidValue {
            field: field.to_string(),
            message: "not supported by Tektii engine".to_string(),
            provided: Some("true".to_string()),
        });
    }
    Ok(())
}

/// Convert gateway `OrderRequest` to engine `SubmitOrderRequest`.
/// Validates that unsupported features are not used.
pub fn order_request_to_engine(
    req: &api::OrderRequest,
) -> Result<engine::SubmitOrderRequest, GatewayError> {
    // Reject unsupported features
    // NOTE: stop_loss, take_profit, and oco_group_id are NOT rejected here.
    // They are handled by the gateway's exit_handler via PendingSlTp bracket strategy.
    // The engine doesn't support these natively, but the gateway layer does.
    reject_if_set("trailing_distance", req.trailing_distance.as_ref())?;
    reject_if_set("margin_mode", req.margin_mode.as_ref())?;
    reject_if_set("leverage", req.leverage.as_ref())?;
    reject_if_set("display_quantity", req.display_quantity.as_ref())?;
    reject_if_true("post_only", req.post_only)?;
    reject_if_true("hidden", req.hidden)?;

    let order_type = api_order_type_to_engine(req.order_type)?;

    Ok(engine::SubmitOrderRequest {
        symbol: req.symbol.clone(),
        side: api_side_to_engine(req.side),
        order_type,
        quantity: req.quantity,
        limit_price: req.limit_price,
        stop_price: req.stop_price,
        client_order_id: req.client_order_id.clone(),
        position_id: req.position_id.clone(),
        reduce_only: req.reduce_only,
    })
}

// =============================================================================
// Response Conversions: Engine -> Gateway
// =============================================================================

/// Convert engine Order to gateway Order.
pub fn engine_order_to_api(order: &engine::Order) -> api::Order {
    let created_at = unix_ms_to_datetime(order.created_at);
    let updated_at = order
        .executed_at
        .or(order.cancelled_at)
        .map_or(created_at, unix_ms_to_datetime);

    // Engine uses `price` for limit/stop price, and `limit_price` for stop-limit's limit price.
    let (limit_price, stop_price) = if order.price.is_zero() {
        (None, None)
    } else {
        match order.order_type {
            engine::OrderType::Limit => (Some(order.price), None),
            engine::OrderType::Stop => (None, Some(order.price)),
            engine::OrderType::StopLimit => (order.limit_price, Some(order.price)),
            engine::OrderType::Market => (None, None),
        }
    };

    api::Order {
        id: order.id.clone(),
        client_order_id: order.client_order_id.clone(),
        symbol: order.symbol.clone(),
        side: engine_side_to_api(order.side),
        order_type: engine_order_type_to_api(order.order_type),
        quantity: order.quantity,
        filled_quantity: order.filled_quantity,
        remaining_quantity: order.quantity - order.filled_quantity,
        limit_price,
        stop_price,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        // Engine doesn't track average fill price separately; we use limit/stop price
        // as approximation, but return None for market orders (price == 0)
        average_fill_price: if order.filled_quantity.is_zero() || order.price.is_zero() {
            None
        } else {
            Some(order.price)
        },
        // Derive PartiallyFilled from filled_quantity since engine only has Open status
        status: if order.status == engine::OrderStatus::Open
            && !order.filled_quantity.is_zero()
            && order.filled_quantity < order.quantity
        {
            api::OrderStatus::PartiallyFilled
        } else {
            engine_order_status_to_api(order.status)
        },
        reject_reason: None, // Engine doesn't create rejected orders
        position_id: order.position_id.clone(),
        reduce_only: None,
        post_only: None,
        hidden: None,
        display_quantity: None,
        oco_group_id: None,
        correlation_id: None,
        time_in_force: api::TimeInForce::Gtc,
        created_at,
        updated_at,
    }
}

/// Convert engine `OrderHandle` to gateway `OrderHandle`.
pub fn engine_order_handle_to_api(handle: &engine::OrderHandle) -> api::OrderHandle {
    api::OrderHandle {
        id: handle.id.clone(),
        client_order_id: handle.client_order_id.clone(),
        correlation_id: None,
        status: engine_order_status_to_api(handle.status),
    }
}

/// Convert engine Position to gateway Position.
///
/// # Data Loss
///
/// The engine doesn't expose position open/update timestamps. We use `Utc::now()`
/// as a placeholder, which means these timestamps are non-deterministic and will
/// differ on each conversion. This is a known limitation.
pub fn engine_position_to_api(pos: &engine::Position) -> api::Position {
    let now = Utc::now();
    api::Position {
        id: pos.id.clone(),
        symbol: pos.symbol.clone(),
        side: engine_position_side_to_api(pos.side),
        quantity: pos.quantity,
        average_entry_price: pos.avg_entry_price,
        current_price: pos.current_price,
        unrealized_pnl: pos.unrealized_pnl,
        realized_pnl: rust_decimal::Decimal::ZERO,
        margin_mode: None,
        leverage: None,
        liquidation_price: None,
        opened_at: now,
        updated_at: now,
    }
}

/// Convert engine Account to gateway Account.
pub fn engine_account_to_api(account: &engine::Account) -> api::Account {
    api::Account {
        balance: account.balance,
        equity: account.equity,
        margin_used: account.margin_used,
        margin_available: account.margin_available,
        unrealized_pnl: account.unrealized_pnl,
        currency: "USD".to_string(), // Engine defaults to USD
    }
}

/// Convert engine `AccountEventType` to gateway `AccountEventType`.
pub const fn engine_account_event_to_api(event: engine::AccountEventType) -> ws::AccountEventType {
    match event {
        engine::AccountEventType::BalanceUpdated => ws::AccountEventType::BalanceUpdated,
        engine::AccountEventType::MarginWarning => ws::AccountEventType::MarginWarning,
        engine::AccountEventType::MarginCall => ws::AccountEventType::MarginCall,
    }
}

/// Convert engine Trade to gateway Trade.
pub fn engine_trade_to_api(trade: &engine::Trade) -> api::Trade {
    api::Trade {
        id: trade.id.clone(),
        order_id: trade.order_id.clone(),
        symbol: trade.symbol.clone(),
        side: engine_side_to_api(trade.side),
        quantity: trade.quantity,
        price: trade.price,
        commission: trade.commission,
        commission_currency: "USD".to_string(), // Engine defaults to USD
        is_maker: None,                         // Engine doesn't distinguish maker/taker
        timestamp: unix_ms_to_datetime(trade.timestamp),
    }
}

/// Convert engine `CancelAllResult` to gateway `CancelAllResult`.
pub const fn engine_cancel_all_to_api(result: &engine::CancelAllResult) -> api::CancelAllResult {
    api::CancelAllResult {
        cancelled_count: result.cancelled_count,
        failed_count: result.failed_count,
        failed_order_ids: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_unix_ms_roundtrip() {
        let original_ms: u64 = 1_704_067_200_000; // 2024-01-01 00:00:00 UTC
        let dt = unix_ms_to_datetime(original_ms);
        let back_to_ms = datetime_to_unix_ms(dt);
        assert_eq!(original_ms, back_to_ms);
    }

    #[test]
    fn test_unix_ms_to_datetime_zero() {
        let dt = unix_ms_to_datetime(0);
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn test_order_type_conversion() {
        assert_eq!(
            api_order_type_to_engine(api::OrderType::Market).unwrap(),
            engine::OrderType::Market
        );
        assert_eq!(
            api_order_type_to_engine(api::OrderType::Limit).unwrap(),
            engine::OrderType::Limit
        );
        assert_eq!(
            api_order_type_to_engine(api::OrderType::Stop).unwrap(),
            engine::OrderType::Stop
        );
        assert_eq!(
            api_order_type_to_engine(api::OrderType::StopLimit).unwrap(),
            engine::OrderType::StopLimit
        );
        assert!(api_order_type_to_engine(api::OrderType::TrailingStop).is_err());
    }

    #[test]
    fn test_order_request_rejects_unsupported() {
        // trailing_distance should be rejected
        let req = api::OrderRequest {
            trailing_distance: Some(dec!(10)),
            ..api::OrderRequest::market("AAPL", api::Side::Buy, dec!(100))
        };
        assert!(order_request_to_engine(&req).is_err());

        // stop_loss is now ALLOWED - handled by gateway's exit_handler via PendingSlTp
        let req =
            api::OrderRequest::market("AAPL", api::Side::Buy, dec!(100)).with_stop_loss(dec!(150));
        assert!(order_request_to_engine(&req).is_ok());

        // take_profit is now ALLOWED - handled by gateway's exit_handler via PendingSlTp
        let req = api::OrderRequest::market("AAPL", api::Side::Buy, dec!(100))
            .with_take_profit(dec!(200));
        assert!(order_request_to_engine(&req).is_ok());

        // oco_group_id is now ALLOWED - handled by gateway's exit_handler
        let req =
            api::OrderRequest::market("AAPL", api::Side::Buy, dec!(100)).with_oco_group("my-group");
        assert!(order_request_to_engine(&req).is_ok());

        // Basic market order should work
        let req = api::OrderRequest::market("AAPL", api::Side::Buy, dec!(100));
        assert!(order_request_to_engine(&req).is_ok());
    }

    #[test]
    fn test_engine_order_to_api() {
        let engine_order = engine::Order {
            id: "order-123".to_string(),
            client_order_id: Some("client-456".to_string()),
            symbol: "AAPL".to_string(),
            side: engine::Side::Buy,
            order_type: engine::OrderType::Limit,
            quantity: dec!(100),
            filled_quantity: dec!(50),
            price: dec!(150),
            limit_price: None,
            status: engine::OrderStatus::Open,
            position_id: None,
            created_at: 1_704_067_200_000,
            executed_at: None,
            cancelled_at: None,
            reject_reason: None,
            rejected_at: None,
        };

        let api_order = engine_order_to_api(&engine_order);
        assert_eq!(api_order.id, "order-123");
        assert_eq!(api_order.symbol, "AAPL");
        assert_eq!(api_order.side, api::Side::Buy);
        assert_eq!(api_order.order_type, api::OrderType::Limit);
        assert_eq!(api_order.quantity, dec!(100));
        assert_eq!(api_order.filled_quantity, dec!(50));
        assert_eq!(api_order.remaining_quantity, dec!(50));
        // Partially filled order (50/100) should be PartiallyFilled
        assert_eq!(api_order.status, api::OrderStatus::PartiallyFilled);
    }

    #[test]
    fn test_reject_reason_conversion() {
        assert_eq!(
            engine_reject_reason_to_api(engine::RejectionReason::InsufficientMargin),
            api::RejectReason::InsufficientMargin
        );
        assert_eq!(
            engine_reject_reason_to_api(engine::RejectionReason::InvalidSymbol),
            api::RejectReason::SymbolNotTradeable
        );
        assert_eq!(
            engine_reject_reason_to_api(engine::RejectionReason::InvalidQuantity),
            api::RejectReason::InvalidQuantity
        );
        assert_eq!(
            engine_reject_reason_to_api(engine::RejectionReason::PriceOutOfBounds),
            api::RejectReason::InvalidPrice
        );
        assert_eq!(
            engine_reject_reason_to_api(engine::RejectionReason::PositionNotFound),
            api::RejectReason::Unknown
        );
        assert_eq!(
            engine_reject_reason_to_api(engine::RejectionReason::ReduceOnlyViolated),
            api::RejectReason::ReduceOnlyViolated
        );
    }

    #[test]
    fn test_order_event_type_conversion() {
        assert_eq!(
            engine_order_event_type_to_api(engine::OrderEventType::Created),
            ws::OrderEventType::OrderCreated
        );
        assert_eq!(
            engine_order_event_type_to_api(engine::OrderEventType::Filled),
            ws::OrderEventType::OrderFilled
        );
        assert_eq!(
            engine_order_event_type_to_api(engine::OrderEventType::Cancelled),
            ws::OrderEventType::OrderCancelled
        );
        assert_eq!(
            engine_order_event_type_to_api(engine::OrderEventType::Rejected),
            ws::OrderEventType::OrderRejected
        );
    }

    #[test]
    fn test_engine_order_type_to_api() {
        assert_eq!(
            engine_order_type_to_api(engine::OrderType::Market),
            api::OrderType::Market
        );
        assert_eq!(
            engine_order_type_to_api(engine::OrderType::Limit),
            api::OrderType::Limit
        );
        assert_eq!(
            engine_order_type_to_api(engine::OrderType::Stop),
            api::OrderType::Stop
        );
        assert_eq!(
            engine_order_type_to_api(engine::OrderType::StopLimit),
            api::OrderType::StopLimit
        );
    }
}
