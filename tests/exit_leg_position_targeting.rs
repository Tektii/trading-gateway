//! Synthesized exit legs must target the position they protect.
//!
//! On a hedging-mode provider, an order only closes an existing position when it
//! carries that position's id; without one it opens a second, independent
//! position in the opposite direction — so a stop-loss would add exposure rather
//! than remove it. The entry order's position id arrives on the fill event and
//! must be carried through to both synthesized legs.
//!
//! Providers that never report a position id (every live broker adapter today)
//! must keep submitting untargeted legs, and must not set `reduce_only` — a
//! hedging broker rejects reduce-only orders that name no position.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tektii_gateway_core::adapter::BracketStrategy;
use tektii_gateway_core::error::GatewayResult;
use tektii_gateway_core::models::{
    OrderHandle, OrderRequest, OrderStatus, OrderType, TradingPlatform,
};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_test_support::harness::{
    StrategyClient, TestGateway, spawn_test_gateway_with_exit_management,
};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::test_order;

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

/// Inject a fill of `filled`/`total` carrying `position_id` and wait for its
/// strategy broadcast, which happens after exit routing has completed.
async fn inject_fill_qty_and_wait(
    gw: &TestGateway,
    client: &mut StrategyClient,
    order_id: &str,
    position_id: Option<&str>,
    filled: Decimal,
    total: Decimal,
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
    order.position_id = position_id.map(ToString::to_string);

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

/// Inject a full fill carrying `position_id`.
async fn inject_fill_and_wait(
    gw: &TestGateway,
    client: &mut StrategyClient,
    order_id: &str,
    position_id: Option<&str>,
) {
    inject_fill_qty_and_wait(gw, client, order_id, position_id, dec!(1), dec!(1)).await;
}

/// Every synthesized leg in `submitted`, asserted to be a stop or limit order.
fn exit_legs(submitted: &[OrderRequest]) -> Vec<&OrderRequest> {
    submitted
        .iter()
        .filter(|o| matches!(o.order_type, OrderType::Stop | OrderType::Limit))
        .collect()
}

#[tokio::test]
async fn synthesized_exits_target_the_parent_position() {
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &order_id, Some("pos-99")).await;

    let submitted = adapter.submitted_orders();
    let legs = exit_legs(&submitted);
    assert_eq!(legs.len(), 2, "expected synthesized SL + TP");

    for leg in legs {
        assert_eq!(
            leg.position_id.as_deref(),
            Some("pos-99"),
            "exit leg must target the position it protects, else it opens a new one"
        );
        assert!(
            leg.reduce_only,
            "a targeted exit leg may only reduce the position"
        );
    }
}

#[tokio::test]
async fn incrementally_placed_legs_keep_targeting_the_position() {
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;

    // A partial fill places legs sized to the filled portion; the completing
    // fill tops them up. That second placement takes a different path -- it
    // sees the already-placed legs -- and must target the position too.
    inject_fill_qty_and_wait(
        &gw,
        &mut client,
        &order_id,
        Some("pos-7"),
        dec!(0.4),
        dec!(1),
    )
    .await;
    inject_fill_qty_and_wait(&gw, &mut client, &order_id, Some("pos-7"), dec!(1), dec!(1)).await;

    let submitted = adapter.submitted_orders();
    let legs = exit_legs(&submitted);
    assert!(
        legs.len() > 2,
        "expected legs from both the partial and the completing fill, got {}",
        legs.len()
    );

    for leg in legs {
        assert_eq!(
            leg.position_id.as_deref(),
            Some("pos-7"),
            "every incrementally placed leg must target the position"
        );
        assert!(leg.reduce_only);
    }
}

#[tokio::test]
async fn exits_stay_untargeted_when_the_parent_reports_no_position() {
    let adapter = pending_sl_tp_adapter();
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let order_id = submit_order(&gw, &order_with_exits_json()).await;
    inject_fill_and_wait(&gw, &mut client, &order_id, None).await;

    let submitted = adapter.submitted_orders();
    let legs = exit_legs(&submitted);
    assert_eq!(legs.len(), 2, "expected synthesized SL + TP");

    for leg in legs {
        assert_eq!(leg.position_id, None);
        assert!(
            !leg.reduce_only,
            "reduce_only without a position id is rejected by hedging brokers"
        );
    }
}

#[tokio::test]
async fn buffered_fill_replay_preserves_the_position_id() {
    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_submit_order_response(handle_with_id("parent-id")),
    );
    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    // The fill beats the submit response, so it is buffered and replayed at
    // registration. The position id must survive that round trip.
    inject_fill_and_wait(&gw, &mut client, "parent-id", Some("pos-buffered")).await;
    assert_eq!(adapter.submitted_orders().len(), 0);

    submit_order(&gw, &order_with_exits_json()).await;

    let submitted = adapter.submitted_orders();
    let legs = exit_legs(&submitted);
    assert_eq!(legs.len(), 2, "expected replayed SL + TP");

    for leg in legs {
        assert_eq!(leg.position_id.as_deref(), Some("pos-buffered"));
        assert!(leg.reduce_only);
    }
}
