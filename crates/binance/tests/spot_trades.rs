mod helpers;

use helpers::{binance_spot_trade_json, test_spot_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{Side, TradeQueryParams};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_trades_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/myTrades",
        200,
        json!([binance_spot_trade_json("BTCUSDT")]),
    )
    .await;

    let params = TradeQueryParams {
        symbol: Some("BTCUSDT".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].symbol, "BTCUSDT");
    assert_eq!(trades[0].side, Side::Buy);
    assert_eq!(trades[0].price, dec!(42000));
    assert_eq!(trades[0].order_id, "123456");
}

#[tokio::test]
async fn get_trades_with_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/myTrades",
        200,
        json!([binance_spot_trade_json("ETHUSDT")]),
    )
    .await;

    let params = TradeQueryParams {
        symbol: Some("ETHUSDT".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].symbol, "ETHUSDT");
}

#[tokio::test]
async fn get_trades_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(&server, "GET", "/api/v3/myTrades", 200, json!([])).await;

    let params = TradeQueryParams {
        symbol: Some("BTCUSDT".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert!(trades.is_empty());
}
