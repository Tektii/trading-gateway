//! Smoke tests for the gateway test harness (Story 6.5.1).
//!
//! Validates that the test harness correctly spins up a full gateway and that
//! REST, WebSocket, and event injection all work.

use std::time::Duration;

use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_test_support::harness::{StrategyClient, spawn_test_gateway};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::{test_order, test_quote};

const TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Health endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn livez_returns_200() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = reqwest::get(format!("{}/livez", gw.root_url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn readyz_returns_200_when_adapter_registered() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = reqwest::get(format!("{}/readyz", gw.root_url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// REST API via mock adapter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rest_get_orders_returns_200() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/orders", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(
        body.is_empty(),
        "No orders configured — empty list expected"
    );
}

#[tokio::test]
async fn rest_get_orders_with_preconfigured_order() {
    let order = test_order();
    let order_id = order.id.clone();

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_order(order);
    let gw = spawn_test_gateway(adapter).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/orders/{}", gw.base_url(), order_id))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], order_id);
}

#[tokio::test]
async fn rest_get_quote_returns_configured_quote() {
    let adapter =
        MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_quote(test_quote("AAPL"));
    let gw = spawn_test_gateway(adapter).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/quotes/AAPL", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["symbol"], "AAPL");
}

#[tokio::test]
async fn rest_get_quote_not_found() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/quotes/NOPE", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

// ---------------------------------------------------------------------------
// WebSocket + event injection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_client_connects_successfully() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let _client = StrategyClient::connect(&gw).await;

    // Connection succeeds — if it panicked, the test would fail.
    // Give server time to register the connection.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(gw.state.ws_connection_manager().connection_count().await, 1);
}

#[tokio::test]
async fn injected_event_reaches_strategy_client() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let mut client = StrategyClient::connect(&gw).await;

    // Wait for connection to be registered
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Also register as a strategy connection so events flow
    // (The WS handler already does this via handle_socket)

    // Inject a quote event
    let quote = test_quote("AAPL");
    gw.inject_event(tektii_gateway_core::websocket::messages::WsMessage::quote(
        quote,
    ));

    // Strategy should receive the event
    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "Expected to receive the injected event");
}
