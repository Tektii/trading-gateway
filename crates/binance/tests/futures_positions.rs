mod helpers;

use helpers::{binance_futures_order_json, binance_futures_position_json, test_futures_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{ClosePositionRequest, OrderStatus, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_positions_long() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/positionRisk",
        200,
        json!([binance_futures_position_json("BTCUSDT")]),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].symbol, "BTCUSDT");
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[0].quantity, dec!(1));
}

#[tokio::test]
async fn get_positions_short() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/positionRisk",
        200,
        json!([{
            "symbol": "ETHUSDT",
            "positionAmt": "-2.000",
            "entryPrice": "3000.00",
            "markPrice": "2950.00",
            "unRealizedProfit": "100.00",
            "liquidationPrice": "3500.00",
            "leverage": "5",
            "positionSide": "SHORT",
            "marginType": "cross",
            "isolatedMargin": "0.00",
            "notional": "-5900.00",
            "updateTime": 1_704_067_200_000_i64
        }]),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].symbol, "ETHUSDT");
    assert_eq!(positions[0].side, PositionSide::Short);
    assert_eq!(positions[0].quantity, dec!(2));
}

#[tokio::test]
async fn get_positions_filters_zero() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/positionRisk",
        200,
        json!([
            {
                "symbol": "BTCUSDT",
                "positionAmt": "0.000",
                "entryPrice": "0.0",
                "markPrice": "42500.00",
                "unRealizedProfit": "0.00",
                "liquidationPrice": "0.00",
                "leverage": "10",
                "positionSide": "BOTH",
                "marginType": "cross",
                "isolatedMargin": "0.00",
                "notional": "0.00",
                "updateTime": 1_704_067_200_000_i64
            }
        ]),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

#[tokio::test]
async fn get_positions_with_leverage_and_margin() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/positionRisk",
        200,
        json!([{
            "symbol": "BTCUSDT",
            "positionAmt": "1.000",
            "entryPrice": "42000.00",
            "markPrice": "42500.00",
            "unRealizedProfit": "500.00",
            "liquidationPrice": "35000.00",
            "leverage": "10",
            "positionSide": "LONG",
            "marginType": "isolated",
            "isolatedMargin": "4200.00",
            "notional": "42500.00",
            "updateTime": 1_704_067_200_000_i64
        }]),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].leverage, Some(dec!(10)));
    assert_eq!(positions[0].unrealized_pnl, dec!(500));
}

#[tokio::test]
async fn get_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(&server, "GET", "/fapi/v2/positionRisk", 200, json!([])).await;

    let err = adapter.get_position("XYZUSDT").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::PositionNotFound { .. }),
        "Expected PositionNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn close_position_submits_sell() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    // close_position calls get_position first (via positionRisk)
    mount_json(
        &server,
        "GET",
        "/fapi/v2/positionRisk",
        200,
        json!([binance_futures_position_json("BTCUSDT")]),
    )
    .await;

    // Then submits a sell order to close
    mount_json(
        &server,
        "POST",
        "/fapi/v1/order",
        200,
        binance_futures_order_json(&json!({"side": "SELL", "reduceOnly": true})),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: false,
    };

    let handle = adapter.close_position("BTCUSDT", &request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Open);
}
