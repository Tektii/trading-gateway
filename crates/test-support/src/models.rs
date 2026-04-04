//! Test fixture builders for core gateway model types.
//!
//! Each function returns a valid instance with sensible defaults.
//! Customize in tests via struct update syntax:
//!
//! ```ignore
//! let order = Order { symbol: "AAPL".into(), ..test_order() };
//! ```

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use uuid::Uuid;

use tektii_gateway_core::models::{
    Account, Bar, Order, OrderHandle, OrderRequest, OrderStatus, OrderType, Position, PositionSide,
    Quote, Side, TimeInForce, Timeframe, Trade,
};

/// A default `Order` (market buy BTCUSD, qty 1, status Open).
#[must_use]
pub fn test_order() -> Order {
    let now = Utc::now();
    Order {
        id: Uuid::new_v4().to_string(),
        client_order_id: None,
        symbol: "BTCUSD".into(),
        side: Side::Buy,
        order_type: OrderType::Market,
        quantity: dec!(1),
        filled_quantity: Decimal::ZERO,
        remaining_quantity: dec!(1),
        limit_price: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        average_fill_price: None,
        status: OrderStatus::Open,
        reject_reason: None,
        position_id: None,
        reduce_only: None,
        post_only: None,
        hidden: None,
        display_quantity: None,
        oco_group_id: None,
        correlation_id: None,
        time_in_force: TimeInForce::Gtc,
        created_at: now,
        updated_at: now,
    }
}

/// A default `OrderHandle` (Open status).
#[must_use]
pub fn test_order_handle() -> OrderHandle {
    OrderHandle {
        id: Uuid::new_v4().to_string(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    }
}

/// A default `Account` (100k USD balance, no margin used).
#[must_use]
pub fn test_account() -> Account {
    Account {
        balance: dec!(100_000),
        equity: dec!(100_000),
        margin_used: Decimal::ZERO,
        margin_available: dec!(100_000),
        unrealized_pnl: Decimal::ZERO,
        currency: "USD".into(),
    }
}

/// A default `Position` (long BTCUSD, qty 1, entry/current 50000, flat PnL).
#[must_use]
pub fn test_position() -> Position {
    let now = Utc::now();
    Position {
        id: Uuid::new_v4().to_string(),
        symbol: "BTCUSD".into(),
        side: PositionSide::Long,
        quantity: dec!(1),
        average_entry_price: dec!(50_000),
        current_price: dec!(50_000),
        unrealized_pnl: Decimal::ZERO,
        realized_pnl: Decimal::ZERO,
        margin_mode: None,
        leverage: None,
        liquidation_price: None,
        opened_at: now,
        updated_at: now,
    }
}

/// A default `Quote` for the given symbol (bid=99, ask=101, last=100).
#[must_use]
pub fn test_quote(symbol: &str) -> Quote {
    Quote {
        symbol: symbol.into(),
        provider: "test".into(),
        bid: dec!(99),
        bid_size: None,
        ask: dec!(101),
        ask_size: None,
        last: dec!(100),
        volume: None,
        timestamp: Utc::now(),
    }
}

/// A default `Bar` for the given symbol (OHLC=100, volume=1000, 1-minute).
#[must_use]
pub fn test_bar(symbol: &str) -> Bar {
    Bar {
        symbol: symbol.into(),
        provider: "test".into(),
        timeframe: Timeframe::OneMinute,
        timestamp: Utc::now(),
        open: dec!(100),
        high: dec!(100),
        low: dec!(100),
        close: dec!(100),
        volume: dec!(1000),
    }
}

/// A default `OrderRequest` (market buy TEST, qty 1, GTC).
///
/// Customize via struct update syntax:
///
/// ```ignore
/// let req = OrderRequest { symbol: "BTCUSDT".into(), ..test_order_request() };
/// ```
#[must_use]
pub fn test_order_request() -> OrderRequest {
    OrderRequest {
        symbol: "TEST".into(),
        side: Side::Buy,
        quantity: Decimal::ONE,
        order_type: OrderType::Market,
        limit_price: None,
        stop_price: None,
        trailing_distance: None,
        trailing_type: None,
        stop_loss: None,
        take_profit: None,
        position_id: None,
        reduce_only: false,
        post_only: false,
        hidden: false,
        display_quantity: None,
        margin_mode: None,
        leverage: None,
        client_order_id: None,
        oco_group_id: None,
        time_in_force: TimeInForce::Gtc,
    }
}

/// A default `Trade` (buy BTCUSD, qty 1, price 50000, zero commission).
#[must_use]
pub fn test_trade() -> Trade {
    Trade {
        id: Uuid::new_v4().to_string(),
        order_id: Uuid::new_v4().to_string(),
        symbol: "BTCUSD".into(),
        side: Side::Buy,
        quantity: dec!(1),
        price: dec!(50_000),
        commission: Decimal::ZERO,
        commission_currency: "USD".into(),
        is_maker: None,
        timestamp: Utc::now(),
    }
}
