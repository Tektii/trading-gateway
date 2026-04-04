//! End-to-end health and observability endpoint tests (Story 6.5.5).
//!
//! Tests `/health`, `/readyz`, and `/metrics` through the full
//! gateway HTTP stack. Metrics tests are consolidated into a single test
//! function because the Prometheus recorder is process-global.

use std::time::Duration;

use tektii_gateway_core::models::{OrderHandle, OrderStatus, TradingPlatform};
use tektii_gateway_test_support::harness::{
    StrategyClient, spawn_test_gateway, spawn_test_gateway_with_metrics,
};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;

fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

// ---------------------------------------------------------------------------
// /health endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_returns_connected_with_provider_details() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = http_client()
        .get(format!("{}/health", gw.root_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "connected");

    let providers = body["providers"]
        .as_array()
        .expect("providers should be an array");
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["platform"], "alpaca-paper");
    assert_eq!(providers[0]["connected"], true);
    // No stale instruments — field should be absent or empty
    let stale = &providers[0]["stale_instruments"];
    assert!(
        stale.is_null() || stale.as_array().is_some_and(|a| a.is_empty()),
        "Expected no stale instruments, got: {stale}"
    );
}

// ---------------------------------------------------------------------------
// /readyz endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn readyz_body_contains_provider_list() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = http_client()
        .get(format!("{}/readyz", gw.root_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ready"], true);
    assert_eq!(body["provider_count"], 1);

    let providers = body["providers"].as_array().expect("providers array");
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0], "alpaca-paper");
}

// ---------------------------------------------------------------------------
// /metrics endpoint + metrics increments
// ---------------------------------------------------------------------------
// Process-global Prometheus recorder — all metrics assertions in one test.

#[tokio::test]
async fn metrics_endpoint_and_increments() {
    let handle = OrderHandle {
        id: "ord-metrics-1".into(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    };

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
        .with_submit_order_response(Ok(handle));

    let Some(gw) = spawn_test_gateway_with_metrics(adapter).await else {
        // Global recorder already claimed — skip gracefully
        return;
    };

    // -- 1. /metrics returns Prometheus text exposition format
    let resp = http_client()
        .get(format!("{}/metrics", gw.root_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Prometheus text format always has TYPE lines
    // (may be empty if no metrics recorded yet, but the endpoint must be alive)
    assert!(
        body.is_empty() || body.contains("# TYPE") || body.contains("# HELP"),
        "Expected Prometheus text format, got:\n{body}"
    );

    // -- 2. Submit an order and verify counter/histogram appear
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
    assert_eq!(resp.status(), 201);

    // Read metrics via the handle (avoids HTTP roundtrip race)
    let output = gw.metrics_output().expect("metrics handle present");
    assert!(
        output.contains("gateway_orders_submitted_total"),
        "Missing gateway_orders_submitted_total after order submit:\n{output}"
    );
    assert!(
        output.contains("gateway_order_submit_duration_seconds"),
        "Missing gateway_order_submit_duration_seconds after order submit:\n{output}"
    );
    // Platform label should be present
    let has_platform = output.contains("alpaca_paper") || output.contains("alpaca-paper");
    assert!(has_platform, "Missing platform label in metrics:\n{output}");

    // Also verify via HTTP endpoint
    let resp = http_client()
        .get(format!("{}/metrics", gw.root_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let http_output = resp.text().await.unwrap();
    assert!(
        http_output.contains("gateway_orders_submitted_total"),
        "Missing counter in HTTP /metrics response"
    );

    // -- 3. WS connection gauge: connect, verify increment, disconnect, verify decrement
    let client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let output = gw.metrics_output().unwrap();
    assert!(
        output.contains("gateway_ws_connections_active"),
        "Missing WS connections gauge after connect:\n{output}"
    );
    // Gauge should show 1 (look for the metric line with value 1)
    assert!(
        output.contains("gateway_ws_connections_active 1"),
        "Expected WS gauge = 1, metrics:\n{output}"
    );

    // Disconnect and verify gauge drops
    client.close().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let output = gw.metrics_output().unwrap();
    assert!(
        output.contains("gateway_ws_connections_active 0"),
        "Expected WS gauge = 0 after disconnect, metrics:\n{output}"
    );
}
