mod helpers;

use helpers::{
    forex_order, saxo_order_json, saxo_order_response_json, saxo_precheck_error_json,
    saxo_precheck_ok_json, test_adapter, test_adapter_with_precheck,
};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderRequest, OrderQueryParams, OrderStatus, OrderType, Side,
};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

// =========================================================================
// Submit order
// =========================================================================

#[tokio::test]
async fn submit_market_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-1"),
    )
    .await;

    let request = forex_order("EURUSD:FxSpot", Side::Buy, OrderType::Market, dec!(100000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-1");
    assert_eq!(handle.status, OrderStatus::Filled);
}

#[tokio::test]
async fn submit_limit_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-2"),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.1)),
        ..forex_order("EURUSD:FxSpot", Side::Buy, OrderType::Limit, dec!(100000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-2");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_stop_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-3"),
    )
    .await;

    let request = OrderRequest {
        stop_price: Some(dec!(1.09)),
        ..forex_order("EURUSD:FxSpot", Side::Sell, OrderType::Stop, dec!(100000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-3");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_stop_limit_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-4"),
    )
    .await;

    let request = OrderRequest {
        stop_price: Some(dec!(1.09)),
        limit_price: Some(dec!(1.085)),
        ..forex_order(
            "EURUSD:FxSpot",
            Side::Sell,
            OrderType::StopLimit,
            dec!(100000),
        )
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-4");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_trailing_stop_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-5"),
    )
    .await;

    let request = OrderRequest {
        trailing_distance: Some(dec!(0.005)),
        ..forex_order(
            "EURUSD:FxSpot",
            Side::Sell,
            OrderType::TrailingStop,
            dec!(100000),
        )
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-5");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_order_with_bracket() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-6"),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.1)),
        stop_loss: Some(dec!(1.08)),
        take_profit: Some(dec!(1.15)),
        ..forex_order("EURUSD:FxSpot", Side::Buy, OrderType::Limit, dec!(100000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-6");
}

#[tokio::test]
async fn submit_order_with_client_order_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-7"),
    )
    .await;

    let request = OrderRequest {
        client_order_id: Some("my-custom-id".to_string()),
        ..forex_order("EURUSD:FxSpot", Side::Buy, OrderType::Market, dec!(100000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-7");
    assert_eq!(handle.client_order_id, Some("my-custom-id".to_string()));
}

#[tokio::test]
async fn submit_order_with_precheck_pass() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter_with_precheck(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders/precheck",
        200,
        saxo_precheck_ok_json(),
    )
    .await;
    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("ORD-8"),
    )
    .await;

    let request = forex_order("EURUSD:FxSpot", Side::Buy, OrderType::Market, dec!(100000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "ORD-8");
}

#[tokio::test]
async fn submit_order_precheck_rejected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter_with_precheck(&server, &base_url).await;

    mount_json(
        &server,
        "POST",
        "/trade/v2/orders/precheck",
        200,
        saxo_precheck_error_json("InsufficientMargin", "Insufficient margin for this order"),
    )
    .await;

    let request = forex_order("EURUSD:FxSpot", Side::Buy, OrderType::Market, dec!(100000));
    let err = adapter.submit_order(&request).await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderRejected { .. }));
}

#[tokio::test]
async fn submit_order_unknown_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    let request = forex_order("UNKNOWN:FxSpot", Side::Buy, OrderType::Market, dec!(100000));
    let err = adapter.submit_order(&request).await.unwrap_err();
    assert!(matches!(err, GatewayError::SymbolNotFound { .. }));
}

// =========================================================================
// Get orders
// =========================================================================

#[tokio::test]
async fn get_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/orders/me",
        200,
        json!({
            "Data": [saxo_order_json(&json!({"OrderId": "ORD-100"}))]
        }),
    )
    .await;

    let order = adapter.get_order("ORD-100").await.unwrap();
    assert_eq!(order.id, "ORD-100");
    assert_eq!(order.symbol, "EURUSD:FxSpot");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.order_type, OrderType::Limit);
    assert_eq!(order.quantity, dec!(100000));
    assert_eq!(order.limit_price, Some(dec!(1.1)));
    assert_eq!(order.status, OrderStatus::Open);
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/orders/me",
        200,
        json!({ "Data": [] }),
    )
    .await;

    let err = adapter.get_order("NONEXISTENT").await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

#[tokio::test]
async fn get_orders_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/orders/me",
        200,
        json!({
            "Data": [
                saxo_order_json(&json!({"OrderId": "ORD-A"})),
                saxo_order_json(&json!({"OrderId": "ORD-B"}))
            ]
        }),
    )
    .await;

    let params = OrderQueryParams {
        symbol: None,
        status: None,
        side: None,
        client_order_id: None,
        oco_group_id: None,
        since: None,
        until: None,
        limit: None,
    };
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
}

#[tokio::test]
async fn get_orders_with_symbol_filter() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/orders/me",
        200,
        json!({
            "Data": [
                saxo_order_json(&json!({"OrderId": "ORD-A", "Uic": 21, "AssetType": "FxSpot"})),
                saxo_order_json(&json!({"OrderId": "ORD-B", "Uic": 101, "AssetType": "CfdOnIndex"}))
            ]
        }),
    )
    .await;

    let params = OrderQueryParams {
        symbol: Some("EURUSD:FxSpot".to_string()),
        status: None,
        side: None,
        client_order_id: None,
        oco_group_id: None,
        since: None,
        until: None,
        limit: None,
    };
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].symbol, "EURUSD:FxSpot");
}

// =========================================================================
// Modify order
// =========================================================================

#[tokio::test]
async fn modify_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // PATCH to modify
    mount_json(&server, "PATCH", "/trade/v2/orders/ORD-123", 200, json!({})).await;

    // GET to fetch updated order
    mount_json(
        &server,
        "GET",
        "/port/v1/orders/me",
        200,
        json!({
            "Data": [saxo_order_json(&json!({
                "OrderId": "ORD-123",
                "Price": 1.12
            }))]
        }),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(1.12)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };
    let result = adapter.modify_order("ORD-123", &request).await.unwrap();
    assert_eq!(result.order.id, "ORD-123");
    assert_eq!(result.order.limit_price, Some(dec!(1.12)));
    assert!(result.previous_order_id.is_none()); // In-place PATCH, not cancel+replace
}

// =========================================================================
// Cancel order
// =========================================================================

#[tokio::test]
async fn cancel_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // DELETE with AccountKey query param — wiremock matches path only
    mount_json(
        &server,
        "DELETE",
        "/trade/v2/orders/ORD-123",
        200,
        json!({}),
    )
    .await;

    let result = adapter.cancel_order("ORD-123").await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.id, "ORD-123");
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}

use tektii_gateway_core::models::OrderRequest;
