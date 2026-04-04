//! End-to-end order flow tests (Story 6.5.2).
//!
//! Tests the full order lifecycle through the gateway stack:
//! submit via REST, fill events via WebSocket, and queries reflecting updated state.

use std::time::Duration;

use chrono::Utc;
use rust_decimal_macros::dec;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderResult, OrderHandle, OrderStatus, OrderType, PositionSide, Side, TradingPlatform,
};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_test_support::harness::{StrategyClient, spawn_test_gateway};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::{test_order, test_position};

const TIMEOUT: Duration = Duration::from_secs(2);

fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

// ---------------------------------------------------------------------------
// Submit order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn submit_order_returns_201_with_handle() {
    let handle = OrderHandle {
        id: "ord-e2e-1".into(),
        client_order_id: Some("my-client-id".into()),
        correlation_id: None, // gateway assigns this
        status: OrderStatus::Open,
    };

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
        .with_submit_order_response(Ok(handle));
    let gw = spawn_test_gateway(adapter).await;

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
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "ord-e2e-1");
    assert_eq!(body["status"], "OPEN");
    // Gateway should assign a correlation_id even though adapter returned None
    assert!(
        body["correlation_id"].is_string(),
        "expected correlation_id to be set by gateway"
    );
}

#[tokio::test]
async fn submit_order_validation_rejects_missing_fields() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    // Missing required fields (no symbol, side, order_type, quantity)
    let resp = http_client()
        .post(format!("{}/orders", gw.base_url()))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

// ---------------------------------------------------------------------------
// Fill event via WebSocket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fill_event_reaches_strategy_via_websocket() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Inject an order filled event
    let mut filled_order = test_order();
    filled_order.id = "ord-fill-1".into();
    filled_order.symbol = "AAPL".into();
    filled_order.status = OrderStatus::Filled;
    filled_order.filled_quantity = dec!(10);
    filled_order.remaining_quantity = dec!(0);
    filled_order.average_fill_price = Some(dec!(155.50));

    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order: filled_order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "expected to receive fill event via WS");

    // Verify it's the correct event type
    if let Some(WsMessage::Order { event, order, .. }) = msg {
        assert_eq!(event, OrderEventType::OrderFilled);
        assert_eq!(order.id, "ord-fill-1");
        assert_eq!(order.symbol, "AAPL");
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.average_fill_price, Some(dec!(155.50)));
    } else {
        panic!("expected WsMessage::Order, got: {msg:?}");
    }
}

#[tokio::test]
async fn cancel_event_reaches_strategy_via_websocket() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut cancelled_order = test_order();
    cancelled_order.id = "ord-cancel-1".into();
    cancelled_order.status = OrderStatus::Cancelled;

    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderCancelled,
        order: cancelled_order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "expected to receive cancel event via WS");

    if let Some(WsMessage::Order { event, order, .. }) = msg {
        assert_eq!(event, OrderEventType::OrderCancelled);
        assert_eq!(order.id, "ord-cancel-1");
        assert_eq!(order.status, OrderStatus::Cancelled);
    } else {
        panic!("expected WsMessage::Order, got: {msg:?}");
    }
}

// ---------------------------------------------------------------------------
// Full round-trip: submit → fill → query
// ---------------------------------------------------------------------------

#[tokio::test]
async fn submit_then_fill_then_query_order() {
    let order_id = "ord-roundtrip-1";

    // Pre-configure: submit response + filled order for GET
    let handle = OrderHandle {
        id: order_id.into(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    };

    let mut filled_order = test_order();
    filled_order.id = order_id.into();
    filled_order.symbol = "AAPL".into();
    filled_order.side = Side::Buy;
    filled_order.order_type = OrderType::Market;
    filled_order.quantity = dec!(10);
    filled_order.status = OrderStatus::Filled;
    filled_order.filled_quantity = dec!(10);
    filled_order.remaining_quantity = dec!(0);
    filled_order.average_fill_price = Some(dec!(155.50));

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
        .with_submit_order_response(Ok(handle))
        .with_order(filled_order.clone());

    let gw = spawn_test_gateway(adapter).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Step 1: Submit order via REST
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

    // Step 2: Broker fills order → event reaches strategy
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order: filled_order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "expected fill event via WS");

    // Step 3: Query order status via REST → sees filled
    let resp = http_client()
        .get(format!("{}/orders/{}", gw.base_url(), order_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], order_id);
    assert_eq!(body["status"], "FILLED");
    assert_eq!(body["filled_quantity"], "10");
    assert_eq!(body["symbol"], "AAPL");
}

#[tokio::test]
async fn submit_then_fill_then_query_position() {
    let order_id = "ord-pos-1";

    let handle = OrderHandle {
        id: order_id.into(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    };

    let mut filled_order = test_order();
    filled_order.id = order_id.into();
    filled_order.symbol = "AAPL".into();
    filled_order.status = OrderStatus::Filled;
    filled_order.filled_quantity = dec!(10);
    filled_order.remaining_quantity = dec!(0);

    let mut position = test_position();
    position.id = "pos-aapl-1".into();
    position.symbol = "AAPL".into();
    position.side = PositionSide::Long;
    position.quantity = dec!(10);
    position.average_entry_price = dec!(155.50);

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
        .with_submit_order_response(Ok(handle))
        .with_order(filled_order.clone())
        .with_position(position);

    let gw = spawn_test_gateway(adapter).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Submit order
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

    // Broker fills order → event reaches strategy
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order: filled_order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "expected fill event via WS");

    // Query positions → sees the resulting position
    let resp = http_client()
        .get(format!("{}/positions", gw.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["symbol"], "AAPL");
    assert_eq!(body[0]["side"], "LONG");
    assert_eq!(body[0]["quantity"], "10");
}

// ---------------------------------------------------------------------------
// Cancel order via REST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_order_returns_cancelled_status() {
    let order_id = "ord-to-cancel";

    let mut open_order = test_order();
    open_order.id = order_id.into();
    open_order.status = OrderStatus::Open;

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_order(open_order);
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .delete(format!("{}/orders/{}", gw.base_url(), order_id))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["order"]["status"], "CANCELLED");
}

// ---------------------------------------------------------------------------
// Modify order via REST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modify_order_returns_updated_order() {
    let order_id = "ord-modify-1";

    let mut order = test_order();
    order.id = order_id.into();
    order.order_type = OrderType::Limit;
    order.limit_price = Some(dec!(150));
    order.status = OrderStatus::Open;

    let mut modified = order.clone();
    modified.limit_price = Some(dec!(151));

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_modify_order_response(
        order_id,
        Ok(ModifyOrderResult {
            order: modified,
            previous_order_id: None,
        }),
    );
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .patch(format!("{}/orders/{}", gw.base_url(), order_id))
        .json(&serde_json::json!({ "limit_price": "151" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["order"]["limit_price"], "151");
    assert!(body["previous_order_id"].is_null());
}

#[tokio::test]
async fn modify_order_cancel_replace_returns_previous_id() {
    let order_id = "ord-modify-cr";

    let mut order = test_order();
    order.id = "ord-modify-cr-new".into();
    order.order_type = OrderType::Limit;
    order.limit_price = Some(dec!(151));
    order.status = OrderStatus::Open;

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_modify_order_response(
        order_id,
        Ok(ModifyOrderResult {
            order,
            previous_order_id: Some(order_id.into()),
        }),
    );
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .patch(format!("{}/orders/{}", gw.base_url(), order_id))
        .json(&serde_json::json!({ "limit_price": "151" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["order"]["id"], "ord-modify-cr-new");
    assert_eq!(body["previous_order_id"], order_id);
}

#[tokio::test]
async fn modify_order_partial_fields() {
    let order_id = "ord-modify-partial";

    let mut order = test_order();
    order.id = order_id.into();
    order.order_type = OrderType::Limit;
    order.limit_price = Some(dec!(150));
    order.status = OrderStatus::Open;

    // Use the fallback path — no pre-configured response, order is in mock state.
    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_order(order);
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .patch(format!("{}/orders/{}", gw.base_url(), order_id))
        .json(&serde_json::json!({
            "stop_loss": "145",
            "take_profit": "160"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["order"]["stop_loss"], "145");
    assert_eq!(body["order"]["take_profit"], "160");
    // Original limit_price should be preserved
    assert_eq!(body["order"]["limit_price"], "150");
}

#[tokio::test]
async fn modify_order_not_found_returns_404() {
    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_modify_order_response(
        "ord-missing",
        Err(GatewayError::OrderNotFound {
            id: "ord-missing".into(),
        }),
    );
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .patch(format!("{}/orders/ord-missing", gw.base_url()))
        .json(&serde_json::json!({ "limit_price": "100" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "ORDER_NOT_FOUND");
}

#[tokio::test]
async fn modify_order_no_body_returns_400() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = http_client()
        .patch(format!("{}/orders/ord-nobody", gw.base_url()))
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn modify_order_invalid_json_returns_400() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = http_client()
        .patch(format!("{}/orders/ord-badjson", gw.base_url()))
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

// ---------------------------------------------------------------------------
// Cancel all orders via REST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_all_orders_cancels_all() {
    let mut o1 = test_order();
    o1.id = "ord-ca-1".into();
    o1.status = OrderStatus::Open;

    let mut o2 = test_order();
    o2.id = "ord-ca-2".into();
    o2.status = OrderStatus::Open;

    let mut o3 = test_order();
    o3.id = "ord-ca-3".into();
    o3.status = OrderStatus::Open;

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
        .with_order(o1)
        .with_order(o2)
        .with_order(o3);
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .delete(format!("{}/orders", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["cancelled_count"], 3);
    assert_eq!(body["failed_count"], 0);
}

#[tokio::test]
async fn cancel_all_orders_no_orders_returns_zero() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = http_client()
        .delete(format!("{}/orders", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["cancelled_count"], 0);
    assert_eq!(body["failed_count"], 0);
}

#[tokio::test]
async fn cancel_all_orders_filtered_by_symbol() {
    let mut o1 = test_order();
    o1.id = "ord-sym-1".into();
    o1.symbol = "AAPL".into();
    o1.status = OrderStatus::Open;

    let mut o2 = test_order();
    o2.id = "ord-sym-2".into();
    o2.symbol = "AAPL".into();
    o2.status = OrderStatus::Open;

    let mut o3 = test_order();
    o3.id = "ord-sym-3".into();
    o3.symbol = "TSLA".into();
    o3.status = OrderStatus::Open;

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
        .with_order(o1)
        .with_order(o2)
        .with_order(o3);
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .delete(format!("{}/orders?symbol=AAPL", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["cancelled_count"], 2);
    assert_eq!(body["failed_count"], 0);
}

#[tokio::test]
async fn cancel_all_with_partial_failure() {
    let mut o1 = test_order();
    o1.id = "ord-pf-1".into();
    o1.status = OrderStatus::Open;

    let mut o2 = test_order();
    o2.id = "ord-pf-2".into();
    o2.status = OrderStatus::Open;

    let mut o3 = test_order();
    o3.id = "ord-pf-3".into();
    o3.status = OrderStatus::Open;

    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper)
        .with_order(o1)
        .with_order(o2)
        .with_order(o3)
        .with_cancel_order_response(
            "ord-pf-2",
            Err(GatewayError::ProviderUnavailable {
                message: "broker timeout".into(),
            }),
        );
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .delete(format!("{}/orders", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["cancelled_count"], 2);
    assert_eq!(body["failed_count"], 1);

    let failed_ids = body["failed_order_ids"].as_array().unwrap();
    assert_eq!(failed_ids.len(), 1);
    assert_eq!(failed_ids[0], "ord-pf-2");
}
