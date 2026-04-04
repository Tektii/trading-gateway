mod helpers;

use helpers::{binance_margin_account_json, binance_margin_order_json, test_margin_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{ClosePositionRequest, OrderStatus, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_positions_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/account",
        200,
        binance_margin_account_json(),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    // USDT filtered as quote currency — only BTC remains
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].symbol, "BTCUSDT");
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[0].quantity, dec!(2.0));
}

#[tokio::test]
async fn get_positions_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/account",
        200,
        json!({
            "borrowEnabled": true,
            "marginLevel": "999.00000000",
            "totalAssetOfBtc": "0.00000000",
            "totalLiabilityOfBtc": "0.00000000",
            "totalNetAssetOfBtc": "0.00000000",
            "tradeEnabled": true,
            "transferEnabled": true,
            "userAssets": [
                {
                    "asset": "USDT",
                    "borrowed": "0.00",
                    "free": "100.00",
                    "interest": "0.00",
                    "locked": "0.00",
                    "netAsset": "100.00"
                }
            ]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

#[tokio::test]
async fn get_position_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/account",
        200,
        binance_margin_account_json(),
    )
    .await;

    let position = adapter.get_position("BTCUSDT").await.unwrap();
    assert_eq!(position.symbol, "BTCUSDT");
    assert_eq!(position.side, PositionSide::Long);
    assert_eq!(position.quantity, dec!(2.0));
}

#[tokio::test]
async fn close_position_submits_sell() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    // close_position calls get_position (which calls get_positions via margin account)
    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/account",
        200,
        binance_margin_account_json(),
    )
    .await;

    // Then submits a sell order
    mount_json(
        &server,
        "POST",
        "/sapi/v1/margin/order",
        200,
        binance_margin_order_json(&json!({"side": "SELL", "type": "MARKET"})),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };

    let handle = adapter.close_position("BTCUSDT", &request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Open);
}
