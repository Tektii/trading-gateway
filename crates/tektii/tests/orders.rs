mod helpers;

use helpers::{engine_order_handle_json, engine_order_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderRequest, OrderQueryParams, OrderStatus, OrderType, Side,
};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::{mount_empty, mount_json, start_mock_server};

#[tokio::test]
async fn submit_market_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        200,
        engine_order_handle_json(&json!({})),
    )
    .await;

    let request = test_order_request();
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
        "/api/v1/orders",
        200,
        engine_order_handle_json(&json!({})),
    )
    .await;

    let request = tektii_gateway_core::models::OrderRequest {
        order_type: OrderType::Limit,
        limit_price: Some(dec!(155)),
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_order_with_client_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        200,
        engine_order_handle_json(&json!({"client_order_id": "my-client-id"})),
    )
    .await;

    let request = tektii_gateway_core::models::OrderRequest {
        client_order_id: Some("my-client-id".to_string()),
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.client_order_id.as_deref(), Some("my-client-id"));
}

#[tokio::test]
async fn submit_order_tracks_oco_group() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Mount POST for submit and GET for subsequent get_order
    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        200,
        engine_order_handle_json(&json!({})),
    )
    .await;
    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({})),
    )
    .await;

    let request = tektii_gateway_core::models::OrderRequest {
        oco_group_id: Some("group-1".to_string()),
        ..test_order_request()
    };
    adapter.submit_order(&request).await.unwrap();

    // Verify OCO group is enriched on get_order
    let order = adapter.get_order("order-abc-123").await.unwrap();
    assert_eq!(order.oco_group_id.as_deref(), Some("group-1"));
}

#[tokio::test]
async fn get_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({})),
    )
    .await;

    let order = adapter.get_order("order-abc-123").await.unwrap();
    assert_eq!(order.id, "order-abc-123");
    assert_eq!(order.symbol, "AAPL");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.order_type, OrderType::Market);
    assert_eq!(order.quantity, dec!(10));
    assert_eq!(order.filled_quantity, dec!(0));
    assert_eq!(order.remaining_quantity, dec!(10));
    assert_eq!(order.status, OrderStatus::Open);
    assert_eq!(order.client_order_id.as_deref(), Some("client-001"));
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/missing",
        404,
        json!({"error": "not found"}),
    )
    .await;

    let err = adapter.get_order("missing").await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

#[tokio::test]
async fn get_order_partially_filled() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({
            "status": "open",
            "quantity": "10",
            "filled_quantity": "5"
        })),
    )
    .await;

    let order = adapter.get_order("order-abc-123").await.unwrap();
    assert_eq!(order.status, OrderStatus::PartiallyFilled);
    assert_eq!(order.filled_quantity, dec!(5));
    assert_eq!(order.remaining_quantity, dec!(5));
}

#[tokio::test]
async fn get_order_limit_price_mapping() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({
            "order_type": "limit",
            "price": "155"
        })),
    )
    .await;

    let order = adapter.get_order("order-abc-123").await.unwrap();
    assert_eq!(order.order_type, OrderType::Limit);
    assert_eq!(order.limit_price, Some(dec!(155)));
    assert_eq!(order.stop_price, None);
}

#[tokio::test]
async fn get_order_stop_price_mapping() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({
            "order_type": "stop",
            "price": "145"
        })),
    )
    .await;

    let order = adapter.get_order("order-abc-123").await.unwrap();
    assert_eq!(order.order_type, OrderType::Stop);
    assert_eq!(order.limit_price, None);
    assert_eq!(order.stop_price, Some(dec!(145)));
}

#[tokio::test]
async fn get_order_enriches_oco_group() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Pre-populate StateManager with OCO group
    adapter
        .state_manager()
        .add_to_oco_group("order-abc-123", "group-42");

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({})),
    )
    .await;

    let order = adapter.get_order("order-abc-123").await.unwrap();
    assert_eq!(order.oco_group_id.as_deref(), Some("group-42"));
}

#[tokio::test]
async fn get_orders_with_filters() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders",
        200,
        json!({
            "orders": [
                engine_order_json(&json!({"id": "order-1", "symbol": "AAPL"})),
                engine_order_json(&json!({"id": "order-2", "symbol": "AAPL"})),
            ]
        }),
    )
    .await;

    let params = OrderQueryParams {
        symbol: Some("AAPL".to_string()),
        status: Some(vec![OrderStatus::Open]),
        ..Default::default()
    };
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].id, "order-1");
    assert_eq!(orders[1].id, "order-2");
}

#[tokio::test]
async fn get_orders_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/api/v1/orders", 200, json!({"orders": []})).await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_orders(&params).await.unwrap();
    assert!(orders.is_empty());
}

#[tokio::test]
async fn cancel_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // cancel_order does DELETE then GET
    mount_empty(&server, "DELETE", "/api/v1/orders/order-abc-123", 204).await;
    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({"status": "cancelled"})),
    )
    .await;

    let result = adapter.cancel_order("order-abc-123").await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn cancel_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "DELETE",
        "/api/v1/orders/missing",
        404,
        json!({"error": "not found"}),
    )
    .await;

    let err = adapter.cancel_order("missing").await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

#[tokio::test]
async fn cancel_all_orders_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "DELETE",
        "/api/v1/orders",
        200,
        json!({"cancelled_count": 3, "failed_count": 0}),
    )
    .await;

    let result = adapter.cancel_all_orders(None).await.unwrap();
    assert_eq!(result.cancelled_count, 3);
    assert_eq!(result.failed_count, 0);
}

#[tokio::test]
async fn cancel_all_orders_with_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "DELETE",
        "/api/v1/orders",
        200,
        json!({"cancelled_count": 1, "failed_count": 0}),
    )
    .await;

    let result = adapter.cancel_all_orders(Some("AAPL")).await.unwrap();
    assert_eq!(result.cancelled_count, 1);
}

// =========================================================================
// modify_order (cancel + replace) tests
// =========================================================================

/// Helper: mount the full cancel+replace sequence for modify_order.
///
/// `modify_order` calls: GET original → DELETE → GET (inside cancel_order) → POST new → GET new.
/// The first and third calls both hit `GET /api/v1/orders/{id}`. Since `modify_order` doesn't
/// inspect cancel_order's result (only propagates errors via `?`), we mount a single persistent
/// GET that always returns the original order. The cancel flow still succeeds.
async fn mount_modify_sequence(
    server: &wiremock::MockServer,
    original_order: serde_json::Value,
    new_handle: serde_json::Value,
    new_order: serde_json::Value,
) {
    // GET original order (persistent — serves both the initial fetch and cancel's post-delete fetch)
    mount_json(
        server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        original_order,
    )
    .await;

    // DELETE → 204
    mount_empty(server, "DELETE", "/api/v1/orders/order-abc-123", 204).await;

    // POST → new order handle
    mount_json(server, "POST", "/api/v1/orders", 200, new_handle).await;

    // GET new order (different ID — no conflict)
    mount_json(
        server,
        "GET",
        "/api/v1/orders/new-order-456",
        200,
        new_order,
    )
    .await;
}

#[tokio::test]
async fn modify_order_happy_path() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_modify_sequence(
        &server,
        engine_order_json(&json!({"order_type": "limit", "price": "150"})),
        engine_order_handle_json(&json!({"id": "new-order-456"})),
        engine_order_json(&json!({
            "id": "new-order-456",
            "order_type": "limit",
            "price": "160"
        })),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let result = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap();
    assert_eq!(result.order.id, "new-order-456");
    assert_eq!(result.previous_order_id, Some("order-abc-123".to_string()));
    assert_eq!(result.order.limit_price, Some(dec!(160)));
}

#[tokio::test]
async fn modify_order_replacement_fails() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // GET → Open (persistent)
    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({"order_type": "limit", "price": "150"})),
    )
    .await;

    // DELETE → 204
    mount_empty(&server, "DELETE", "/api/v1/orders/order-abc-123", 204).await;

    // POST → 500 (replacement fails)
    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        500,
        json!({"error": "internal server error"}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let err = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap_err();
    // submit_order 500 → ProviderError (the default arm in handle_response)
    assert!(
        matches!(err, GatewayError::ProviderError { .. }),
        "Expected ProviderError, got: {err:?}"
    );
}

#[tokio::test]
async fn modify_order_filled_not_modifiable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({"status": "filled"})),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let err = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap_err();
    match &err {
        GatewayError::OrderNotModifiable { reason, .. } => {
            assert!(
                reason.contains("Filled"),
                "Expected 'Filled' in reason: {reason}"
            );
        }
        _ => panic!("Expected OrderNotModifiable, got: {err:?}"),
    }
}

#[tokio::test]
async fn modify_order_cancelled_not_modifiable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({"status": "cancelled"})),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let err = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotModifiable { .. }),
        "Expected OrderNotModifiable, got: {err:?}"
    );
}

#[tokio::test]
async fn modify_order_rejected_not_modifiable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/order-abc-123",
        200,
        engine_order_json(&json!({"status": "rejected"})),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let err = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotModifiable { .. }),
        "Expected OrderNotModifiable, got: {err:?}"
    );
}

#[tokio::test]
async fn modify_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/orders/missing",
        404,
        json!({"error": "not found"}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let err = adapter.modify_order("missing", &request).await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotFound { .. }),
        "Expected OrderNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn modify_order_preserves_unmodified_fields() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_modify_sequence(
        &server,
        // Original is a limit order with specific fields
        engine_order_json(&json!({
            "order_type": "limit",
            "price": "150",
            "symbol": "TSLA",
            "side": "sell",
            "quantity": "20",
            "client_order_id": "my-client-id"
        })),
        engine_order_handle_json(&json!({"id": "new-order-456"})),
        // New order reflects the preserved fields + modified quantity
        engine_order_json(&json!({
            "id": "new-order-456",
            "order_type": "limit",
            "price": "150",
            "symbol": "TSLA",
            "side": "sell",
            "quantity": "5",
            "client_order_id": "my-client-id"
        })),
    )
    .await;

    // Only modify quantity — everything else should be preserved from original
    let request = ModifyOrderRequest {
        quantity: Some(dec!(5)),
        ..Default::default()
    };

    let result = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap();
    assert_eq!(result.order.id, "new-order-456");
    assert_eq!(result.order.symbol, "TSLA");
    assert_eq!(result.order.side, Side::Sell);
    assert_eq!(result.order.order_type, OrderType::Limit);
    assert_eq!(result.order.limit_price, Some(dec!(150)));
    assert_eq!(result.order.quantity, dec!(5));
    assert_eq!(
        result.order.client_order_id.as_deref(),
        Some("my-client-id")
    );
    assert_eq!(result.previous_order_id, Some("order-abc-123".to_string()));
}

#[tokio::test]
async fn modify_order_partially_filled_modifiable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_modify_sequence(
        &server,
        engine_order_json(&json!({
            "order_type": "limit",
            "price": "150",
            "quantity": "10",
            "filled_quantity": "5",
            "status": "open"
        })),
        engine_order_handle_json(&json!({"id": "new-order-456"})),
        engine_order_json(&json!({
            "id": "new-order-456",
            "order_type": "limit",
            "price": "155"
        })),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(155)),
        ..Default::default()
    };

    let result = adapter
        .modify_order("order-abc-123", &request)
        .await
        .unwrap();
    assert_eq!(result.order.id, "new-order-456");
    assert_eq!(result.previous_order_id, Some("order-abc-123".to_string()));
}
