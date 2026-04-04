//! Integration tests for trailing stop registration and tracking.
//!
//! Tests the `TrailingStopHandler` registration flow, entry lifecycle,
//! and cancellation via `MockPriceSource`.

use std::sync::Arc;

use rust_decimal_macros::dec;

use tektii_gateway_core::models::{OrderRequest, OrderType, Side, TimeInForce, TradingPlatform};
use tektii_gateway_core::trailing_stop::{
    StopOrderExecutor, TrailingEntryStatus, TrailingStopConfig, TrailingStopHandler,
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

#[tokio::test]
async fn register_creates_entry() {
    let source = Arc::new(MockPriceSource::new());
    source.set_price("BTCUSD", dec!(50000));

    let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
    let handler =
        TrailingStopHandler::new_arc(source, executor, PLATFORM, TrailingStopConfig::default());

    let result = handler.register("primary-1", &trailing_stop_order("BTCUSD"));
    assert!(result.is_ok(), "Registration should succeed");

    let registered = result.unwrap();
    assert!(registered.placeholder_id.starts_with("trailing:primary-1"));
    assert_eq!(handler.entry_count(), 1);
}

#[tokio::test]
async fn register_creates_pending_entry() {
    // Registration always creates entry in Pending state
    let source = Arc::new(MockPriceSource::new());
    let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
    let handler =
        TrailingStopHandler::new_arc(source, executor, PLATFORM, TrailingStopConfig::default());

    let result = handler.register("primary-2", &trailing_stop_order("UNKNOWN"));

    assert!(result.is_ok());
    let entry = handler
        .get_entry(&result.unwrap().placeholder_id)
        .expect("entry should exist");
    assert!(
        matches!(entry.status, TrailingEntryStatus::Pending),
        "Expected Pending, got {:?}",
        entry.status
    );
}

#[tokio::test]
async fn primary_fill_without_price_enters_unavailable() {
    // After primary fills with no price available → PriceSourceUnavailable
    let source = Arc::new(MockPriceSource::new());
    let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
    let config = TrailingStopConfig {
        activation_timeout: std::time::Duration::from_millis(10),
        ..TrailingStopConfig::default()
    };
    let handler = TrailingStopHandler::new_arc(source, executor, PLATFORM, config);

    let result = handler
        .register("primary-2", &trailing_stop_order("UNKNOWN"))
        .unwrap();

    handler
        .on_primary_filled("primary-2", dec!(50000))
        .await
        .unwrap();

    let entry = handler
        .get_entry(&result.placeholder_id)
        .expect("entry should exist");
    assert!(
        matches!(
            entry.status,
            TrailingEntryStatus::PriceSourceUnavailable { .. }
        ),
        "Expected PriceSourceUnavailable after fill without price, got {:?}",
        entry.status
    );
}

#[tokio::test]
async fn cancel_transitions_to_cancelled() {
    let source = Arc::new(MockPriceSource::new());
    source.set_price("BTCUSD", dec!(50000));

    let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
    let handler =
        TrailingStopHandler::new_arc(source, executor, PLATFORM, TrailingStopConfig::default());
    let registered = handler
        .register("primary-3", &trailing_stop_order("BTCUSD"))
        .unwrap();

    let cancelled = handler
        .cancel(
            &registered.placeholder_id,
            tektii_gateway_core::trailing_stop::types::CancellationReason::UserRequested,
        )
        .unwrap();
    assert!(cancelled, "cancel() should return true for active entry");

    let entry = handler
        .get_entry(&registered.placeholder_id)
        .expect("entry should still exist");
    assert!(
        matches!(entry.status, TrailingEntryStatus::Cancelled { .. }),
        "Expected Cancelled, got {:?}",
        entry.status
    );
}

#[tokio::test]
async fn rejects_non_trailing_stop_order() {
    let source = Arc::new(MockPriceSource::new());
    let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
    let handler =
        TrailingStopHandler::new_arc(source, executor, PLATFORM, TrailingStopConfig::default());

    let mut order = trailing_stop_order("BTCUSD");
    order.order_type = OrderType::Market; // Wrong type

    let result = handler.register("primary-4", &order);
    assert!(result.is_err(), "Should reject non-TrailingStop order");
}

#[tokio::test]
async fn max_entries_enforced() {
    let source = Arc::new(MockPriceSource::new());
    source.set_price("BTCUSD", dec!(50000));

    let config = TrailingStopConfig {
        max_pending_entries: 2,
        ..TrailingStopConfig::default()
    };
    let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
    let handler = TrailingStopHandler::new_arc(source, executor, PLATFORM, config);

    handler
        .register("p1", &trailing_stop_order("BTCUSD"))
        .unwrap();
    handler
        .register("p2", &trailing_stop_order("BTCUSD"))
        .unwrap();

    let result = handler.register("p3", &trailing_stop_order("BTCUSD"));
    assert!(result.is_err(), "Should reject when at capacity");
}
