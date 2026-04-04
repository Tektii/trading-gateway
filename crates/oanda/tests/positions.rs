mod helpers;

use helpers::{oanda_position_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{ClosePositionRequest, OrderStatus, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

// =========================================================================
// Get All Positions
// =========================================================================

#[tokio::test]
async fn get_positions_all() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/openPositions",
        200,
        json!({
            "positions": [
                oanda_position_json("EUR_USD", "10000", "0"),
                oanda_position_json("GBP_USD", "5000", "0"),
            ]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].symbol, "EUR_USD");
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[0].quantity, dec!(10000));
    assert_eq!(positions[1].symbol, "GBP_USD");
}

#[tokio::test]
async fn get_positions_with_both_sides() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/openPositions",
        200,
        json!({
            "positions": [
                oanda_position_json("EUR_USD", "10000", "5000"),
            ]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    // One Oanda position with both sides → 2 gateway positions
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[0].quantity, dec!(10000));
    assert_eq!(positions[0].id, "EUR_USD_LONG");
    assert_eq!(positions[1].side, PositionSide::Short);
    assert_eq!(positions[1].quantity, dec!(5000));
    assert_eq!(positions[1].id, "EUR_USD_SHORT");
}

#[tokio::test]
async fn get_positions_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/openPositions",
        200,
        json!({"positions": []}),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

// =========================================================================
// Get Single Position (various ID formats)
// =========================================================================

#[tokio::test]
async fn get_position_long_suffix() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/positions/EUR_USD",
        200,
        json!({"position": oanda_position_json("EUR_USD", "10000", "0")}),
    )
    .await;

    let position = adapter.get_position("EUR_USD_LONG").await.unwrap();
    assert_eq!(position.side, PositionSide::Long);
    assert_eq!(position.quantity, dec!(10000));
    assert_eq!(position.symbol, "EUR_USD");
    assert_eq!(position.average_entry_price, dec!(1.10000));
}

#[tokio::test]
async fn get_position_short_suffix() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/positions/EUR_USD",
        200,
        json!({"position": oanda_position_json("EUR_USD", "0", "5000")}),
    )
    .await;

    let position = adapter.get_position("EUR_USD_SHORT").await.unwrap();
    assert_eq!(position.side, PositionSide::Short);
    assert_eq!(position.quantity, dec!(5000));
}

#[tokio::test]
async fn get_position_oanda_format() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/positions/EUR_USD",
        200,
        json!({"position": oanda_position_json("EUR_USD", "10000", "0")}),
    )
    .await;

    // Raw Oanda format — no prefix, no suffix
    let position = adapter.get_position("EUR_USD").await.unwrap();
    assert_eq!(position.side, PositionSide::Long);
}

#[tokio::test]
async fn get_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/positions/EUR_USD",
        404,
        json!({"errorMessage": "Position not found"}),
    )
    .await;

    let err = adapter.get_position("EUR_USD_LONG").await.unwrap_err();
    assert!(matches!(err, GatewayError::PositionNotFound { .. }));
}

#[tokio::test]
async fn get_position_empty_sides() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Both sides have 0 units → translate returns empty vec → PositionNotFound
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/positions/EUR_USD",
        200,
        json!({"position": oanda_position_json("EUR_USD", "0", "0")}),
    )
    .await;

    let err = adapter.get_position("EUR_USD_LONG").await.unwrap_err();
    assert!(matches!(err, GatewayError::PositionNotFound { .. }));
}

// =========================================================================
// Close Position
// =========================================================================

#[tokio::test]
async fn close_position_long_full() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/positions/EUR_USD/close",
        200,
        json!({
            "longOrderFillTransaction": {
                "id": "500",
                "type": "ORDER_FILL",
                "instrument": "EUR_USD"
            }
        }),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };
    let handle = adapter
        .close_position("EUR_USD_LONG", &request)
        .await
        .unwrap();
    assert_eq!(handle.id, "500");
    assert_eq!(handle.status, OrderStatus::Filled);
}

#[tokio::test]
async fn close_position_short_partial() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/positions/EUR_USD/close",
        200,
        json!({
            "shortOrderFillTransaction": {
                "id": "501",
                "type": "ORDER_FILL",
                "instrument": "EUR_USD"
            }
        }),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: Some(dec!(5000)),
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };
    let handle = adapter
        .close_position("EUR_USD_SHORT", &request)
        .await
        .unwrap();
    assert_eq!(handle.id, "501");
    assert_eq!(handle.status, OrderStatus::Filled);
}

#[tokio::test]
async fn close_position_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/positions/EUR_USD/close",
        404,
        json!({"errorMessage": "Position not found"}),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };
    let err = adapter
        .close_position("EUR_USD_LONG", &request)
        .await
        .unwrap_err();
    assert!(matches!(err, GatewayError::PositionNotFound { .. }));
}
