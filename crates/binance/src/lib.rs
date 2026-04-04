//! Binance provider for cryptocurrency trading.
//!
//! This crate provides adapters and WebSocket providers for Binance trading products:
//! - [`spot`] - Binance Spot trading
//! - [`futures`] - Binance USDS-M Futures trading
//! - [`coin_futures`] - Binance COIN-M Futures trading
//! - [`margin`] - Binance Margin trading (Cross/Isolated)
//! - [`common`] - Shared utilities (auth, HTTP client, error mapping)

pub mod coin_futures;
pub mod common;
pub mod credentials;
pub mod futures;
pub mod margin;
pub mod spot;
pub mod user_data_stream;
pub mod websocket;
pub mod websocket_types;

pub use coin_futures::adapter::BinanceCoinFuturesAdapter;
pub use credentials::BinanceCredentials;
pub use futures::adapter::BinanceFuturesAdapter;
pub use margin::adapter::BinanceMarginAdapter;
pub use spot::adapter::BinanceSpotAdapter;
pub use websocket::BinanceWebSocketProvider;

// Re-export common utilities for external use
#[allow(unused_imports)]
pub use common::BinanceHttpClient;
pub use common::binance_error_mapper;

// Re-export WebSocket types
#[allow(unused_imports)]
pub use user_data_stream::BinanceUserDataStream;

/// Map a Binance order status string to a gateway [`OrderStatus`](tektii_gateway_core::models::OrderStatus).
///
/// Binance uses these status strings:
/// `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, `PENDING_CANCEL`, `REJECTED`, `EXPIRED`
pub fn map_binance_order_status(
    status: &str,
) -> tektii_gateway_core::GatewayResult<tektii_gateway_core::models::OrderStatus> {
    match status {
        "NEW" => Ok(tektii_gateway_core::models::OrderStatus::Open),
        "PARTIALLY_FILLED" => Ok(tektii_gateway_core::models::OrderStatus::PartiallyFilled),
        "FILLED" => Ok(tektii_gateway_core::models::OrderStatus::Filled),
        "CANCELED" => Ok(tektii_gateway_core::models::OrderStatus::Cancelled),
        "PENDING_CANCEL" => Ok(tektii_gateway_core::models::OrderStatus::Pending),
        "REJECTED" => Ok(tektii_gateway_core::models::OrderStatus::Rejected),
        "EXPIRED" => Ok(tektii_gateway_core::models::OrderStatus::Expired),
        other => Err(tektii_gateway_core::GatewayError::InvalidValue {
            field: "status".to_string(),
            provided: Some(other.to_string()),
            message: format!("Unknown order status from Binance: {other}"),
        }),
    }
}
