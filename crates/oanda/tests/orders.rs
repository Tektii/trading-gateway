mod helpers;

use helpers::{
    oanda_market_fill_json, oanda_order_json, oanda_pending_order_json, oanda_reject_json,
    test_adapter,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderRequest, OrderQueryParams, OrderRequest, OrderStatus, OrderType, Side, TimeInForce,
};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

/// Helper to build an `OrderRequest` for Oanda forex tests.
fn forex_order(symbol: &str, side: Side, order_type: OrderType, qty: Decimal) -> OrderRequest {
    OrderRequest {
        symbol: symbol.to_string(),
        side,
        quantity: qty,
        order_type,
        ..test_order_request()
    }
}

// =========================================================================
// Submit Order
// =========================================================================

#[tokio::test]
async fn submit_market_order_fill() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({})),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "456");
    assert_eq!(handle.status, OrderStatus::Filled);
}

#[tokio::test]
async fn submit_limit_order_pending() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_pending_order_json(&json!({})),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "789");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_stop_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_pending_order_json(&json!({"id": "790"})),
    )
    .await;

    let request = OrderRequest {
        stop_price: Some(dec!(1.09000)),
        ..forex_order("EUR_USD", Side::Sell, OrderType::Stop, dec!(5000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "790");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_order_with_bracket() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"id": "460"})),
    )
    .await;

    let request = OrderRequest {
        stop_loss: Some(dec!(1.08000)),
        take_profit: Some(dec!(1.12000)),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "460");
    assert_eq!(handle.status, OrderStatus::Filled);
}

#[tokio::test]
async fn submit_order_with_client_order_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_pending_order_json(&json!({})),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        client_order_id: Some("my-client-001".to_string()),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.client_order_id, Some("my-client-001".to_string()));
}

#[tokio::test]
async fn submit_order_rejected_via_transaction() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_reject_json("INSUFFICIENT_MARGIN"),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let err = adapter.submit_order(&request).await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, Some("INSUFFICIENT_FUNDS".to_string()));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_order_http_422() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        422,
        json!({"errorMessage": "Order rejected", "rejectReason": "MARKET_HALTED"}),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let err = adapter.submit_order(&request).await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, Some("MARKET_CLOSED".to_string()));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
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
        "/v3/accounts/test-account-123/orders/123",
        200,
        json!({"order": oanda_order_json(&json!({}))}),
    )
    .await;

    let order = adapter.get_order("123").await.unwrap();
    assert_eq!(order.id, "123");
    assert_eq!(order.symbol, "EUR_USD");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.order_type, OrderType::Limit);
    assert_eq!(order.quantity, dec!(10000));
    assert_eq!(order.limit_price, Some(dec!(1.10000)));
    assert_eq!(order.status, OrderStatus::Open);
    assert_eq!(order.time_in_force, TimeInForce::Gtc);
}

#[tokio::test]
async fn get_order_sell_side() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/124",
        200,
        json!({"order": oanda_order_json(&json!({"id": "124", "units": "-5000"}))}),
    )
    .await;

    let order = adapter.get_order("124").await.unwrap();
    assert_eq!(order.side, Side::Sell);
    assert_eq!(order.quantity, dec!(5000));
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/999",
        404,
        json!({"errorMessage": "Order not found"}),
    )
    .await;

    let err = adapter.get_order("999").await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

// =========================================================================
// Get Orders (list)
// =========================================================================

#[tokio::test]
async fn get_orders_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [
                oanda_order_json(&json!({"id": "1"})),
                oanda_order_json(&json!({"id": "2"})),
            ]
        }),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].id, "1");
    assert_eq!(orders[1].id, "2");
}

#[tokio::test]
async fn get_orders_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({"orders": []}),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_orders(&params).await.unwrap();
    assert!(orders.is_empty());
}

#[tokio::test]
async fn get_orders_with_symbol_filter() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [oanda_order_json(&json!({}))]
        }),
    )
    .await;

    let params = OrderQueryParams {
        symbol: Some("EUR_USD".to_string()),
        ..Default::default()
    };
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 1);
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
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [
                oanda_order_json(&json!({"id": "10", "state": "FILLED"})),
                oanda_order_json(&json!({"id": "11", "state": "CANCELLED"})),
            ]
        }),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_order_history(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].status, OrderStatus::Filled);
    assert_eq!(orders[1].status, OrderStatus::Cancelled);
}

// =========================================================================
// Modify Order (PUT replacement)
// =========================================================================

#[tokio::test]
async fn modify_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // 1. GET current order
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/100",
        200,
        json!({"order": oanda_order_json(&json!({"id": "100"}))}),
    )
    .await;

    // 2. PUT replacement -> returns orderCreateTransaction with new ID
    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/100",
        201,
        json!({
            "orderCancelTransaction": {"id": "101", "type": "ORDER_CANCEL"},
            "orderCreateTransaction": {"id": "102", "type": "LIMIT_ORDER"}
        }),
    )
    .await;

    // 3. GET the new order (adapter fetches the replacement)
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/102",
        200,
        json!({"order": oanda_order_json(&json!({"id": "102", "price": "1.11000"}))}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(1.11000)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let result = adapter.modify_order("100", &request).await.unwrap();
    assert_eq!(result.order.id, "102");
    assert_eq!(result.previous_order_id, Some("100".to_string()));
    assert_eq!(result.order.limit_price, Some(dec!(1.11000)));
}

#[tokio::test]
async fn modify_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/999",
        404,
        json!({"errorMessage": "Order not found"}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(1.11000)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let err = adapter.modify_order("999", &request).await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

// =========================================================================
// Cancel Order
// =========================================================================

#[tokio::test]
async fn cancel_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/100/cancel",
        200,
        json!({"orderCancelTransaction": {"id": "100", "type": "ORDER_CANCEL"}}),
    )
    .await;

    let result = adapter.cancel_order("100").await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.id, "100");
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn cancel_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/999/cancel",
        404,
        json!({"errorMessage": "Order not found"}),
    )
    .await;

    let err = adapter.cancel_order("999").await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

// =========================================================================
// Cancel All Orders (default trait impl)
// =========================================================================

#[tokio::test]
async fn cancel_all_orders() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // GET open orders returns 2
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [
                oanda_order_json(&json!({"id": "1"})),
                oanda_order_json(&json!({"id": "2"})),
            ]
        }),
    )
    .await;

    // Cancel each
    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/1/cancel",
        200,
        json!({"orderCancelTransaction": {"id": "1", "type": "ORDER_CANCEL"}}),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/2/cancel",
        200,
        json!({"orderCancelTransaction": {"id": "2", "type": "ORDER_CANCEL"}}),
    )
    .await;

    let result = adapter.cancel_all_orders(None).await.unwrap();
    assert_eq!(result.cancelled_count, 2);
}
