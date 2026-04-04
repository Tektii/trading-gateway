//! Core trading models and enums.

mod account;
mod bar;
mod bracket;
mod order;
mod platform;
mod position;
mod quote;
mod system;
mod trade;

pub use account::Account;
pub use bar::{Bar, BarParams, Timeframe};
pub use bracket::{PlaceOcoOrderRequest, PlaceOcoOrderResponse};
pub use order::{
    CancelAllResult, CancelOrderResult, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle,
    OrderQueryParams, OrderRequest,
};
pub use platform::{
    GatewayMode, TradingPlatform, TradingPlatformKind, TradingPlatformParseError, VALID_PROVIDERS,
};
pub use position::{CloseAllPositionsResult, ClosePositionRequest, Position};
pub use quote::Quote;
pub use system::{
    Capabilities, ConnectionStatus, DetailedHealthStatus, HealthStatus, OverallStatus,
    ProviderHealth, RateLimits, ReadyStatus,
};
pub use trade::{Trade, TradeQueryParams};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Order side (buy or sell)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side {
    /// Buy order (long entry, short exit)
    #[serde(alias = "buy")]
    Buy,
    /// Sell order (short entry, long exit)
    #[serde(alias = "sell")]
    Sell,
}

/// Position side (long or short)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PositionSide {
    /// Long position (profit from price increase)
    Long,
    /// Short position (profit from price decrease)
    Short,
}

/// Order type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    /// Execute immediately at best available price
    #[serde(alias = "market")]
    Market,
    /// Execute at specified price or better
    #[serde(alias = "limit")]
    Limit,
    /// Trigger market order when stop price reached
    #[serde(alias = "stop")]
    Stop,
    /// Trigger limit order when stop price reached
    #[serde(alias = "stop_limit")]
    StopLimit,
    /// Trailing stop that follows price movement
    #[serde(alias = "trailing_stop")]
    TrailingStop,
}

/// Order status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    /// Order received but not yet processed
    Pending,
    /// Cancellation requested but not yet confirmed
    PendingCancel,
    /// Order active in the order book
    Open,
    /// Order partially executed
    PartiallyFilled,
    /// Order fully executed
    Filled,
    /// Order cancelled by user or system
    Cancelled,
    /// Order rejected by exchange
    Rejected,
    /// Order expired (time-in-force reached)
    Expired,
}

/// Trailing stop type (how trailing distance is interpreted)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrailingType {
    /// Absolute price distance (e.g., $5 from peak)
    #[serde(alias = "absolute")]
    Absolute,
    /// Percentage of price (e.g., 2% from peak)
    #[serde(alias = "percent")]
    Percent,
}

/// Margin mode for leveraged trading
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarginMode {
    /// Shared margin across all positions
    #[serde(alias = "cross")]
    Cross,
    /// Margin allocated per position
    #[serde(alias = "isolated")]
    Isolated,
}

/// Asset class
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssetClass {
    /// Equities (stocks, ETFs)
    Stock,
    /// Foreign exchange currency pairs
    Forex,
    /// Cryptocurrencies and digital assets
    Crypto,
    /// Futures contracts
    Futures,
}

/// Position mode (how positions are aggregated)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PositionMode {
    /// Single position per symbol, orders aggregate
    Netting,
    /// Multiple positions per symbol allowed
    Hedging,
}

/// Time in force for orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    /// Good till cancelled (default)
    #[default]
    #[serde(alias = "gtc")]
    Gtc,
    /// Day order - expires at end of trading day
    #[serde(alias = "day")]
    Day,
    /// Immediate or cancel
    #[serde(alias = "ioc")]
    Ioc,
    /// Fill or kill
    #[serde(alias = "fok")]
    Fok,
}

/// Order rejection reason
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectReason {
    /// Not enough margin to place order
    InsufficientMargin,
    /// Not enough cash balance
    InsufficientBalance,
    /// Quantity below minimum or invalid step
    InvalidQuantity,
    /// Price below minimum or invalid tick size
    InvalidPrice,
    /// Symbol not available for trading
    SymbolNotTradeable,
    /// Trading hours closed
    MarketClosed,
    /// Reduce-only order would increase position
    ReduceOnlyViolated,
    /// Post-only order would take liquidity
    PostOnlyViolated,
    /// Rate limit exceeded
    RateLimited,
    /// Unknown rejection reason
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_accepts_uppercase_and_lowercase() {
        assert_eq!(serde_json::from_str::<Side>(r#""BUY""#).unwrap(), Side::Buy);
        assert_eq!(serde_json::from_str::<Side>(r#""buy""#).unwrap(), Side::Buy);
        // Serialization stays uppercase
        assert_eq!(serde_json::to_string(&Side::Buy).unwrap(), r#""BUY""#);
    }

    #[test]
    fn order_type_accepts_uppercase_and_lowercase() {
        assert_eq!(
            serde_json::from_str::<OrderType>(r#""LIMIT""#).unwrap(),
            OrderType::Limit
        );
        assert_eq!(
            serde_json::from_str::<OrderType>(r#""limit""#).unwrap(),
            OrderType::Limit
        );
        assert_eq!(
            serde_json::from_str::<OrderType>(r#""stop_limit""#).unwrap(),
            OrderType::StopLimit
        );
    }

    #[test]
    fn time_in_force_accepts_uppercase_and_lowercase() {
        assert_eq!(
            serde_json::from_str::<TimeInForce>(r#""GTC""#).unwrap(),
            TimeInForce::Gtc
        );
        assert_eq!(
            serde_json::from_str::<TimeInForce>(r#""gtc""#).unwrap(),
            TimeInForce::Gtc
        );
    }
}
