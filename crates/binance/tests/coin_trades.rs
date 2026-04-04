mod helpers;

use helpers::{binance_coin_trade_json, test_coin_futures_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{Side, TradeQueryParams};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_trades_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/dapi/v1/userTrades",
        200,
        json!([binance_coin_trade_json("BTCUSD_PERP")]),
    )
    .await;

    let params = TradeQueryParams {
        symbol: Some("BTCUSD_PERP".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].symbol, "BTCUSD_PERP");
    assert_eq!(trades[0].side, Side::Buy);
    assert_eq!(trades[0].price, dec!(42000));
    assert_eq!(trades[0].order_id, "345678");
}

#[tokio::test]
async fn get_trades_with_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/dapi/v1/userTrades",
        200,
        json!([binance_coin_trade_json("ETHUSD_PERP")]),
    )
    .await;

    let params = TradeQueryParams {
        symbol: Some("ETHUSD_PERP".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].symbol, "ETHUSD_PERP");
}

#[tokio::test]
async fn get_trades_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(&server, "GET", "/dapi/v1/userTrades", 200, json!([])).await;

    let params = TradeQueryParams {
        symbol: Some("BTCUSD_PERP".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert!(trades.is_empty());
}
