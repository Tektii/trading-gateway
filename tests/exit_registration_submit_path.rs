//! Exit registration through the production order-submit path.
//!
//! Orders submitted with attached `stop_loss` / `take_profit` on a platform whose
//! `bracket_strategy` is `PendingSlTp` must be registered with that platform's
//! `ExitHandler` at submit time, so the parent fill synthesizes the exit orders.
//! Native-bracket platforms delegate exits to the broker and must not register.
//!
//! Tests await the strategy broadcast of each injected event before asserting:
//! the provider registry routes events through the `EventRouter` (which places
//! exit orders) *before* broadcasting, so receipt of the broadcast guarantees
//! exit processing has completed.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rust_decimal_macros::dec;
use tektii_gateway_core::adapter::BracketStrategy;
use tektii_gateway_core::error::GatewayResult;
use tektii_gateway_core::models::{OrderHandle, OrderStatus, OrderType, Side, TradingPlatform};
use tektii_gateway_core::websocket::messages::{OrderEventType, PositionEventType, WsMessage};
use tektii_gateway_test_support::harness::{
    StrategyClient, TestGateway, spawn_test_gateway_with_adapter,
    spawn_test_gateway_with_exit_management,
};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::{test_order, test_position};

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;
const TIMEOUT: Duration = Duration::from_secs(2);

fn pending_sl_tp_adapter() -> Arc<MockTradingAdapter> {
    Arc::new(MockTradingAdapter::new(PLATFORM).with_bracket_strategy(BracketStrategy::PendingSlTp))
}

fn handle_with_id(id: &str) -> GatewayResult<OrderHandle> {
    Ok(OrderHandle {
        id: id.to_string(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    })
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

/// Inject a fill event and wait for its strategy broadcast (exit routing done).
async fn inject_fill_qty_and_wait(
    gw: &TestGateway,
    client: &mut StrategyClient,
    order_id: &str,
    filled: rust_decimal::Decimal,
    total: rust_decimal::Decimal,
) {
    let full = filled >= total;
    let mut order = test_order();
    order.id = order_id.to_string();
    order.symbol = "AAPL".to_string();
    order.quantity = total;
    order.status = if full {
        OrderStatus::Filled
    } else {
        OrderStatus::PartiallyFilled
    };
    order.filled_quantity = filled;
    order.remaining_quantity = total - filled;
    gw.inject_event(WsMessage::Order {
        event: if full {
            OrderEventType::OrderFilled
        } else {
            OrderEventType::OrderPartiallyFilled
        },
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Order { order, .. }) => assert_eq!(order.id, order_id),
        other => panic!("expected fill broadcast, got {other:?}"),
    }
}

/// Inject a full parent fill and wait for its strategy broadcast.
async fn inject_fill_and_wait(gw: &TestGateway, client: &mut StrategyClient, order_id: &str) {
    inject_fill_qty_and_wait(gw, client, order_id, dec!(1), dec!(1)).await;
}

/// Inject a position-closed event and wait for its strategy broadcast.
async fn inject_position_closed_and_wait(gw: &TestGateway, client: &mut StrategyClient) {
    let mut position = test_position();
    position.symbol = "AAPL".to_string();
    position.quantity = dec!(0);
    gw.inject_event(WsMessage::Position {
        event: PositionEventType::PositionClosed,
        position,
        timestamp: Utc::now(),
    });

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Position { .. }) => {}
        other => panic!("expected position broadcast, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_with_sl_tp_registers_pending_exits() {
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;

    let exit_handler = gw
        .state
        .exit_handler_registry()
        .get(&PLATFORM)
        .expect("exit handler registered for platform");
    assert!(
        exit_handler.has_pending_for_primary(&order_id),
        "SL/TP should be registered as pending exits at submit time"
    );
}

#[tokio::test]
async fn parent_fill_places_synthesized_exits_with_derived_ids() {
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &order_id).await;

    // Parent + synthesized SL + synthesized TP, nothing more
    let submitted = adapter.submitted_orders();
    assert_eq!(submitted.len(), 3, "expected parent + SL + TP submissions");

    let stop = submitted
        .iter()
        .find(|o| o.order_type == OrderType::Stop)
        .expect("synthesized stop-loss submitted");
    assert_eq!(stop.client_order_id.as_deref(), Some("parent-1-sl"));
    assert_eq!(stop.symbol, "AAPL");
    assert_eq!(stop.side, Side::Sell);
    assert_eq!(stop.stop_price, Some(dec!(90)));
    assert_eq!(stop.quantity, dec!(1));

    let limit = submitted
        .iter()
        .find(|o| o.order_type == OrderType::Limit)
        .expect("synthesized take-profit submitted");
    assert_eq!(limit.client_order_id.as_deref(), Some("parent-1-tp"));
    assert_eq!(limit.symbol, "AAPL");
    assert_eq!(limit.side, Side::Sell);
    assert_eq!(limit.limit_price, Some(dec!(120)));
    assert_eq!(limit.quantity, dec!(1));
}

#[tokio::test]
async fn position_close_cancels_placed_exits() {
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &order_id).await;
    assert_eq!(adapter.submitted_orders().len(), 3);

    inject_position_closed_and_wait(&gw, &mut client).await;

    assert_eq!(
        adapter.cancelled_orders().len(),
        2,
        "both resting exit legs should be cancelled when the position closes"
    );
}

#[tokio::test]
async fn native_bracket_platform_skips_registration() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM).with_bracket_strategy(BracketStrategy::NativeBracket),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;

    let exit_handler = gw
        .state
        .exit_handler_registry()
        .get(&PLATFORM)
        .expect("exit handler registered for platform");
    assert!(
        !exit_handler.has_pending_for_primary(&order_id),
        "native-bracket platforms must not register pending exits"
    );

    inject_fill_and_wait(&gw, &mut client, &order_id).await;
    assert_eq!(
        adapter.submitted_orders().len(),
        1,
        "no synthesized exits should be placed for native-bracket platforms"
    );
}

#[tokio::test]
async fn submit_without_sl_tp_registers_nothing() {
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;

    let order_id = submit_order(
        &gw,
        &serde_json::json!({
            "symbol": "AAPL",
            "side": "BUY",
            "order_type": "MARKET",
            "quantity": "1"
        }),
    )
    .await;

    let exit_handler = gw
        .state
        .exit_handler_registry()
        .get(&PLATFORM)
        .expect("exit handler registered for platform");
    assert!(
        !exit_handler.has_pending_for_primary(&order_id),
        "orders without SL/TP must not create pending exits"
    );
}

#[tokio::test]
async fn fill_arriving_before_registration_is_replayed() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id")),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    // The broker's fill beats the submit response: nothing is registered yet,
    // so the fill must be buffered rather than dropped.
    inject_fill_and_wait(&gw, &mut client, "parent-id").await;
    assert_eq!(adapter.submitted_orders().len(), 0);

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    assert_eq!(order_id, "parent-id");

    // Registration replays the buffered fill before the route responds.
    let submitted = adapter.submitted_orders();
    assert_eq!(submitted.len(), 3, "parent + replayed SL + TP submissions");
    let exit_ids: Vec<_> = submitted
        .iter()
        .filter_map(|o| o.client_order_id.as_deref())
        .collect();
    assert!(exit_ids.contains(&"parent-1-sl"));
    assert!(exit_ids.contains(&"parent-1-tp"));
}

#[tokio::test]
async fn exit_leg_fill_cancels_sibling_leg() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id"))
            .with_submit_order_response(handle_with_id("sl-leg-id"))
            .with_submit_order_response(handle_with_id("tp-leg-id")),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &order_id).await;
    assert_eq!(adapter.submitted_orders().len(), 3);

    // The stop-loss leg fills at the broker: the take-profit leg must be
    // cancelled immediately off the fill event, not only on position close.
    inject_fill_and_wait(&gw, &mut client, "sl-leg-id").await;

    assert_eq!(
        adapter.cancelled_orders(),
        vec!["tp-leg-id".to_string()],
        "surviving exit leg should be cancelled when its sibling fills"
    );
    let exit_handler = gw
        .state
        .exit_handler_registry()
        .get(&PLATFORM)
        .expect("exit handler registered for platform");
    assert!(
        !exit_handler.has_pending_for_primary(&order_id),
        "both exit entries should be cleaned up after the OCO resolution"
    );
}

#[tokio::test]
async fn failed_sibling_cancel_keeps_entry_for_position_close() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id"))
            .with_submit_order_response(handle_with_id("sl-leg-id"))
            .with_submit_order_response(handle_with_id("tp-leg-id"))
            .with_cancel_order_response(
                "tp-leg-id",
                Err(
                    tektii_gateway_core::error::GatewayError::ProviderUnavailable {
                        message: "broker down".to_string(),
                    },
                ),
            ),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &order_id).await;
    inject_fill_and_wait(&gw, &mut client, "sl-leg-id").await;

    // The TP cancel failed at the broker: its entry must stay tracked so the
    // position-close path can still clean up the live resting order.
    let exit_handler = gw
        .state
        .exit_handler_registry()
        .get(&PLATFORM)
        .expect("exit handler registered for platform");
    assert!(
        exit_handler.has_pending_for_primary(&order_id),
        "sibling entry must be retained when its broker cancel fails"
    );
}

#[tokio::test]
async fn partial_exit_leg_fill_leaves_both_legs_resting() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id"))
            .with_submit_order_response(handle_with_id("sl-leg-id"))
            .with_submit_order_response(handle_with_id("tp-leg-id")),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &order_id).await;

    // The SL leg fills only partially: the position is still partly open, so
    // nothing may be cancelled or dropped.
    inject_fill_qty_and_wait(&gw, &mut client, "sl-leg-id", dec!(0.4), dec!(1)).await;

    assert!(adapter.cancelled_orders().is_empty());
    let exit_handler = gw
        .state
        .exit_handler_registry()
        .get(&PLATFORM)
        .expect("exit handler registered for platform");
    assert!(
        exit_handler.has_pending_for_primary(&order_id),
        "both legs must keep resting after a partial exit fill"
    );
}

#[tokio::test]
async fn duplicate_submit_with_same_order_id_does_not_double_register() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id"))
            .with_submit_order_response(handle_with_id("parent-id")),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    // A client retry that the broker dedupes returns the same order id twice.
    submit_order(&gw, &order_with_exits_json()).await;
    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    assert_eq!(order_id, "parent-id");

    inject_fill_and_wait(&gw, &mut client, &order_id).await;

    // 2 parent submissions + exactly one SL/TP pair.
    assert_eq!(
        adapter.submitted_orders().len(),
        4,
        "duplicate registration would have placed four exit legs"
    );
}

#[tokio::test]
async fn missing_exit_handler_still_accepts_order() {
    // Plain harness: PendingSlTp strategy but no exit handler in the registry.
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_adapter(Arc::clone(&adapter)).await;

    // Must not panic or fail the submission; the gap is logged.
    submit_order(&gw, &order_with_exits_json()).await;
    assert_eq!(adapter.submitted_orders().len(), 1);
}
