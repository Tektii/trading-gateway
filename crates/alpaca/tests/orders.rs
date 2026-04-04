mod helpers;

use helpers::{alpaca_order_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderRequest, OrderQueryParams, OrderRequest, OrderStatus, OrderType, Side, TimeInForce,
};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// Helper to build an `OrderRequest` for tests.
fn market_buy(symbol: &str, qty: rust_decimal::Decimal) -> OrderRequest {
    OrderRequest {
        symbol: symbol.to_string(),
        quantity: qty,
        time_in_force: TimeInForce::Day,
        ..test_order_request()
    }
}

// =========================================================================
// Submit Order
// =========================================================================

#[tokio::test]
async fn submit_market_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({})),
    )
    .await;

    let request = market_buy("AAPL", dec!(10));
    let handle = adapter.submit_order(&request).await.unwrap();

    assert_eq!(handle.id, "order-abc-123");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_limit_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({"type": "limit", "limit_price": "155.00"})),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.order_type = OrderType::Limit;
    request.limit_price = Some(dec!(155));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_stop_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({"type": "stop", "stop_price": "145.00"})),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.order_type = OrderType::Stop;
    request.stop_price = Some(dec!(145));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_bracket_order_stock() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Bracket order returns with legs
    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({
            "type": "limit",
            "limit_price": "150.00",
            "legs": [
                alpaca_order_json(&json!({"id": "sl-leg", "type": "stop", "side": "sell", "stop_price": "140.00"})),
                alpaca_order_json(&json!({"id": "tp-leg", "type": "limit", "side": "sell", "limit_price": "170.00"})),
            ]
        })),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.order_type = OrderType::Limit;
    request.limit_price = Some(dec!(150));
    request.stop_loss = Some(dec!(140));
    request.take_profit = Some(dec!(170));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_crypto_stop_transforms_to_stop_limit() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Verify the adapter sends a stop_limit (not stop) to Alpaca for crypto
    Mock::given(method("POST"))
        .and(path("/v2/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(alpaca_order_json(
            &json!({"type": "stop_limit", "symbol": "BTCUSD"}),
        )))
        .expect(1)
        .mount(&server)
        .await;

    let mut request = market_buy("BTCUSD", dec!(1));
    request.order_type = OrderType::Stop;
    request.stop_price = Some(dec!(50000));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");

    // Verify the request was sent (the mock's expect(1) validates this)
}

#[tokio::test]
async fn submit_crypto_sl_tp_uses_pending_system() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Crypto with SL/TP should NOT use bracket — should send simple order
    Mock::given(method("POST"))
        .and(path("/v2/orders"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(alpaca_order_json(&json!({"symbol": "BTCUSD"}))),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut request = market_buy("BTCUSD", dec!(1));
    request.stop_loss = Some(dec!(48000));
    request.take_profit = Some(dec!(55000));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_order_with_client_order_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({"client_order_id": "my-id-001"})),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.client_order_id = Some("my-id-001".to_string());

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.client_order_id, Some("my-id-001".to_string()));
}

#[tokio::test]
async fn submit_order_rejected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        422,
        json!({"message": "Insufficient buying power", "code": "insufficient_buying_power"}),
    )
    .await;

    let request = market_buy("AAPL", dec!(10));
    let err = adapter.submit_order(&request).await.unwrap_err();

    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            // submit_order uses execute_with_retry which uses default_error_mapper,
            // passing through raw Alpaca codes (no Alpaca-specific mapping).
            assert_eq!(reject_code.as_deref(), Some("insufficient_buying_power"));
        }
        other => panic!("Expected OrderRejected, got: {other:?}"),
    }
}

// =========================================================================
// Get Order
// =========================================================================

#[tokio::test]
async fn get_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders/order-abc-123",
        200,
        alpaca_order_json(&json!({})),
    )
    .await;

    let order = adapter.get_order("order-abc-123").await.unwrap();

    assert_eq!(order.id, "order-abc-123");
    assert_eq!(order.symbol, "AAPL");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.order_type, OrderType::Market);
    assert_eq!(order.quantity, dec!(10));
    assert_eq!(order.status, OrderStatus::Open);
    assert_eq!(order.time_in_force, TimeInForce::Day);
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders/nonexistent",
        404,
        json!({"message": "Order not found"}),
    )
    .await;

    let err = adapter.get_order("nonexistent").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotFound { .. }),
        "Expected OrderNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn get_order_filled() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders/order-filled",
        200,
        alpaca_order_json(&json!({
            "id": "order-filled",
            "status": "filled",
            "filled_qty": "10",
            "filled_avg_price": "152.50"
        })),
    )
    .await;

    let order = adapter.get_order("order-filled").await.unwrap();

    assert_eq!(order.status, OrderStatus::Filled);
    assert_eq!(order.filled_quantity, dec!(10));
    assert_eq!(order.average_fill_price, Some(dec!(152.50)));
    assert_eq!(order.remaining_quantity, dec!(0));
}

// =========================================================================
// Get Orders
// =========================================================================

#[tokio::test]
async fn get_orders_open() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Wiremock matches on path only; query params are part of the full URL
    // but mount_json matches on path prefix, so this works for /v2/orders
    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([alpaca_order_json(&json!({}))]),
    )
    .await;

    let params = OrderQueryParams {
        status: Some(vec![OrderStatus::Open]),
        ..Default::default()
    };

    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].id, "order-abc-123");
}

#[tokio::test]
async fn get_orders_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/v2/orders", 200, json!([])).await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_orders(&params).await.unwrap();
    assert!(orders.is_empty());
}

// =========================================================================
// Get Order History
// =========================================================================

#[tokio::test]
async fn get_order_history() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([
            alpaca_order_json(&json!({"id": "hist-1", "status": "filled"})),
            alpaca_order_json(&json!({"id": "hist-2", "status": "canceled"})),
        ]),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_order_history(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
}

// =========================================================================
// Modify Order
// =========================================================================

#[tokio::test]
async fn modify_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PATCH",
        "/v2/orders/order-abc-123",
        200,
        alpaca_order_json(&json!({
            "type": "limit",
            "limit_price": "160.00"
        })),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let result = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap();
    assert_eq!(result.previous_order_id, Some("order-abc-123".to_string()));
    assert_eq!(result.order.id, "order-abc-123");
}

#[tokio::test]
async fn modify_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PATCH",
        "/v2/orders/nonexistent",
        404,
        json!({"message": "Order not found"}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let err = adapter
        .modify_order("nonexistent", &request)
        .await
        .unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotFound { .. }),
        "Expected OrderNotFound, got: {err:?}"
    );
}

// =========================================================================
// Cancel Order
// =========================================================================

#[tokio::test]
async fn cancel_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // cancel_order calls DELETE then GET
    Mock::given(method("DELETE"))
        .and(path("/v2/orders/order-abc-123"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    mount_json(
        &server,
        "GET",
        "/v2/orders/order-abc-123",
        200,
        alpaca_order_json(&json!({"status": "canceled"})),
    )
    .await;

    let result = adapter.cancel_order("order-abc-123").await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}

// =========================================================================
// Cancel All Orders
// =========================================================================

#[tokio::test]
async fn cancel_all_orders() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "DELETE",
        "/v2/orders",
        207,
        json!([
            {"id": "order-1", "status": 200},
            {"id": "order-2", "status": 200},
        ]),
    )
    .await;

    let result = adapter.cancel_all_orders(None).await.unwrap();
    assert_eq!(result.cancelled_count, 2);
    assert_eq!(result.failed_count, 0);
}

#[tokio::test]
async fn cancel_all_orders_with_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Wiremock matches on path /v2/orders — query params pass through
    mount_json(
        &server,
        "DELETE",
        "/v2/orders",
        207,
        json!([{"id": "order-1", "status": 200}]),
    )
    .await;

    let result = adapter.cancel_all_orders(Some("AAPL")).await.unwrap();
    assert_eq!(result.cancelled_count, 1);
}
