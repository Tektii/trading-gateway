//! Integration tests for trailing stop handler wiring with executor.
//!
//! Tests that `TrailingStopHandler` correctly interacts with
//! `MockStopOrderExecutor` when configured, and that the
//! `new_arc` constructor produces a functional handler.

use std::sync::Arc;

use rust_decimal_macros::dec;

use tektii_gateway_core::models::{OrderRequest, OrderType, Side, TimeInForce, TradingPlatform};
use tektii_gateway_core::trailing_stop::{
    StopOrderExecutor, TrailingStopConfig, TrailingStopHandler,
};
use tektii_gateway_test_support::mock_executor::MockStopOrderExecutor;
use tektii_gateway_test_support::mock_price_source::MockPriceSource;

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;

fn trailing_stop_order(symbol: &str) -> OrderRequest {
    OrderRequest {
        symbol: symbol.into(),
        side: Side::Sell,
        quantity: dec!(1),
        order_type: OrderType::TrailingStop,
        trailing_distance: Some(dec!(5)),
        trailing_type: Some(tektii_gateway_core::models::TrailingType::Percent),
        limit_price: None,
        stop_price: None,
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

fn mock_executor() -> Arc<dyn StopOrderExecutor> {
    Arc::new(MockStopOrderExecutor::new())
}

#[tokio::test]
async fn handler_new_arc_creates_valid_instance() {
    let source = Arc::new(MockPriceSource::new());
    let handler = TrailingStopHandler::new_arc(
        source,
        mock_executor(),
        PLATFORM,
        TrailingStopConfig::default(),
    );

    assert_eq!(handler.platform(), PLATFORM);
    assert_eq!(handler.entry_count(), 0);
}

#[tokio::test]
async fn handler_config_max_entries_applied() {
    let source = Arc::new(MockPriceSource::new());
    source.set_price("BTCUSD", dec!(50000));

    let config = TrailingStopConfig {
        max_pending_entries: 1,
        ..TrailingStopConfig::default()
    };
    let handler = TrailingStopHandler::new_arc(source, mock_executor(), PLATFORM, config);

    // First registration succeeds
    handler
        .register("p1", &trailing_stop_order("BTCUSD"))
        .expect("first registration should succeed");

    // Second registration fails (at capacity)
    let result = handler.register("p2", &trailing_stop_order("BTCUSD"));
    assert!(result.is_err(), "Should reject when at max capacity");
}

#[tokio::test]
async fn mock_executor_records_place_calls() {
    let executor = Arc::new(MockStopOrderExecutor::new());

    let request = tektii_gateway_core::trailing_stop::StopOrderRequest {
        symbol: "BTCUSD".into(),
        side: Side::Sell,
        quantity: dec!(1),
        stop_price: dec!(47500),
        limit_price: dec!(47400),
    };

    let result = executor.place_stop_order(&request).await;
    assert!(result.is_ok());

    let placed = result.unwrap();
    assert!(placed.order_id.starts_with("mock-"));
    assert_eq!(placed.stop_price, dec!(47500));
    assert_eq!(executor.place_call_count(), 1);
}

#[tokio::test]
async fn mock_executor_records_modify_calls() {
    let executor = Arc::new(MockStopOrderExecutor::new());

    let request = tektii_gateway_core::trailing_stop::ModifyStopRequest {
        order_id: "mock-1".into(),
        symbol: "BTCUSD".into(),
        side: Side::Sell,
        quantity: dec!(1),
        new_stop_price: dec!(48000),
        new_limit_price: dec!(47900),
    };

    let result = executor.modify_stop_order(&request).await;
    assert!(result.is_ok());
    assert_eq!(executor.modify_call_count(), 1);
}

#[tokio::test]
async fn mock_executor_configurable_errors() {
    let executor = Arc::new(MockStopOrderExecutor::new());

    executor.set_place_error(tektii_gateway_core::error::GatewayError::ProviderError {
        message: "simulated failure".into(),
        provider: Some("test".into()),
        source: None,
    });

    let request = tektii_gateway_core::trailing_stop::StopOrderRequest {
        symbol: "BTCUSD".into(),
        side: Side::Sell,
        quantity: dec!(1),
        stop_price: dec!(47500),
        limit_price: dec!(47400),
    };

    let result = executor.place_stop_order(&request).await;
    assert!(result.is_err(), "Should fail with configured error");

    // Subsequent call succeeds (error was consumed)
    let result = executor.place_stop_order(&request).await;
    assert!(result.is_ok(), "Should succeed after error consumed");
}

#[tokio::test]
async fn circuit_breaker_starts_closed() {
    let source = Arc::new(MockPriceSource::new());
    let handler = TrailingStopHandler::new_arc(
        source,
        mock_executor(),
        PLATFORM,
        TrailingStopConfig::default(),
    );

    assert!(
        !handler.is_circuit_breaker_open(),
        "Circuit breaker should start closed"
    );
}
