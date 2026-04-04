mod helpers;

use helpers::{binance_oco_response_json, test_spot_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{PlaceOcoOrderRequest, Side};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn place_oco_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order/oco",
        200,
        binance_oco_response_json(),
    )
    .await;

    let request = PlaceOcoOrderRequest {
        symbol: "BTCUSDT".to_string(),
        side: Side::Sell,
        quantity: dec!(1),
        take_profit_price: dec!(45000),
        stop_loss_trigger: dec!(40000),
        stop_loss_limit: Some(dec!(39900)),
    };

    let response = adapter.place_oco_order(&request).await.unwrap();
    assert_eq!(response.list_order_id, "999");
    assert_eq!(response.symbol, "BTCUSDT");
    assert_eq!(response.status, "active");
}

#[tokio::test]
async fn place_oco_order_returns_leg_ids() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order/oco",
        200,
        binance_oco_response_json(),
    )
    .await;

    let request = PlaceOcoOrderRequest {
        symbol: "BTCUSDT".to_string(),
        side: Side::Sell,
        quantity: dec!(1),
        take_profit_price: dec!(45000),
        stop_loss_trigger: dec!(40000),
        stop_loss_limit: None,
    };

    let response = adapter.place_oco_order(&request).await.unwrap();
    assert_eq!(response.stop_loss_order_id, "200001");
    assert_eq!(response.take_profit_order_id, "200002");
}

#[tokio::test]
async fn place_oco_order_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v3/order/oco",
        400,
        json!({"code": -2010, "msg": "Order would immediately trigger."}),
    )
    .await;

    let request = PlaceOcoOrderRequest {
        symbol: "BTCUSDT".to_string(),
        side: Side::Sell,
        quantity: dec!(1),
        take_profit_price: dec!(45000),
        stop_loss_trigger: dec!(40000),
        stop_loss_limit: None,
    };

    let err = adapter.place_oco_order(&request).await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderRejected { .. }),
        "Expected OrderRejected, got: {err:?}"
    );
}
