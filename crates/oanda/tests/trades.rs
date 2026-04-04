mod helpers;

use helpers::{oanda_trade_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{Side, TradeQueryParams};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

// =========================================================================
// Trades: Success
// =========================================================================

#[tokio::test]
async fn get_trades_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/trades",
        200,
        json!({
            "trades": [oanda_trade_json("EUR_USD", "10000")]
        }),
    )
    .await;

    let params = TradeQueryParams::default();
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].id, "1");
    assert_eq!(trades[0].symbol, "EUR_USD");
    assert_eq!(trades[0].side, Side::Buy);
    assert_eq!(trades[0].quantity, dec!(10000));
    assert_eq!(trades[0].price, dec!(1.10000));
    assert_eq!(trades[0].commission, dec!(0)); // Spread-based
}

#[tokio::test]
async fn get_trades_sell_side() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/trades",
        200,
        json!({
            "trades": [oanda_trade_json("EUR_USD", "-5000")]
        }),
    )
    .await;

    let params = TradeQueryParams::default();
    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades[0].side, Side::Sell);
    assert_eq!(trades[0].quantity, dec!(5000));
}

#[tokio::test]
async fn get_trades_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/trades",
        200,
        json!({"trades": []}),
    )
    .await;

    let params = TradeQueryParams::default();
    let trades = adapter.get_trades(&params).await.unwrap();
    assert!(trades.is_empty());
}

#[tokio::test]
async fn get_trades_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/trades",
        500,
        json!({"errorMessage": "Internal error"}),
    )
    .await;

    let params = TradeQueryParams::default();
    let err = adapter.get_trades(&params).await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}
