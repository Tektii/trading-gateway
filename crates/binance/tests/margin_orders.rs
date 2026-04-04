mod helpers;

use helpers::{binance_margin_order_json, test_margin_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderRequest, OrderQueryParams, OrderRequest, OrderStatus, OrderType, Side,
};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

fn market_buy_request() -> OrderRequest {
    OrderRequest {
        symbol: "BTCUSDT".into(),
        ..test_order_request()
    }
}

// =========================================================================
// Submit Order
// =========================================================================

#[tokio::test]
async fn submit_market_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/sapi/v1/margin/order",
        200,
        binance_margin_order_json(&json!({})),
    )
    .await;

    let handle = adapter.submit_order(&market_buy_request()).await.unwrap();
    assert_eq!(handle.id, "789012");
    assert_eq!(handle.status, OrderStatus::Open);
    assert_eq!(handle.client_order_id.as_deref(), Some("margin-client-001"));
}

#[tokio::test]
async fn submit_limit_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/sapi/v1/margin/order",
        200,
        binance_margin_order_json(&json!({
            "type": "LIMIT",
            "price": "41000.00000000"
        })),
    )
    .await;

    let mut req = market_buy_request();
    req.order_type = OrderType::Limit;
    req.limit_price = Some(dec!(41000));

    let handle = adapter.submit_order(&req).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_order_rejected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/sapi/v1/margin/order",
        400,
        json!({"code": -2010, "msg": "Account has insufficient balance for requested action."}),
    )
    .await;

    let err = adapter
        .submit_order(&market_buy_request())
        .await
        .unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderRejected { .. }),
        "Expected OrderRejected, got: {err:?}"
    );
}

// =========================================================================
// Get Order
// =========================================================================

#[tokio::test]
async fn get_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    // get_order first fetches openOrders to find the symbol
    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/openOrders",
        200,
        json!([binance_margin_order_json(&json!({}))]),
    )
    .await;

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/order",
        200,
        binance_margin_order_json(&json!({})),
    )
    .await;

    let order = adapter.get_order("789012").await.unwrap();
    assert_eq!(order.id, "789012");
    assert_eq!(order.symbol, "BTCUSDT");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.status, OrderStatus::Open);
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    // Empty open orders list — order not found
    mount_json(&server, "GET", "/sapi/v1/margin/openOrders", 200, json!([])).await;

    let err = adapter.get_order("999999").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotFound { .. }),
        "Expected OrderNotFound, got: {err:?}"
    );
}

// =========================================================================
// Get Orders
// =========================================================================

#[tokio::test]
async fn get_orders_open() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/openOrders",
        200,
        json!([
            binance_margin_order_json(&json!({})),
            binance_margin_order_json(&json!({"orderId": 789_013, "symbol": "ETHUSDT"}))
        ]),
    )
    .await;

    let params = OrderQueryParams {
        status: Some(vec![OrderStatus::Open]),
        ..Default::default()
    };
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
}

// =========================================================================
// Modify Order
// =========================================================================

#[tokio::test]
async fn modify_order_unsupported() {
    let (_, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(42000)),
        quantity: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let err = adapter.modify_order("789012", &request).await.unwrap_err();
    assert!(
        matches!(err, GatewayError::UnsupportedOperation { .. }),
        "Expected UnsupportedOperation, got: {err:?}"
    );
}

// =========================================================================
// Cancel Order
// =========================================================================

#[tokio::test]
async fn cancel_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    // 1) GET openOrders to find symbol
    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/openOrders",
        200,
        json!([binance_margin_order_json(&json!({}))]),
    )
    .await;

    // 2) POST cancel (signed_post to same path — returns order-shaped JSON)
    mount_json(
        &server,
        "POST",
        "/sapi/v1/margin/order",
        200,
        binance_margin_order_json(&json!({"status": "CANCELED"})),
    )
    .await;

    // 3) GET re-fetch the cancelled order
    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/order",
        200,
        binance_margin_order_json(&json!({"status": "CANCELED"})),
    )
    .await;

    let result = adapter.cancel_order("789012").await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}
