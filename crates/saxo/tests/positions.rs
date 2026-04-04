mod helpers;

use helpers::{close_request, saxo_order_response_json, saxo_position_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{OrderStatus, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_positions_all() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/netpositions/me",
        200,
        json!({
            "Data": [
                saxo_position_json(&json!({"NetPositionId": "POS-1"})),
                saxo_position_json(&json!({
                    "NetPositionId": "POS-2",
                    "NetPositionBase": {
                        "Uic": 101,
                        "AssetType": "CfdOnIndex",
                        "Amount": 10.0,
                        "AverageOpenPrice": 4500.0,
                        "MarketValue": 45100.0,
                        "ProfitLossOnTrade": 100.0,
                        "CurrentPrice": 4510.0
                    }
                }))
            ]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].symbol, "EURUSD:FxSpot");
    assert_eq!(positions[1].symbol, "US500:CfdOnIndex");
}

#[tokio::test]
async fn get_positions_long() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/netpositions/me",
        200,
        json!({ "Data": [saxo_position_json(&json!({}))] }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[0].quantity, dec!(100000));
}

#[tokio::test]
async fn get_positions_short() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/netpositions/me",
        200,
        json!({
            "Data": [saxo_position_json(&json!({
                "NetPositionBase": {
                    "Uic": 21,
                    "AssetType": "FxSpot",
                    "Amount": -50000.0,
                    "AverageOpenPrice": 1.1,
                    "MarketValue": 0.0,
                    "ProfitLossOnTrade": -200.0,
                    "CurrentPrice": 1.104
                }
            }))]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions[0].side, PositionSide::Short);
    assert_eq!(positions[0].quantity, dec!(50000));
}

#[tokio::test]
async fn get_positions_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/netpositions/me",
        200,
        json!({ "Data": [] }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

#[tokio::test]
async fn get_position_by_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/netpositions/me",
        200,
        json!({
            "Data": [
                saxo_position_json(&json!({"NetPositionId": "POS-1"})),
                saxo_position_json(&json!({"NetPositionId": "POS-2"}))
            ]
        }),
    )
    .await;

    let position = adapter.get_position("POS-2").await.unwrap();
    assert_eq!(position.id, "POS-2");
}

#[tokio::test]
async fn get_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/netpositions/me",
        200,
        json!({ "Data": [] }),
    )
    .await;

    let err = adapter.get_position("NONEXISTENT").await.unwrap_err();
    assert!(matches!(err, GatewayError::PositionNotFound { .. }));
}

#[tokio::test]
async fn close_position_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // GET positions to find the one to close
    mount_json(
        &server,
        "GET",
        "/port/v1/netpositions/me",
        200,
        json!({
            "Data": [saxo_position_json(&json!({"NetPositionId": "POS-1"}))]
        }),
    )
    .await;

    // POST market close order
    mount_json(
        &server,
        "POST",
        "/trade/v2/orders",
        200,
        saxo_order_response_json("CLOSE-1"),
    )
    .await;

    let handle = adapter
        .close_position("POS-1", &close_request())
        .await
        .unwrap();
    assert_eq!(handle.id, "CLOSE-1");
    assert_eq!(handle.status, OrderStatus::Filled);
}
