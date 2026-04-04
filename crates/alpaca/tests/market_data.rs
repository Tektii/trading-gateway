mod helpers;

use helpers::{
    alpaca_bars_json, alpaca_crypto_bars_json, alpaca_crypto_quote_json, alpaca_quote_json,
    test_adapter,
};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{BarParams, Timeframe};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

// =========================================================================
// Quotes
// =========================================================================

#[tokio::test]
async fn get_quote_stock() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/stocks/AAPL/quotes/latest",
        200,
        alpaca_quote_json(),
    )
    .await;

    let quote = adapter.get_quote("AAPL").await.unwrap();

    assert_eq!(quote.symbol, "AAPL");
    assert_eq!(quote.provider, "alpaca");
    assert_eq!(quote.bid, dec!(150));
    assert_eq!(quote.ask, dec!(150.5));
    // Last = (bid + ask) / 2
    assert_eq!(quote.last, dec!(150.25));
    assert_eq!(quote.bid_size, Some(dec!(100)));
    assert_eq!(quote.ask_size, Some(dec!(200)));
}

#[tokio::test]
async fn get_quote_crypto() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v1beta3/crypto/us/latest/quotes",
        200,
        alpaca_crypto_quote_json("BTC/USD"),
    )
    .await;

    let quote = adapter.get_quote("BTCUSD").await.unwrap();

    assert_eq!(quote.symbol, "BTCUSD");
    assert_eq!(quote.bid, dec!(50000));
    assert_eq!(quote.ask, dec!(50100));
}

#[tokio::test]
async fn get_quote_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/stocks/AAPL/quotes/latest",
        500,
        json!({"message": "Internal error"}),
    )
    .await;

    let err = adapter.get_quote("AAPL").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::ProviderError { .. }),
        "Expected ProviderError, got: {err:?}"
    );
}

// =========================================================================
// Bars
// =========================================================================

#[tokio::test]
async fn get_bars_stock() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/stocks/AAPL/bars",
        200,
        alpaca_bars_json(),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        start: None,
        end: None,
        limit: None,
    };

    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].symbol, "AAPL");
    assert_eq!(bars[0].provider, "alpaca");
    assert_eq!(bars[0].open, dec!(150));
    assert_eq!(bars[0].high, dec!(152));
    assert_eq!(bars[0].low, dec!(149.5));
    assert_eq!(bars[0].close, dec!(151));
    assert_eq!(bars[0].volume, dec!(10000));
    assert_eq!(bars[0].timeframe, Timeframe::OneMinute);
}

#[tokio::test]
async fn get_bars_crypto() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v1beta3/crypto/us/bars",
        200,
        alpaca_crypto_bars_json("BTC/USD"),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        start: None,
        end: None,
        limit: None,
    };

    let bars = adapter.get_bars("BTCUSD", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].symbol, "BTCUSD");
    assert_eq!(bars[0].open, dec!(50000));
}

#[tokio::test]
async fn get_bars_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/stocks/AAPL/bars",
        200,
        json!({"bars": []}),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        start: None,
        end: None,
        limit: None,
    };

    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    assert!(bars.is_empty());
}

#[tokio::test]
async fn get_bars_timeframe_translation() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Mount for all three requests (same path, different query params)
    mount_json(
        &server,
        "GET",
        "/v2/stocks/AAPL/bars",
        200,
        alpaca_bars_json(),
    )
    .await;

    // Verify 1h timeframe works
    let params = BarParams {
        timeframe: Timeframe::OneHour,
        start: None,
        end: None,
        limit: None,
    };
    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].timeframe, Timeframe::OneHour);

    // Verify 1d timeframe works
    let params = BarParams {
        timeframe: Timeframe::OneDay,
        start: None,
        end: None,
        limit: None,
    };
    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].timeframe, Timeframe::OneDay);
}
