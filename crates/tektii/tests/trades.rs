mod helpers;

use helpers::{engine_trade_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{Side, TradeQueryParams};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_trades_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/trades",
        200,
        json!({
            "trades": [engine_trade_json(&json!({}))]
        }),
    )
    .await;

    let params = TradeQueryParams::default();
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].id, "trade-001");
    assert_eq!(trades[0].order_id, "order-abc-123");
    assert_eq!(trades[0].symbol, "AAPL");
    assert_eq!(trades[0].side, Side::Buy);
    assert_eq!(trades[0].quantity, dec!(10));
    assert_eq!(trades[0].price, dec!(150.50));
    assert_eq!(trades[0].commission, dec!(1.50));
    assert_eq!(trades[0].commission_currency, "USD");
    assert_eq!(trades[0].is_maker, None);
}

#[tokio::test]
async fn get_trades_with_symbol_filter() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/trades",
        200,
        json!({
            "trades": [engine_trade_json(&json!({"symbol": "AAPL"}))]
        }),
    )
    .await;

    let params = TradeQueryParams {
        symbol: Some("AAPL".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
}

#[tokio::test]
async fn get_trades_with_order_id_filter() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/trades",
        200,
        json!({
            "trades": [engine_trade_json(&json!({}))]
        }),
    )
    .await;

    let params = TradeQueryParams {
        order_id: Some("order-abc-123".to_string()),
        ..Default::default()
    };
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
}

#[tokio::test]
async fn get_trades_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/api/v1/trades", 200, json!({"trades": []})).await;

    let params = TradeQueryParams::default();
    let trades = adapter.get_trades(&params).await.unwrap();
    assert!(trades.is_empty());
}
