mod helpers;

use helpers::{binance_cancel_replace_json, binance_spot_order_json, test_spot_adapter};
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
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({})),
    )
    .await;

    let handle = adapter.submit_order(&market_buy_request()).await.unwrap();
    assert_eq!(handle.id, "123456");
    assert_eq!(handle.status, OrderStatus::Open);
    assert_eq!(handle.client_order_id.as_deref(), Some("client-001"));
}

#[tokio::test]
async fn submit_limit_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({
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
async fn submit_stop_limit_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({
            "type": "STOP_LOSS_LIMIT",
            "price": "40000.00000000",
            "stopPrice": "40500.00000000"
        })),
    )
    .await;

    let mut req = market_buy_request();
    req.order_type = OrderType::StopLimit;
    req.limit_price = Some(dec!(40000));
    req.stop_price = Some(dec!(40500));

    let handle = adapter.submit_order(&req).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_order_with_client_order_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({ "clientOrderId": "my-custom-id" })),
    )
    .await;

    let mut req = market_buy_request();
    req.client_order_id = Some("my-custom-id".to_string());

    let handle = adapter.submit_order(&req).await.unwrap();
    assert_eq!(handle.client_order_id.as_deref(), Some("my-custom-id"));
}

#[tokio::test]
async fn submit_order_rejected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order",
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
    let adapter = test_spot_adapter(&base_url);

    // get_order first fetches openOrders to find the symbol
    mount_json(
        &server,
        "GET",
        "/api/v3/openOrders",
        200,
        json!([binance_spot_order_json(&json!({}))]),
    )
    .await;

    mount_json(
        &server,
        "GET",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({})),
    )
    .await;

    let order = adapter.get_order("123456").await.unwrap();
    assert_eq!(order.id, "123456");
    assert_eq!(order.symbol, "BTCUSDT");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.status, OrderStatus::Open);
}

#[tokio::test]
async fn get_order_filled() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    let filled_order = binance_spot_order_json(&json!({
        "status": "FILLED",
        "executedQty": "1.00000000",
        "cummulativeQuoteQty": "42000.00000000"
    }));

    mount_json(
        &server,
        "GET",
        "/api/v3/openOrders",
        200,
        json!([filled_order.clone()]),
    )
    .await;

    mount_json(&server, "GET", "/api/v3/order", 200, filled_order).await;

    let order = adapter.get_order("123456").await.unwrap();
    assert_eq!(order.status, OrderStatus::Filled);
    assert_eq!(order.filled_quantity, dec!(1));
    assert!(order.average_fill_price.is_some());
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    // Empty open orders list — order not found
    mount_json(&server, "GET", "/api/v3/openOrders", 200, json!([])).await;

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
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/openOrders",
        200,
        json!([
            binance_spot_order_json(&json!({})),
            binance_spot_order_json(&json!({"orderId": 123_457, "symbol": "ETHUSDT"}))
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

#[tokio::test]
async fn get_orders_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(&server, "GET", "/api/v3/openOrders", 200, json!([])).await;

    let params = OrderQueryParams {
        status: Some(vec![OrderStatus::Open]),
        ..Default::default()
    };
    let orders = adapter.get_orders(&params).await.unwrap();
    assert!(orders.is_empty());
}

// =========================================================================
// Modify Order (cancel-replace)
// =========================================================================

#[tokio::test]
async fn modify_order_cancel_replace() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    // modify_order first GETs the existing order (by orderId param)
    mount_json(
        &server,
        "GET",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({
            "type": "LIMIT",
            "price": "41000.00000000",
            "status": "NEW"
        })),
    )
    .await;

    // Then POSTs cancel-replace
    mount_json(
        &server,
        "POST",
        "/api/v3/order/cancelReplace",
        200,
        binance_cancel_replace_json(&json!({})),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(41500)),
        quantity: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let result = adapter.modify_order("123456", &request).await.unwrap();
    assert_eq!(result.previous_order_id.as_deref(), Some("123456"));
    assert_eq!(result.order.id, "123457");
    assert_eq!(result.order.status, OrderStatus::Open);
}

#[tokio::test]
async fn modify_order_not_modifiable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({ "status": "FILLED" })),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(42000)),
        quantity: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let err = adapter.modify_order("123456", &request).await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotModifiable { .. }),
        "Expected OrderNotModifiable, got: {err:?}"
    );
}

// =========================================================================
// Cancel Order
// =========================================================================

#[tokio::test]
async fn cancel_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    // cancel_order: 1) GET openOrders to find symbol, 2) DELETE order, 3) GET to confirm
    mount_json(
        &server,
        "GET",
        "/api/v3/openOrders",
        200,
        json!([binance_spot_order_json(&json!({}))]),
    )
    .await;

    // DELETE returns empty
    mount_json(&server, "DELETE", "/api/v3/order", 200, json!({})).await;

    // Re-fetch returns cancelled order
    mount_json(
        &server,
        "GET",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({"status": "CANCELED"})),
    )
    .await;

    let result = adapter.cancel_order("123456").await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn cancel_all_orders_with_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "DELETE",
        "/api/v3/openOrders",
        200,
        json!([{"orderId": 1}, {"orderId": 2}]),
    )
    .await;

    let result = adapter.cancel_all_orders(Some("BTCUSDT")).await.unwrap();
    assert_eq!(result.cancelled_count, 2);
}
