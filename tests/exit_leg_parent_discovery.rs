//! Exit legs carry the entry order they protect, via `parent_order_id`.
//!
//! The gateway synthesizes SL/TP legs itself on the `PendingSlTp` path, so the
//! entry↔exit link exists only in its `ExitHandler` — no broker reports it and
//! nothing persisted it. Order events now carry the link, and the REST reads
//! expose the same value so a strategy reconnecting mid-session can rebuild the
//! association during reconciliation.
//!
//! The event stream is the primary channel; the REST reads are for recovery,
//! not for driving a strategy by repeated querying.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rust_decimal_macros::dec;
use tektii_gateway_core::adapter::BracketStrategy;
use tektii_gateway_core::error::GatewayResult;
use tektii_gateway_core::models::{Order, OrderHandle, OrderStatus, TradingPlatform};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_test_support::harness::{
    StrategyClient, TestGateway, spawn_test_gateway_with_adapter,
    spawn_test_gateway_with_exit_management,
};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::test_order;

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;
const TIMEOUT: Duration = Duration::from_secs(2);

fn handle_with_id(id: &str) -> GatewayResult<OrderHandle> {
    Ok(OrderHandle {
        id: id.to_string(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    })
}

fn stored_order(id: &str) -> Order {
    let mut order = test_order();
    order.id = id.to_string();
    order.symbol = "AAPL".to_string();
    order
}

fn order_with_exits_json() -> serde_json::Value {
    serde_json::json!({
        "symbol": "AAPL",
        "side": "BUY",
        "order_type": "MARKET",
        "quantity": "1",
        "stop_loss": "90",
        "take_profit": "120",
        "client_order_id": "parent-1"
    })
}

/// Adapter that resolves the entry plus both synthesized legs to fixed ids, and
/// serves all three back from `get_orders` / `get_order`.
fn bracket_adapter() -> Arc<MockTradingAdapter> {
    Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id"))
            .with_submit_order_response(handle_with_id("sl-leg-id"))
            .with_submit_order_response(handle_with_id("tp-leg-id"))
            .with_order(stored_order("parent-id"))
            .with_order(stored_order("sl-leg-id"))
            .with_order(stored_order("tp-leg-id")),
    )
}

async fn submit_order(gw: &TestGateway, body: &serde_json::Value) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{}/orders", gw.base_url()))
        .json(body)
        .send()
        .await
        .expect("submit request failed");
    assert_eq!(resp.status(), 201, "order submission should succeed");
    let handle: serde_json::Value = resp.json().await.expect("invalid response body");
    handle["id"].as_str().expect("handle has id").to_string()
}

/// Inject a full fill for `order_id` and wait for its strategy broadcast, which
/// guarantees the exit legs have been placed.
async fn inject_fill_and_wait(gw: &TestGateway, client: &mut StrategyClient, order_id: &str) {
    let mut order = stored_order(order_id);
    order.status = OrderStatus::Filled;
    order.filled_quantity = dec!(1);
    order.remaining_quantity = dec!(0);
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order { order, .. }) => assert_eq!(order.id, order_id),
        other => panic!("expected fill broadcast, got {other:?}"),
    }
}

async fn list_orders(gw: &TestGateway) -> Vec<Order> {
    reqwest::Client::new()
        .get(format!("{}/orders", gw.base_url()))
        .send()
        .await
        .expect("list request failed")
        .json()
        .await
        .expect("invalid list body")
}

async fn get_order(gw: &TestGateway, order_id: &str) -> Order {
    reqwest::Client::new()
        .get(format!("{}/orders/{order_id}", gw.base_url()))
        .send()
        .await
        .expect("get request failed")
        .json()
        .await
        .expect("invalid order body")
}

#[tokio::test]
async fn list_orders_exposes_parent_order_id_on_exit_legs() {
    let adapter = bracket_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let entry_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &entry_id).await;

    let orders = list_orders(&gw).await;

    // Reconciling after a reconnect: the exit legs are recovered by filtering
    // on the entry's id.
    let legs: Vec<&Order> = orders
        .iter()
        .filter(|o| o.parent_order_id.as_deref() == Some(entry_id.as_str()))
        .collect();

    let mut leg_ids: Vec<&str> = legs.iter().map(|o| o.id.as_str()).collect();
    leg_ids.sort_unstable();
    assert_eq!(
        leg_ids,
        vec!["sl-leg-id", "tp-leg-id"],
        "both synthesized exit legs should point at the entry order"
    );
}

#[tokio::test]
async fn entry_order_has_no_parent_order_id() {
    let adapter = bracket_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let entry_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &entry_id).await;

    let orders = list_orders(&gw).await;
    let entry = orders
        .iter()
        .find(|o| o.id == entry_id)
        .expect("entry order listed");

    assert_eq!(
        entry.parent_order_id, None,
        "the entry order is not an exit leg and has no parent"
    );
}

#[tokio::test]
async fn get_order_exposes_parent_order_id_on_exit_leg() {
    let adapter = bracket_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let entry_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &entry_id).await;

    let leg = get_order(&gw, "sl-leg-id").await;

    assert_eq!(
        leg.parent_order_id.as_deref(),
        Some(entry_id.as_str()),
        "single-order fetch should carry the same parent link as the list"
    );
}

#[tokio::test]
async fn broadcast_order_event_carries_parent_order_id_for_exit_leg() {
    let adapter = bracket_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let entry_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &entry_id).await;

    // A non-terminal event on the resting leg: the legs stay tracked, so the
    // broadcast must carry the parent link the REST path exposes.
    let mut leg = stored_order("sl-leg-id");
    leg.status = OrderStatus::Open;
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderModified,
        order: leg,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order {
            order,
            parent_order_id,
            ..
        }) => {
            assert_eq!(order.id, "sl-leg-id");
            assert_eq!(
                order.parent_order_id.as_deref(),
                Some(entry_id.as_str()),
                "the broadcast order should carry the same parent link as REST"
            );
            assert_eq!(
                parent_order_id.as_deref(),
                Some(entry_id.as_str()),
                "the event's parent_order_id field should be populated, not dead"
            );
        }
        other => panic!("expected leg broadcast, got {other:?}"),
    }
}

#[tokio::test]
async fn exit_leg_fill_broadcast_carries_parent_order_id() {
    let adapter = bracket_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let entry_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &entry_id).await;

    // The leg's own terminal fill is the event a streaming strategy most needs
    // the parent link on: it is what closed the entry.
    let mut leg = stored_order("sl-leg-id");
    leg.status = OrderStatus::Filled;
    leg.filled_quantity = dec!(1);
    leg.remaining_quantity = dec!(0);
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order: leg,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order {
            order,
            parent_order_id,
            ..
        }) => {
            assert_eq!(order.id, "sl-leg-id");
            assert_eq!(
                order.parent_order_id.as_deref(),
                Some(entry_id.as_str()),
                "the leg's own fill must still identify the entry it closed"
            );
            assert_eq!(parent_order_id.as_deref(), Some(entry_id.as_str()));
        }
        other => panic!("expected leg fill broadcast, got {other:?}"),
    }
}

#[tokio::test]
async fn provider_supplied_parent_order_id_wins_over_tracked() {
    let adapter = bracket_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let entry_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &entry_id).await;

    // A provider that reports the link itself is authoritative over the
    // gateway's own tracking.
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderModified,
        order: stored_order("sl-leg-id"),
        parent_order_id: Some("broker-reported-parent".to_string()),
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order {
            order,
            parent_order_id,
            ..
        }) => {
            assert_ne!(
                entry_id.as_str(),
                "broker-reported-parent",
                "test is meaningless if the two candidate values coincide"
            );
            assert_eq!(parent_order_id.as_deref(), Some("broker-reported-parent"));
            assert_eq!(
                order.parent_order_id.as_deref(),
                Some("broker-reported-parent"),
            );
        }
        other => panic!("expected leg broadcast, got {other:?}"),
    }
}

#[tokio::test]
async fn order_history_exposes_parent_order_id_on_exit_legs() {
    let adapter = bracket_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let entry_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &entry_id).await;

    let history: Vec<Order> = reqwest::Client::new()
        .get(format!("{}/orders/history", gw.base_url()))
        .send()
        .await
        .expect("history request failed")
        .json()
        .await
        .expect("invalid history body");

    let leg = history
        .iter()
        .find(|o| o.id == "sl-leg-id")
        .expect("exit leg listed in history");
    assert_eq!(
        leg.parent_order_id.as_deref(),
        Some(entry_id.as_str()),
        "history should carry the same parent link as the open-orders list"
    );
}

#[tokio::test]
async fn orders_are_served_when_no_exit_handler_is_registered() {
    // Plain harness: no exit handler in the registry, so the lookup finds no
    // handler for the platform at all.
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id"))
            .with_order(stored_order("parent-id")),
    );
    let gw = spawn_test_gateway_with_adapter(Arc::clone(&adapter)).await;

    submit_order(&gw, &order_with_exits_json()).await;

    let orders = list_orders(&gw).await;
    assert!(
        orders.iter().all(|o| o.parent_order_id.is_none()),
        "a missing exit handler must yield no parent link rather than failing"
    );
    assert_eq!(get_order(&gw, "parent-id").await.parent_order_id, None);
}

#[tokio::test]
async fn orders_without_exits_have_no_parent_order_id() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("plain-id"))
            .with_order(stored_order("plain-id")),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;

    submit_order(
        &gw,
        &serde_json::json!({
            "symbol": "AAPL",
            "side": "BUY",
            "order_type": "MARKET",
            "quantity": "1"
        }),
    )
    .await;

    let orders = list_orders(&gw).await;
    assert!(
        orders.iter().all(|o| o.parent_order_id.is_none()),
        "orders placed without SL/TP should never carry a parent link"
    );
}
