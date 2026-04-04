//! Integration tests for Prometheus metrics recording.
//!
//! Verifies that gateway operations produce the correct Prometheus counters,
//! gauges, and histograms. Uses `MetricsHandle::try_init()` since the global
//! recorder can only be installed once per process.

use std::sync::Arc;

use rust_decimal_macros::dec;
use tokio::sync::broadcast;

use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::ExitHandler;
use tektii_gateway_core::metrics::MetricsHandle;
use tektii_gateway_core::models::{OrderStatus, TradingPlatform};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_test_support::models::test_order;

fn create_router() -> (EventRouter, broadcast::Receiver<WsMessage>) {
    let state_manager = Arc::new(StateManager::new());
    let exit_handler: Arc<dyn tektii_gateway_core::exit_management::ExitHandling> = Arc::new(
        ExitHandler::with_defaults(Arc::clone(&state_manager), TradingPlatform::AlpacaPaper),
    );
    let (tx, rx) = broadcast::channel(64);
    let router = EventRouter::new(
        state_manager,
        exit_handler,
        tx,
        TradingPlatform::AlpacaPaper,
    );
    (router, rx)
}

/// Single test function because the Prometheus recorder is process-global.
/// `try_init()` returns `None` if another test binary already installed one.
#[tokio::test]
async fn metrics_recorded_through_event_router() {
    let Some(handle) = MetricsHandle::try_init() else {
        // Another test binary installed the recorder — skip gracefully
        return;
    };

    let (router, _rx) = create_router();

    // Process a filled order event through the router
    let order = tektii_gateway_core::models::Order {
        status: OrderStatus::Filled,
        filled_quantity: dec!(1),
        remaining_quantity: dec!(0),
        ..test_order()
    };

    router
        .handle_order_event(OrderEventType::OrderFilled, &order, None)
        .await;

    let output = handle.render();

    // Verify order events counter includes the platform label and event type
    assert!(
        output.contains("gateway_order_events_total"),
        "Missing gateway_order_events_total in metrics output:\n{output}"
    );

    // Check platform label — the header_value() format may use hyphens
    let has_platform = output.contains("alpaca_paper") || output.contains("alpaca-paper");
    assert!(
        has_platform,
        "Missing platform label in metrics output:\n{output}"
    );
    assert!(
        output.contains("filled"),
        "Missing event_type label 'filled' in metrics output"
    );

    // Process a cancellation to verify different event types are tracked separately
    let cancelled_order = tektii_gateway_core::models::Order {
        id: "cancelled-order-1".into(),
        status: OrderStatus::Cancelled,
        ..test_order()
    };
    router.handle_order_cancelled(&cancelled_order).await;

    let output = handle.render();
    assert!(
        output.contains("cancelled"),
        "Missing event_type label 'cancelled' after processing cancellation"
    );

    // Record a histogram value directly to verify bucket configuration
    metrics::histogram!(
        "gateway_order_submit_duration_seconds",
        "platform" => "alpaca-paper",
    )
    .record(0.042);

    let output = handle.render();
    assert!(
        output.contains("gateway_order_submit_duration_seconds"),
        "Missing histogram after recording a value:\n{output}"
    );
}
