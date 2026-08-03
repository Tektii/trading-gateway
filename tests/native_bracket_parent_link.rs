//! Broker-native bracket links reach the strategy, same as synthesized ones.
//!
//! Alpaca and Saxo manage brackets themselves and report the entry↔exit link in
//! their own payloads. The adapter is the only component that sees it, so it
//! resolves the link and the gateway must carry that value through untouched —
//! REST reads and order events alike.
//!
//! The synthesized path (`ExitHandler`) stays the fallback: it owns the link for
//! providers that report nothing. These tests pin the precedence between the two
//! and the consistency between the two channels.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tektii_gateway_core::models::{Order, OrderStatus, TradingPlatform};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_test_support::harness::{
    StrategyClient, TestGateway, spawn_test_gateway_with_adapter,
    spawn_test_gateway_with_exit_management,
};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::test_order;

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;
const TIMEOUT: Duration = Duration::from_secs(2);

fn stored_order(id: &str) -> Order {
    let mut order = test_order();
    order.id = id.to_string();
    order.symbol = "AAPL".to_string();
    order
}

/// Adapter standing in for a native-bracket broker: it serves the entry and both
/// legs, and reports the link it read off the broker payload.
fn native_bracket_adapter() -> Arc<MockTradingAdapter> {
    Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_order(stored_order("entry-1"))
            .with_order(stored_order("sl-1"))
            .with_order(stored_order("tp-1"))
            .with_bracket_link("sl-1", "entry-1")
            .with_bracket_link("tp-1", "entry-1"),
    )
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

#[tokio::test]
async fn get_order_exposes_broker_reported_parent_on_exit_leg() {
    let gw = spawn_test_gateway_with_adapter(native_bracket_adapter()).await;

    let leg = get_order(&gw, "sl-1").await;

    assert_eq!(
        leg.parent_order_id.as_deref(),
        Some("entry-1"),
        "the adapter resolved the broker's bracket link; the route must not drop it"
    );
}

#[tokio::test]
async fn list_orders_exposes_broker_reported_parent_on_exit_legs() {
    let gw = spawn_test_gateway_with_adapter(native_bracket_adapter()).await;

    let orders = list_orders(&gw).await;

    let mut leg_ids: Vec<&str> = orders
        .iter()
        .filter(|o| o.parent_order_id.as_deref() == Some("entry-1"))
        .map(|o| o.id.as_str())
        .collect();
    leg_ids.sort_unstable();

    assert_eq!(
        leg_ids,
        vec!["sl-1", "tp-1"],
        "both native exit legs should point at the entry order"
    );
}

#[tokio::test]
async fn entry_order_has_no_parent_order_id() {
    let gw = spawn_test_gateway_with_adapter(native_bracket_adapter()).await;

    let entry = get_order(&gw, "entry-1").await;

    assert_eq!(
        entry.parent_order_id, None,
        "the entry order is not an exit leg and has no parent"
    );
}

#[tokio::test]
async fn broadcast_order_event_carries_broker_reported_parent() {
    // Exit management registers the adapter alongside the event router, as
    // production does. Its ExitHandler knows nothing of these legs, so the link
    // can only come from the adapter.
    let gw = spawn_test_gateway_with_exit_management(native_bracket_adapter()).await;
    let mut client = StrategyClient::connect(&gw).await;

    // The broker reports a leg event without a parent reference of its own —
    // only the adapter, which saw the entry, can supply it.
    let mut order = stored_order("sl-1");
    order.status = OrderStatus::Filled;
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order {
            order,
            parent_order_id,
            ..
        }) => {
            assert_eq!(order.id, "sl-1");
            assert_eq!(
                parent_order_id.as_deref(),
                Some("entry-1"),
                "the event envelope should carry the broker's bracket link"
            );
            assert_eq!(
                order.parent_order_id.as_deref(),
                Some("entry-1"),
                "the embedded order should match the envelope, and the REST read"
            );
        }
        other => panic!("expected order broadcast, got {other:?}"),
    }
}

#[tokio::test]
async fn provider_supplied_parent_on_the_event_wins_over_the_adapter_lookup() {
    let gw = spawn_test_gateway_with_exit_management(native_bracket_adapter()).await;
    let mut client = StrategyClient::connect(&gw).await;

    // A provider that puts the link on the event itself is the most direct
    // source; the adapter's cache must not override it.
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order: stored_order("sl-1"),
        parent_order_id: Some("entry-from-event".to_string()),
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order {
            parent_order_id, ..
        }) => assert_eq!(parent_order_id.as_deref(), Some("entry-from-event")),
        other => panic!("expected order broadcast, got {other:?}"),
    }
}

#[tokio::test]
async fn orders_without_a_broker_link_report_no_parent() {
    let gw = spawn_test_gateway_with_adapter(Arc::new(
        MockTradingAdapter::new(PLATFORM).with_order(stored_order("lone-1")),
    ))
    .await;

    let order = get_order(&gw, "lone-1").await;

    assert_eq!(
        order.parent_order_id, None,
        "an adapter reporting no bracket link must not invent one"
    );
}

#[tokio::test]
async fn a_terminal_leg_event_releases_the_link() {
    let gw = spawn_test_gateway_with_exit_management(native_bracket_adapter()).await;
    let mut client = StrategyClient::connect(&gw).await;

    let mut order = stored_order("sl-1");
    order.status = OrderStatus::Filled;
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    // The fill still reports the link — that event is when it matters.
    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order {
            parent_order_id, ..
        }) => assert_eq!(parent_order_id.as_deref(), Some("entry-1")),
        other => panic!("expected order broadcast, got {other:?}"),
    }

    // Afterwards the leg has resolved and the link is not retained. Events are
    // the channel a resolved leg normally arrives on, so this is what keeps the
    // adapter's link map bounded by live brackets.
    let leg = get_order(&gw, "sl-1").await;
    assert_eq!(leg.parent_order_id, None);
}

#[tokio::test]
async fn a_non_terminal_leg_event_keeps_the_link() {
    let gw = spawn_test_gateway_with_exit_management(native_bracket_adapter()).await;
    let mut client = StrategyClient::connect(&gw).await;

    let mut order = stored_order("sl-1");
    order.status = OrderStatus::PartiallyFilled;
    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderPartiallyFilled,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    client.recv_message(TIMEOUT).await.expect("broadcast");

    let leg = get_order(&gw, "sl-1").await;
    assert_eq!(
        leg.parent_order_id.as_deref(),
        Some("entry-1"),
        "a leg that is still working must keep its link"
    );
}

#[tokio::test]
async fn broadcast_reports_no_parent_for_an_order_neither_source_knows() {
    let gw = spawn_test_gateway_with_exit_management(Arc::new(
        MockTradingAdapter::new(PLATFORM).with_order(stored_order("lone-1")),
    ))
    .await;
    let mut client = StrategyClient::connect(&gw).await;

    gw.inject_event(WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order: stored_order("lone-1"),
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order {
            order,
            parent_order_id,
            ..
        }) => {
            assert_eq!(parent_order_id, None, "no source knows a link; invent none");
            assert_eq!(order.parent_order_id, None);
        }
        other => panic!("expected order broadcast, got {other:?}"),
    }
}
