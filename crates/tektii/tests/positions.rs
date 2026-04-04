mod helpers;

use helpers::{engine_order_handle_json, engine_position_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{ClosePositionRequest, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_positions_all() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/positions",
        200,
        json!({
            "positions": [
                engine_position_json(&json!({"id": "pos-001", "symbol": "AAPL"})),
                engine_position_json(&json!({"id": "pos-002", "symbol": "GOOG", "side": "short"})),
            ]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].id, "pos-001");
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[1].id, "pos-002");
    assert_eq!(positions[1].side, PositionSide::Short);
}

#[tokio::test]
async fn get_positions_with_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/positions",
        200,
        json!({
            "positions": [
                engine_position_json(&json!({"id": "pos-001", "symbol": "AAPL"})),
            ]
        }),
    )
    .await;

    let positions = adapter.get_positions(Some("AAPL")).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].symbol, "AAPL");
}

#[tokio::test]
async fn get_positions_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/positions",
        200,
        json!({"positions": []}),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

#[tokio::test]
async fn get_position_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/positions",
        200,
        json!({
            "positions": [
                engine_position_json(&json!({"id": "pos-001"})),
                engine_position_json(&json!({"id": "pos-002"})),
            ]
        }),
    )
    .await;

    let position = adapter.get_position("pos-001").await.unwrap();
    assert_eq!(position.id, "pos-001");
    assert_eq!(position.quantity, dec!(10));
    assert_eq!(position.average_entry_price, dec!(150));
    assert_eq!(position.current_price, dec!(155));
    assert_eq!(position.unrealized_pnl, dec!(50));
}

#[tokio::test]
async fn get_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/positions",
        200,
        json!({"positions": []}),
    )
    .await;

    let err = adapter.get_position("nonexistent").await.unwrap_err();
    assert!(matches!(err, GatewayError::PositionNotFound { .. }));
}

#[tokio::test]
async fn close_position_full() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // close_position calls get_position (GET all) then submit_order (POST)
    mount_json(
        &server,
        "GET",
        "/api/v1/positions",
        200,
        json!({
            "positions": [
                engine_position_json(&json!({"id": "pos-001", "side": "long", "quantity": "10"})),
            ]
        }),
    )
    .await;
    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        200,
        engine_order_handle_json(&json!({"id": "close-order-1"})),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: Some(tektii_gateway_core::models::OrderType::Market),
        limit_price: None,
        cancel_associated_orders: false,
    };
    let handle = adapter.close_position("pos-001", &request).await.unwrap();
    assert_eq!(handle.id, "close-order-1");
}

#[tokio::test]
async fn close_position_partial() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/positions",
        200,
        json!({
            "positions": [
                engine_position_json(&json!({"id": "pos-001", "side": "short", "quantity": "20"})),
            ]
        }),
    )
    .await;
    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        200,
        engine_order_handle_json(&json!({"id": "close-order-2"})),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: Some(dec!(5)),
        order_type: Some(tektii_gateway_core::models::OrderType::Market),
        limit_price: None,
        cancel_associated_orders: false,
    };
    let handle = adapter.close_position("pos-001", &request).await.unwrap();
    assert_eq!(handle.id, "close-order-2");
}
