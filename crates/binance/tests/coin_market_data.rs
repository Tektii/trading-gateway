mod helpers;

use helpers::{binance_coin_quote_json, binance_futures_kline_json, test_coin_futures_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{BarParams, Timeframe};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_quote_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/dapi/v1/ticker/bookTicker",
        200,
        binance_coin_quote_json(),
    )
    .await;

    let quote = adapter.get_quote("BTCUSD_PERP").await.unwrap();
    assert_eq!(quote.symbol, "BTCUSD_PERP");
    assert_eq!(quote.bid, dec!(42000));
    assert_eq!(quote.ask, dec!(42100));
    assert!(quote.bid_size.is_some());
    assert!(quote.ask_size.is_some());
}

#[tokio::test]
async fn get_quote_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    // The quote endpoint does not use execute_with_retry, so a 500 with a
    // non-parseable body results in an Internal error (parse failure).
    mount_json(
        &server,
        "GET",
        "/dapi/v1/ticker/bookTicker",
        500,
        json!({"code": -1000, "msg": "Internal error"}),
    )
    .await;

    let err = adapter.get_quote("BTCUSD_PERP").await.unwrap_err();
    assert!(
        matches!(
            err,
            GatewayError::ProviderError { .. } | GatewayError::Internal { .. }
        ),
        "Expected ProviderError or Internal, got: {err:?}"
    );
}

#[tokio::test]
async fn get_bars_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/dapi/v1/klines",
        200,
        binance_futures_kline_json(),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneHour,
        limit: Some(100),
        start: None,
        end: None,
    };
    let bars = adapter.get_bars("BTCUSD_PERP", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].symbol, "BTCUSD_PERP");
    assert_eq!(bars[0].open, dec!(42000));
    assert_eq!(bars[0].high, dec!(42500));
    assert_eq!(bars[0].low, dec!(41800));
    assert_eq!(bars[0].close, dec!(42200));
}

#[tokio::test]
async fn get_bars_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_coin_futures_adapter(&base_url);

    mount_json(&server, "GET", "/dapi/v1/klines", 200, json!([])).await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: Some(10),
        start: None,
        end: None,
    };
    let bars = adapter.get_bars("BTCUSD_PERP", &params).await.unwrap();
    assert!(bars.is_empty());
}
