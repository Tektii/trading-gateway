//! Moving a position's stop-loss or take-profit must never unprotect it.
//!
//! SL/TP submitted alongside an entry order become separate exit-leg orders once
//! the entry fills, so moving one used to mean the strategy cancelling the leg
//! and placing a replacement itself -- two calls with no rollback, leaving the
//! position naked if the second failed.
//!
//! `PATCH /positions/{id}` does the replace gateway-side. It prefers the
//! provider's native modify (atomic at the broker); where that is unsupported it
//! falls back to cancel-and-replace and restores the original exit if the
//! replacement is rejected. The position is only ever reported as unprotected
//! when that restore also fails.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tektii_gateway_core::adapter::BracketStrategy;
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::models::{
    CancelOrderResult, ModifyOrderResult, OrderHandle, OrderRequest, OrderStatus, OrderType,
    TradingPlatform,
};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_test_support::harness::{
    StrategyClient, TestGateway, spawn_test_gateway_with_exit_management,
};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::{test_order, test_position};

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;
const TIMEOUT: Duration = Duration::from_secs(2);

const POSITION_ID: &str = "pos-1";
const SL_ORDER_ID: &str = "sl-order";
const TP_ORDER_ID: &str = "tp-order";

fn handle_with_id(id: &str) -> GatewayResult<OrderHandle> {
    Ok(OrderHandle {
        id: id.to_string(),
        client_order_id: None,
        correlation_id: None,
        status: OrderStatus::Open,
    })
}

fn modify_result(order_id: &str) -> GatewayResult<ModifyOrderResult> {
    let mut order = test_order();
    order.id = order_id.to_string();
    Ok(ModifyOrderResult {
        order,
        previous_order_id: None,
    })
}

fn cancelled(order_id: &str) -> GatewayResult<CancelOrderResult> {
    let mut order = test_order();
    order.id = order_id.to_string();
    order.status = OrderStatus::Cancelled;
    Ok(CancelOrderResult {
        success: true,
        order,
    })
}

/// A position at the provider whose entry order has filled, with SL at 90 and
/// TP at 120 resting as separate exit-leg orders `sl-order` and `tp-order`.
fn adapter_with_open_position() -> MockTradingAdapter {
    let mut position = test_position();
    position.id = POSITION_ID.to_string();
    position.symbol = "AAPL".to_string();

    MockTradingAdapter::new(PLATFORM)
        .with_bracket_strategy(BracketStrategy::PendingSlTp)
        .with_position(position)
        .with_submit_order_response(handle_with_id("parent-1"))
        .with_submit_order_response(handle_with_id(SL_ORDER_ID))
        .with_submit_order_response(handle_with_id(TP_ORDER_ID))
}

/// Submit an entry order carrying SL at 90 and TP at 120, then fill it so both
/// legs are placed against `POSITION_ID`.
async fn open_position_with_exits(
    adapter: Arc<MockTradingAdapter>,
) -> (TestGateway, StrategyClient) {
    open_position(
        adapter,
        &serde_json::json!({
            "symbol": "AAPL",
            "side": "BUY",
            "order_type": "MARKET",
            "quantity": "1",
            "stop_loss": "90",
            "take_profit": "120",
            "client_order_id": "parent-1"
        }),
    )
    .await
}

async fn open_position(
    adapter: Arc<MockTradingAdapter>,
    order: &serde_json::Value,
) -> (TestGateway, StrategyClient) {
    let gw = spawn_test_gateway_with_exit_management(adapter).await;
    let mut client = StrategyClient::connect(&gw).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/orders", gw.base_url()))
        .json(order)
        .send()
        .await
        .expect("submit request failed");
    assert_eq!(resp.status(), 201, "order submission should succeed");

    inject_fill(&gw, &mut client, dec!(1), dec!(1)).await;

    (gw, client)
}

/// Inject a fill of `filled`/`total` for the entry order and wait for its
/// broadcast, which lands after exit routing has run.
async fn inject_fill(
    gw: &TestGateway,
    client: &mut StrategyClient,
    filled: Decimal,
    total: Decimal,
) {
    let full = filled >= total;
    let mut order = test_order();
    order.id = "parent-1".to_string();
    order.symbol = "AAPL".to_string();
    order.quantity = total;
    order.status = if full {
        OrderStatus::Filled
    } else {
        OrderStatus::PartiallyFilled
    };
    order.filled_quantity = filled;
    order.remaining_quantity = total - filled;
    order.position_id = Some(POSITION_ID.to_string());

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
        Some(WsMessage::Order { order, .. }) => assert_eq!(order.id, "parent-1"),
        other => panic!("expected fill broadcast, got {other:?}"),
    }
}

async fn patch_position(
    gw: &TestGateway,
    position_id: &str,
    body: &serde_json::Value,
) -> reqwest::Response {
    reqwest::Client::new()
        .patch(format!("{}/positions/{position_id}", gw.base_url()))
        .json(body)
        .send()
        .await
        .expect("patch request failed")
}

/// Exit legs the adapter was asked to place, in submission order.
fn exit_legs(submitted: &[OrderRequest]) -> Vec<&OrderRequest> {
    submitted
        .iter()
        .filter(|o| matches!(o.order_type, OrderType::Stop | OrderType::Limit))
        .collect()
}

#[tokio::test]
async fn moving_the_stop_loss_uses_the_providers_native_modify() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(SL_ORDER_ID, modify_result(SL_ORDER_ID)),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "95" })).await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("invalid response body");
    assert_eq!(body["position_id"], POSITION_ID);
    assert_eq!(
        body["stop_loss"]["order_ids"],
        serde_json::json!([SL_ORDER_ID])
    );
    assert_eq!(body["stop_loss"]["trigger_price"], "95");

    let modified = adapter.modified_orders();
    assert_eq!(modified.len(), 1, "expected exactly one modify");
    assert_eq!(modified[0].0, SL_ORDER_ID);
    assert_eq!(
        modified[0].1.stop_price,
        Some(dec!(95)),
        "a stop-loss leg is a stop order -- its trigger is stop_price"
    );

    assert!(
        adapter.cancelled_orders().is_empty(),
        "native modify must not cancel the resting exit"
    );
    assert_eq!(
        exit_legs(&adapter.submitted_orders()).len(),
        2,
        "native modify must not place a replacement leg"
    );
}

#[tokio::test]
async fn moving_the_take_profit_targets_its_limit_price() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(TP_ORDER_ID, modify_result(TP_ORDER_ID)),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(
        &gw,
        POSITION_ID,
        &serde_json::json!({ "take_profit": "130" }),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let modified = adapter.modified_orders();
    assert_eq!(modified.len(), 1);
    assert_eq!(modified[0].0, TP_ORDER_ID);
    assert_eq!(
        modified[0].1.limit_price,
        Some(dec!(130)),
        "a take-profit leg is a limit order -- its trigger is limit_price"
    );
}

#[tokio::test]
async fn both_legs_move_in_one_request() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(SL_ORDER_ID, modify_result(SL_ORDER_ID))
            .with_modify_order_response(TP_ORDER_ID, modify_result(TP_ORDER_ID)),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(
        &gw,
        POSITION_ID,
        &serde_json::json!({ "stop_loss": "95", "take_profit": "130" }),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("invalid response body");
    assert_eq!(body["stop_loss"]["trigger_price"], "95");
    assert_eq!(body["take_profit"]["trigger_price"], "130");
    assert_eq!(adapter.modified_orders().len(), 2);
}

#[tokio::test]
async fn falls_back_to_cancel_replace_when_the_provider_cannot_modify() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(
                SL_ORDER_ID,
                Err(GatewayError::unsupported("modify_order", "mock")),
            )
            .with_cancel_order_response(SL_ORDER_ID, cancelled(SL_ORDER_ID))
            .with_submit_order_response(handle_with_id("sl-order-2")),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "95" })).await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("invalid response body");
    assert_eq!(
        body["stop_loss"]["order_ids"],
        serde_json::json!(["sl-order-2"]),
        "the response must name the replacement order, not the cancelled one"
    );

    assert_eq!(adapter.cancelled_orders(), vec![SL_ORDER_ID.to_string()]);

    let submitted = adapter.submitted_orders();
    let legs = exit_legs(&submitted);
    assert_eq!(
        legs.len(),
        3,
        "expected the original SL, TP, and a replacement"
    );
    let replacement = legs.last().expect("replacement leg");
    assert_eq!(replacement.stop_price, Some(dec!(95)));
    assert_eq!(
        replacement.position_id.as_deref(),
        Some(POSITION_ID),
        "the replacement must keep targeting the position it protects"
    );
    assert!(replacement.reduce_only);
}

#[tokio::test]
async fn a_rejected_replacement_restores_the_original_exit() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(
                SL_ORDER_ID,
                Err(GatewayError::unsupported("modify_order", "mock")),
            )
            .with_cancel_order_response(SL_ORDER_ID, cancelled(SL_ORDER_ID))
            .with_submit_order_response(Err(GatewayError::OrderRejected {
                reason: "stop price crosses the market".to_string(),
                reject_code: None,
                details: None,
            }))
            .with_submit_order_response(handle_with_id("sl-order-restored")),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "95" })).await;
    assert_eq!(
        resp.status(),
        409,
        "the move failed, but the position is still protected"
    );

    let body: serde_json::Value = resp.json().await.expect("invalid response body");
    assert_eq!(body["code"], "ORDER_NOT_MODIFIABLE");

    let submitted = adapter.submitted_orders();
    let legs = exit_legs(&submitted);
    assert_eq!(
        legs.len(),
        4,
        "expected original SL, TP, the rejected replacement, and the restore"
    );

    let restored = legs.last().expect("restored leg");
    assert_eq!(
        restored.stop_price,
        Some(dec!(90)),
        "the restore must re-place the exit at its original trigger, not the requested one"
    );
    assert_eq!(restored.position_id.as_deref(), Some(POSITION_ID));
}

#[tokio::test]
async fn a_failed_restore_reports_the_position_as_unprotected() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(
                SL_ORDER_ID,
                Err(GatewayError::unsupported("modify_order", "mock")),
            )
            .with_cancel_order_response(SL_ORDER_ID, cancelled(SL_ORDER_ID))
            .with_submit_order_response(Err(GatewayError::OrderRejected {
                reason: "stop price crosses the market".to_string(),
                reject_code: None,
                details: None,
            }))
            .with_submit_order_response(Err(GatewayError::ProviderUnavailable {
                message: "connection reset".to_string(),
            })),
    );
    let (gw, mut client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "95" })).await;
    assert_eq!(
        resp.status(),
        502,
        "the exit could not be restored -- this must not read as success"
    );

    let body: serde_json::Value = resp.json().await.expect("invalid response body");
    assert_eq!(body["code"], "PROVIDER_ERROR");

    match client.recv_message(TIMEOUT).await {
        Some(WsMessage::Error { code, details, .. }) => {
            assert_eq!(format!("{code:?}"), "PositionUnprotected");
            let details = details.expect("unprotected notification carries details");
            assert_eq!(details["position_id"], POSITION_ID);
        }
        other => panic!("expected a PositionUnprotected broadcast, got {other:?}"),
    }
}

#[tokio::test]
async fn a_second_move_targets_the_replacement_order() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(
                SL_ORDER_ID,
                Err(GatewayError::unsupported("modify_order", "mock")),
            )
            .with_cancel_order_response(SL_ORDER_ID, cancelled(SL_ORDER_ID))
            .with_submit_order_response(handle_with_id("sl-order-2"))
            .with_modify_order_response("sl-order-2", modify_result("sl-order-2")),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "95" })).await;
    assert_eq!(resp.status(), 200);

    // The entry must now point at the replacement. If it still tracked the
    // cancelled order, this second move would address a dead id -- the same
    // staleness that would later make cancel-on-close miss the live exit.
    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "97" })).await;
    assert_eq!(resp.status(), 200);

    let modified = adapter.modified_orders();
    assert_eq!(
        modified.last().expect("second move").0,
        "sl-order-2",
        "the second move must address the replacement, not the cancelled order"
    );
}

#[tokio::test]
async fn a_leg_split_across_partial_fills_moves_every_resting_order() {
    let mut position = test_position();
    position.id = POSITION_ID.to_string();
    position.symbol = "AAPL".to_string();

    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_position(position)
            .with_submit_order_response(handle_with_id("parent-1"))
            .with_submit_order_response(handle_with_id("sl-part-1"))
            .with_submit_order_response(handle_with_id("tp-part-1"))
            .with_submit_order_response(handle_with_id("sl-part-2"))
            .with_submit_order_response(handle_with_id("tp-part-2"))
            .with_modify_order_response("sl-part-1", modify_result("sl-part-1"))
            .with_modify_order_response("sl-part-2", modify_result("sl-part-2")),
    );

    let gw = spawn_test_gateway_with_exit_management(Arc::clone(&adapter)).await;
    let mut client = StrategyClient::connect(&gw).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/orders", gw.base_url()))
        .json(&serde_json::json!({
            "symbol": "AAPL",
            "side": "BUY",
            "order_type": "MARKET",
            "quantity": "1",
            "stop_loss": "90",
            "take_profit": "120",
            "client_order_id": "parent-1"
        }))
        .send()
        .await
        .expect("submit request failed");
    assert_eq!(resp.status(), 201);

    // Each fill places a leg sized to its portion, so the stop-loss ends up
    // resting as two separate orders.
    inject_fill(&gw, &mut client, dec!(0.4), dec!(1)).await;
    inject_fill(&gw, &mut client, dec!(1), dec!(1)).await;

    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "95" })).await;
    assert_eq!(resp.status(), 200);

    let moved: Vec<String> = adapter
        .modified_orders()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        moved.contains(&"sl-part-1".to_string()) && moved.contains(&"sl-part-2".to_string()),
        "every resting order for the leg must move, else part of the position \
         keeps the old stop while the API reports success — got {moved:?}"
    );

    let body: serde_json::Value = resp.json().await.expect("invalid response body");
    let ids = body["stop_loss"]["order_ids"]
        .as_array()
        .expect("order_ids array");
    assert_eq!(ids.len(), 2, "the response must name both resting orders");
}

#[tokio::test]
async fn a_two_leg_request_that_fails_halfway_keeps_the_leg_it_already_moved() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(SL_ORDER_ID, modify_result(SL_ORDER_ID))
            .with_modify_order_response(
                TP_ORDER_ID,
                Err(GatewayError::ProviderUnavailable {
                    message: "connection reset".to_string(),
                }),
            ),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(
        &gw,
        POSITION_ID,
        &serde_json::json!({ "stop_loss": "95", "take_profit": "130" }),
    )
    .await;
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "a request that did not fully apply must not report success"
    );

    // Legs move one at a time and are not rolled back as a set: the stop-loss
    // move stands even though the take-profit failed. Callers re-read the
    // position rather than assuming the request was all-or-nothing.
    let moved: Vec<String> = adapter
        .modified_orders()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        moved.contains(&SL_ORDER_ID.to_string()),
        "the stop-loss move already happened and is not undone"
    );
    assert!(
        adapter.cancelled_orders().is_empty(),
        "the failed take-profit must leave its resting order live"
    );
}

#[tokio::test]
async fn a_failed_cancel_leaves_the_exit_untouched() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(
                SL_ORDER_ID,
                Err(GatewayError::unsupported("modify_order", "mock")),
            )
            .with_cancel_order_response(
                SL_ORDER_ID,
                Err(GatewayError::ProviderUnavailable {
                    message: "connection reset".to_string(),
                }),
            ),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let before = exit_legs(&adapter.submitted_orders()).len();
    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({ "stop_loss": "95" })).await;
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "a cancel that failed cannot report a successful move"
    );

    assert_eq!(
        exit_legs(&adapter.submitted_orders()).len(),
        before,
        "no replacement may be placed when the original was never cancelled — \
         that would double the position's exit orders"
    );
}

#[tokio::test]
async fn a_take_profit_falls_back_to_cancel_replace_too() {
    let adapter = Arc::new(
        adapter_with_open_position()
            .with_modify_order_response(
                TP_ORDER_ID,
                Err(GatewayError::unsupported("modify_order", "mock")),
            )
            .with_cancel_order_response(TP_ORDER_ID, cancelled(TP_ORDER_ID))
            .with_submit_order_response(handle_with_id("tp-order-2")),
    );
    let (gw, _client) = open_position_with_exits(Arc::clone(&adapter)).await;

    let resp = patch_position(
        &gw,
        POSITION_ID,
        &serde_json::json!({ "take_profit": "130" }),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let submitted = adapter.submitted_orders();
    let replacement = exit_legs(&submitted).last().copied().expect("replacement");
    assert_eq!(
        replacement.limit_price,
        Some(dec!(130)),
        "a replaced take-profit must rest as a limit order at the new price"
    );
    assert_eq!(
        replacement.quantity,
        dec!(1),
        "the replacement must cover the same quantity as the order it replaced"
    );
}

#[tokio::test]
async fn an_unknown_position_cannot_be_moved() {
    let adapter = Arc::new(adapter_with_open_position());
    let (gw, _client) = open_position_with_exits(adapter).await;

    let resp = patch_position(
        &gw,
        "pos-unknown",
        &serde_json::json!({ "stop_loss": "95" }),
    )
    .await;
    assert_eq!(resp.status(), 404, "no such position at the provider");
}

#[tokio::test]
async fn a_leg_that_was_never_placed_cannot_be_moved() {
    let mut position = test_position();
    position.id = POSITION_ID.to_string();
    position.symbol = "AAPL".to_string();

    let adapter = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_bracket_strategy(BracketStrategy::PendingSlTp)
            .with_position(position)
            .with_submit_order_response(handle_with_id("parent-1"))
            .with_submit_order_response(handle_with_id(SL_ORDER_ID)),
    );
    let (gw, _client) = open_position(
        Arc::clone(&adapter),
        &serde_json::json!({
            "symbol": "AAPL",
            "side": "BUY",
            "order_type": "MARKET",
            "quantity": "1",
            "stop_loss": "90",
            "client_order_id": "parent-1"
        }),
    )
    .await;

    let resp = patch_position(
        &gw,
        POSITION_ID,
        &serde_json::json!({ "take_profit": "130" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        409,
        "the position exists but has no take-profit resting to move"
    );

    let body: serde_json::Value = resp.json().await.expect("invalid response body");
    assert_eq!(body["code"], "ORDER_NOT_MODIFIABLE");
    assert!(
        adapter.cancelled_orders().is_empty(),
        "a leg that cannot be moved must leave the other leg alone"
    );
}

#[tokio::test]
async fn a_request_that_moves_nothing_is_rejected() {
    let adapter = Arc::new(adapter_with_open_position());
    let (gw, _client) = open_position_with_exits(adapter).await;

    let resp = patch_position(&gw, POSITION_ID, &serde_json::json!({})).await;
    assert_eq!(
        resp.status(),
        400,
        "an empty modification is a client mistake, not a no-op success"
    );
}
