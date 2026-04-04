//! Concurrent order submission tests (Checklist #27).
//!
//! Verifies the gateway's shared state holds up under parallel access:
//! DashMap-based StateManager, CorrelationStore, and WsConnectionManager.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rust_decimal_macros::dec;
use tektii_gateway_core::models::{OrderStatus, TradingPlatform};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_test_support::harness::{
    StrategyClient, spawn_test_gateway, spawn_test_gateway_with_adapter,
};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::{test_order, test_position};

const TIMEOUT: Duration = Duration::from_secs(2);

fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

fn order_json() -> serde_json::Value {
    serde_json::json!({
        "symbol": "AAPL",
        "side": "BUY",
        "order_type": "MARKET",
        "quantity": "1"
    })
}

// ---------------------------------------------------------------------------
// Test 1: Concurrent order submissions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_order_submissions_all_succeed() {
    let adapter = Arc::new(MockTradingAdapter::new(TradingPlatform::AlpacaPaper));
    let gw = spawn_test_gateway_with_adapter(Arc::clone(&adapter)).await;

    let n = 20;
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let base_url = gw.base_url();
    let client = http_client();

    let handles: Vec<_> = (0..n)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let base_url = base_url.clone();
            let client = client.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                client
                    .post(format!("{base_url}/orders"))
                    .json(&order_json())
                    .send()
                    .await
            })
        })
        .collect();

    let results = tokio::time::timeout(Duration::from_secs(10), futures::future::join_all(handles))
        .await
        .expect("timed out waiting for concurrent submissions");

    let mut order_ids = HashSet::new();
    let mut correlation_ids = HashSet::new();

    for result in results {
        let resp = result.expect("task panicked").expect("request failed");
        assert_eq!(resp.status(), 201, "expected 201 for order submission");

        let body: serde_json::Value = resp.json().await.unwrap();
        let id = body["id"].as_str().unwrap().to_string();
        let corr = body["correlation_id"].as_str().unwrap().to_string();
        order_ids.insert(id);
        correlation_ids.insert(corr);
    }

    assert_eq!(order_ids.len(), n, "all order IDs should be unique");
    assert_eq!(
        correlation_ids.len(),
        n,
        "all correlation IDs should be unique"
    );
    assert_eq!(
        adapter.submitted_orders().len(),
        n,
        "adapter should have received all {n} orders"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Concurrent submissions + fill events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_submissions_with_fill_events() {
    let adapter = Arc::new(MockTradingAdapter::new(TradingPlatform::AlpacaPaper));
    let gw = spawn_test_gateway_with_adapter(Arc::clone(&adapter)).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let base_url = gw.base_url();
    let http = http_client();

    // Spawn 10 REST submissions
    let submit_handles: Vec<_> = (0..10)
        .map(|_| {
            let base_url = base_url.clone();
            let http = http.clone();
            tokio::spawn(async move {
                http.post(format!("{base_url}/orders"))
                    .json(&order_json())
                    .send()
                    .await
            })
        })
        .collect();

    // Simultaneously inject 10 fill events
    for i in 0..10 {
        let mut order = test_order();
        order.id = format!("fill-{i}");
        order.status = OrderStatus::Filled;
        order.filled_quantity = dec!(1);
        order.remaining_quantity = dec!(0);
        gw.inject_event(WsMessage::Order {
            event: OrderEventType::OrderFilled,
            order,
            parent_order_id: None,
            timestamp: Utc::now(),
        });
    }

    // Verify all submissions succeeded
    let submit_results = futures::future::join_all(submit_handles).await;
    for result in submit_results {
        let resp = result.expect("task panicked").expect("request failed");
        assert_eq!(resp.status(), 201);
    }
    assert_eq!(adapter.submitted_orders().len(), 10);

    // Collect fill events from WS
    let mut received_ids = HashSet::new();
    for _ in 0..10 {
        if let Some(WsMessage::Order { order, .. }) = client.recv_message(TIMEOUT).await {
            received_ids.insert(order.id.clone());
        }
    }

    let expected: HashSet<String> = (0..10).map(|i| format!("fill-{i}")).collect();
    assert_eq!(
        received_ids, expected,
        "strategy should receive all 10 fill events"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Concurrent reads during event processing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_reads_during_event_processing() {
    let mut adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper);
    for i in 0..5 {
        let mut order = test_order();
        order.id = format!("preload-ord-{i}");
        order.status = OrderStatus::Open;
        adapter = adapter.with_order(order);
    }
    for i in 0..3 {
        let mut pos = test_position();
        pos.id = format!("preload-pos-{i}");
        adapter = adapter.with_position(pos);
    }

    let gw = spawn_test_gateway(adapter).await;
    let base_url = gw.base_url();
    let http = http_client();

    // Inject 20 fill events rapidly
    let inject_handle = {
        let event_tx = gw.event_tx.clone();
        tokio::spawn(async move {
            for i in 0..20 {
                let mut order = test_order();
                order.id = format!("inject-{i}");
                order.status = OrderStatus::Filled;
                order.filled_quantity = dec!(1);
                order.remaining_quantity = dec!(0);
                let _ = event_tx.send(WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    order,
                    parent_order_id: None,
                    timestamp: Utc::now(),
                });
            }
        })
    };

    // Simultaneously fire 20 GET requests
    let read_handles: Vec<_> = (0..20)
        .map(|i| {
            let base_url = base_url.clone();
            let http = http.clone();
            tokio::spawn(async move {
                let endpoint = if i % 2 == 0 { "orders" } else { "positions" };
                http.get(format!("{base_url}/{endpoint}")).send().await
            })
        })
        .collect();

    inject_handle.await.expect("inject task panicked");
    let read_results = futures::future::join_all(read_handles).await;

    for result in read_results {
        let resp = result.expect("task panicked").expect("request failed");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body.is_array(), "response should be a JSON array");
    }
}

// ---------------------------------------------------------------------------
// Test 4: Multiple WS clients receive all events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_ws_clients_receive_all_events() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let num_clients = 5;
    let num_events = 10;

    // Connect all clients
    let mut clients = Vec::with_capacity(num_clients);
    for _ in 0..num_clients {
        clients.push(StrategyClient::connect(&gw).await);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Inject events
    for i in 0..num_events {
        let mut order = test_order();
        order.id = format!("multi-{i}");
        order.status = OrderStatus::Filled;
        order.filled_quantity = dec!(1);
        order.remaining_quantity = dec!(0);
        gw.inject_event(WsMessage::Order {
            event: OrderEventType::OrderFilled,
            order,
            parent_order_id: None,
            timestamp: Utc::now(),
        });
    }

    // Each client collects events in its own task
    let client_handles: Vec<_> = clients
        .into_iter()
        .enumerate()
        .map(|(client_idx, mut client)| {
            tokio::spawn(async move {
                let mut ids = HashSet::new();
                for _ in 0..num_events {
                    match client.recv_message(TIMEOUT).await {
                        Some(WsMessage::Order { order, .. }) => {
                            ids.insert(order.id.clone());
                        }
                        other => panic!("client {client_idx}: unexpected message: {other:?}"),
                    }
                }
                ids
            })
        })
        .collect();

    let expected: HashSet<String> = (0..num_events).map(|i| format!("multi-{i}")).collect();

    let results = tokio::time::timeout(
        Duration::from_secs(10),
        futures::future::join_all(client_handles),
    )
    .await
    .expect("timed out waiting for WS clients");

    for (i, result) in results.into_iter().enumerate() {
        let ids = result.expect("client task panicked");
        assert_eq!(
            ids, expected,
            "client {i} should receive all {num_events} events"
        );
    }
}
