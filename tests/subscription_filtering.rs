//! Integration tests for subscription filtering with real `WsMessage` types.
//!
//! Verifies that `SubscriptionFilter` correctly gates event delivery when
//! wired into the broadcast pipeline. Complements the unit tests in
//! `core/src/subscription/filter.rs` which test string-level matching.

use chrono::Utc;

use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::messages::{
    ConnectionEventType, InstrumentSubscription, OrderEventType, PositionEventType, WsErrorCode,
    WsMessage,
};
use tektii_gateway_test_support::models::{test_order, test_position, test_quote};

fn platform() -> TradingPlatform {
    TradingPlatform::AlpacaPaper
}

fn order_msg(symbol: &str) -> WsMessage {
    let order = tektii_gateway_core::models::Order {
        symbol: symbol.into(),
        ..test_order()
    };
    WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    }
}

fn position_msg(symbol: &str) -> WsMessage {
    let position = tektii_gateway_core::models::Position {
        symbol: symbol.into(),
        ..test_position()
    };
    WsMessage::Position {
        event: PositionEventType::PositionOpened,
        position,
        timestamp: Utc::now(),
    }
}

fn quote_msg(symbol: &str) -> WsMessage {
    WsMessage::QuoteData {
        quote: test_quote(symbol),
        timestamp: Utc::now(),
    }
}

fn connection_msg() -> WsMessage {
    WsMessage::Connection {
        event: ConnectionEventType::BrokerReconnected,
        error: None,
        broker: Some("alpaca-paper".into()),
        gap_duration_ms: Some(500),
        timestamp: Utc::now(),
    }
}

fn error_msg() -> WsMessage {
    WsMessage::error(WsErrorCode::InternalError, "test error")
}

#[test]
fn subscribed_order_event_passes_filter() {
    let subs = vec![InstrumentSubscription::new(
        platform(),
        "AAPL".into(),
        vec!["order_update".into()],
    )];
    let filter = SubscriptionFilter::new(&subs);

    assert!(filter.matches(&order_msg("AAPL"), platform()));
}

#[test]
fn unsubscribed_event_blocked() {
    let subs = vec![InstrumentSubscription::new(
        platform(),
        "AAPL".into(),
        vec!["quote".into()],
    )];
    let filter = SubscriptionFilter::new(&subs);

    // Subscribed to quotes, not orders — order event should be blocked
    assert!(!filter.matches(&order_msg("AAPL"), platform()));
}

#[test]
fn system_events_always_pass() {
    let subs = vec![InstrumentSubscription::new(
        platform(),
        "AAPL".into(),
        vec!["quote".into()],
    )];
    let filter = SubscriptionFilter::new(&subs);

    // Connection events always pass regardless of subscriptions
    assert!(filter.matches(&connection_msg(), platform()));
    assert!(filter.matches(&error_msg(), platform()));
    assert!(filter.matches(&WsMessage::ping(), platform()));
}

#[test]
fn wildcard_instrument_receives_all_symbols() {
    let subs = vec![InstrumentSubscription::new(
        platform(),
        "*".into(),
        vec!["order_update".into()],
    )];
    let filter = SubscriptionFilter::new(&subs);

    assert!(filter.matches(&order_msg("AAPL"), platform()));
    assert!(filter.matches(&order_msg("BTCUSD"), platform()));
    assert!(filter.matches(&order_msg("EURUSD"), platform()));
}

#[test]
fn wildcard_event_receives_all_event_types() {
    let subs = vec![InstrumentSubscription::new(
        platform(),
        "AAPL".into(),
        vec!["*".into()],
    )];
    let filter = SubscriptionFilter::new(&subs);

    assert!(filter.matches(&order_msg("AAPL"), platform()));
    assert!(filter.matches(&quote_msg("AAPL"), platform()));
    assert!(filter.matches(&position_msg("AAPL"), platform()));

    // Different symbol should NOT match
    assert!(!filter.matches(&order_msg("BTCUSD"), platform()));
}

#[test]
fn empty_filter_passes_all_events() {
    // Empty filter = no static subscriptions configured → pass everything
    let filter = SubscriptionFilter::new(&[]);

    assert!(filter.matches(&order_msg("AAPL"), platform()));
    assert!(filter.matches(&quote_msg("BTCUSD"), platform()));
    assert!(filter.matches(&connection_msg(), platform()));
}

#[test]
fn wrong_platform_blocks_event() {
    let subs = vec![InstrumentSubscription::new(
        TradingPlatform::AlpacaPaper,
        "AAPL".into(),
        vec!["order_update".into()],
    )];
    let filter = SubscriptionFilter::new(&subs);

    // Event for a different platform should be blocked
    assert!(!filter.matches(&order_msg("AAPL"), TradingPlatform::AlpacaLive));
}

#[test]
fn wrong_symbol_blocks_event() {
    let subs = vec![InstrumentSubscription::new(
        platform(),
        "AAPL".into(),
        vec!["order_update".into()],
    )];
    let filter = SubscriptionFilter::new(&subs);

    assert!(!filter.matches(&order_msg("MSFT"), platform()));
}

#[test]
fn multiple_subscriptions_combined() {
    let subs = vec![
        InstrumentSubscription::new(platform(), "AAPL".into(), vec!["order_update".into()]),
        InstrumentSubscription::new(platform(), "BTCUSD".into(), vec!["quote".into()]),
    ];
    let filter = SubscriptionFilter::new(&subs);

    assert!(filter.matches(&order_msg("AAPL"), platform()));
    assert!(filter.matches(&quote_msg("BTCUSD"), platform()));

    // Cross: AAPL quote not subscribed, BTCUSD order not subscribed
    assert!(!filter.matches(&quote_msg("AAPL"), platform()));
    assert!(!filter.matches(&order_msg("BTCUSD"), platform()));
}
