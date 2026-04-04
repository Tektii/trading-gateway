mod helpers;

use helpers::{binance_coin_order_json, binance_coin_position_json, test_coin_futures_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{ClosePositionRequest, OrderStatus, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_positions_long() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/dapi/v1/positionRisk",
        200,
        json!([binance_coin_position_json("BTCUSD_PERP")]),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].symbol, "BTCUSD_PERP");
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[0].quantity, dec!(1));
    assert_eq!(positions[0].average_entry_price, dec!(42000));
}

#[tokio::test]
async fn get_positions_short() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    let mut pos = binance_coin_position_json("BTCUSD_PERP");
    pos["positionAmt"] = json!("-2");
    pos["positionSide"] = json!("SHORT");

    mount_json(&server, "GET", "/dapi/v1/positionRisk", 200, json!([pos])).await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].side, PositionSide::Short);
    assert_eq!(positions[0].quantity, dec!(2));
}

#[tokio::test]
async fn get_positions_filters_zero() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    let mut zero_pos = binance_coin_position_json("BTCUSD_PERP");
    zero_pos["positionAmt"] = json!("0");

    mount_json(
        &server,
        "GET",
        "/dapi/v1/positionRisk",
        200,
        json!([zero_pos]),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

#[tokio::test]
async fn get_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(&server, "GET", "/dapi/v1/positionRisk", 200, json!([])).await;

    let err = adapter.get_position("BTCUSD_PERP").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::PositionNotFound { .. }),
        "Expected PositionNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn close_position_submits_sell() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    // close_position calls get_position (which calls get_positions via positionRisk)
    mount_json(
        &server,
        "GET",
        "/dapi/v1/positionRisk",
        200,
        json!([binance_coin_position_json("BTCUSD_PERP")]),
    )
    .await;

    // Then submits a sell order via POST
    mount_json(
        &server,
        "POST",
        "/dapi/v1/order",
        200,
        binance_coin_order_json(&json!({"side": "SELL", "reduceOnly": true})),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: false,
    };

    let handle = adapter
        .close_position("BTCUSD_PERP", &request)
        .await
        .unwrap();
    assert_eq!(handle.status, OrderStatus::Open);
}
