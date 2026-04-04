//! End-to-end strategy reconnection tests (Story 6.5.4).
//!
//! Verifies that when a strategy disconnects and reconnects to the gateway:
//! - Broker state (orders, positions) is preserved (no cancel calls made)
//! - REST queries return accurate state after reconnection
//! - WebSocket events resume flowing to the reconnected client

use std::sync::Arc;
use std::time::{Duration, Instant};

use rust_decimal_macros::dec;
use tektii_gateway_core::models::{
    OrderHandle, OrderStatus, OrderType, PositionSide, Side, TradingPlatform,
};
use tektii_gateway_core::websocket::connection::WsConnectionManager;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_test_support::harness::{StrategyClient, spawn_test_gateway_with_adapter};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::{test_order, test_position, test_quote};

const TIMEOUT: Duration = Duration::from_secs(2);

fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Poll `connection_count()` until it reaches `expected`, or panic after `deadline`.
async fn wait_for_connection_count(
    manager: &WsConnectionManager,
    expected: usize,
    deadline: Duration,
) {
    let start = Instant::now();
    loop {
        let count = manager.connection_count().await;
        if count == expected {
            return;
        }
        assert!(
            start.elapsed() <= deadline,
            "Timed out after {deadline:?} waiting for connection_count == {expected} (actual: {count})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll `connected_strategy_count()` until it reaches `expected`, or panic after `deadline`.
async fn wait_for_strategy_count(registry: &ProviderRegistry, expected: usize, deadline: Duration) {
    let start = Instant::now();
    loop {
        let count = registry.connected_strategy_count().await;
        if count == expected {
            return;
        }
        assert!(
            start.elapsed() <= deadline,
            "Timed out after {deadline:?} waiting for connected_strategy_count == {expected} (actual: {count})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_disconnect_preserves_state_and_reconnect_resumes_events() {
    // --- Setup: adapter with submit response, pre-populated order + position ---

    let handle = OrderHandle {
        id: "ord-reconnect-1".into(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    };

    let mut open_order = test_order();
    open_order.id = "ord-reconnect-1".into();
    open_order.symbol = "AAPL".into();
    open_order.side = Side::Buy;
    open_order.order_type = OrderType::Market;
    open_order.quantity = dec!(10);
    open_order.status = OrderStatus::Open;

    let mut position = test_position();
    position.symbol = "AAPL".into();
    position.side = PositionSide::Long;
    position.quantity = dec!(10);

    let adapter = Arc::new(
        MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
            .with_submit_order_response(Ok(handle))
            .with_order(open_order)
            .with_position(position),
    );
    let gw = spawn_test_gateway_with_adapter(Arc::clone(&adapter)).await;

    // --- Phase 1: Connect and place order ---

    let client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = http_client()
        .post(format!("{}/orders", gw.base_url()))
        .json(&serde_json::json!({
            "symbol": "AAPL",
            "side": "BUY",
            "order_type": "MARKET",
            "quantity": "10"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "order submission should succeed");
    assert_eq!(
        adapter.submitted_orders().len(),
        1,
        "adapter should have received the order"
    );

    // --- Phase 2: Disconnect — verify no cancel calls ---

    client.close().await;
    wait_for_connection_count(gw.state.ws_connection_manager(), 0, TIMEOUT).await;
    wait_for_strategy_count(gw.state.provider_registry(), 0, TIMEOUT).await;

    assert!(
        adapter.cancelled_orders().is_empty(),
        "disconnect must NOT trigger cancel calls; got: {:?}",
        adapter.cancelled_orders()
    );

    // --- Phase 3: Reconnect ---

    let mut new_client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        gw.state.ws_connection_manager().connection_count().await,
        1,
        "reconnected client should be registered"
    );
    assert_eq!(
        gw.state
            .provider_registry()
            .connected_strategy_count()
            .await,
        1,
        "reconnected strategy should be registered"
    );

    // --- Phase 4: REST queries show accurate state ---

    let resp = http_client()
        .get(format!("{}/orders", gw.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let orders = body.as_array().expect("orders should be an array");
    assert!(
        orders.iter().any(|o| o["id"] == "ord-reconnect-1"),
        "order should persist after disconnect/reconnect; got: {body}"
    );

    let resp = http_client()
        .get(format!("{}/positions", gw.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let positions = body.as_array().expect("positions should be an array");
    assert!(
        positions.iter().any(|p| p["symbol"] == "AAPL"),
        "position should persist after disconnect/reconnect; got: {body}"
    );

    // --- Phase 5: Events resume flowing ---

    gw.inject_event(WsMessage::quote(test_quote("AAPL")));

    let msg = new_client.recv_message(TIMEOUT).await;
    assert!(
        msg.is_some(),
        "reconnected client should receive events via WS"
    );

    if let Some(WsMessage::QuoteData { quote, .. }) = msg {
        assert_eq!(quote.symbol, "AAPL");
    } else {
        panic!("expected WsMessage::QuoteData, got: {msg:?}");
    }

    // Verify still no cancel calls after full scenario
    assert!(
        adapter.cancelled_orders().is_empty(),
        "no cancel calls should have been made at any point"
    );
}
