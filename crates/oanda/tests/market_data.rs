mod helpers;

use helpers::{oanda_candles_json, oanda_pricing_json, test_adapter};
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
async fn get_quote_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/pricing",
        200,
        oanda_pricing_json("EUR_USD"),
    )
    .await;

    let quote = adapter.get_quote("EUR_USD").await.unwrap();
    assert_eq!(quote.symbol, "EUR_USD");
    assert_eq!(quote.provider, "oanda");
    assert_eq!(quote.bid, dec!(1.09900));
    assert_eq!(quote.ask, dec!(1.10100));
    // Mid-price as "last": (1.09900 + 1.10100) / 2 = 1.10000
    assert_eq!(quote.last, dec!(1.10000));
    assert_eq!(quote.bid_size, Some(dec!(1000000)));
    assert_eq!(quote.ask_size, Some(dec!(2000000)));
}

#[tokio::test]
async fn get_quote_symbol_passthrough() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/pricing",
        200,
        oanda_pricing_json("GBP_JPY"),
    )
    .await;

    let quote = adapter.get_quote("GBP_JPY").await.unwrap();
    assert_eq!(quote.symbol, "GBP_JPY");
}

#[tokio::test]
async fn get_quote_empty_prices() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/pricing",
        200,
        json!({"prices": []}),
    )
    .await;

    let err = adapter.get_quote("EUR_USD").await.unwrap_err();
    assert!(matches!(err, GatewayError::SymbolNotFound { .. }));
}

#[tokio::test]
async fn get_quote_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/pricing",
        500,
        json!({"errorMessage": "Internal error"}),
    )
    .await;

    let err = adapter.get_quote("EUR_USD").await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}

// =========================================================================
// Bars (candles)
// =========================================================================

#[tokio::test]
async fn get_bars_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Bars use instrument_url: /v3/instruments/{instrument}/candles
    mount_json(
        &server,
        "GET",
        "/v3/instruments/EUR_USD/candles",
        200,
        oanda_candles_json(),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: None,
        start: None,
        end: None,
    };

    let bars = adapter.get_bars("EUR_USD", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].symbol, "EUR_USD");
    assert_eq!(bars[0].provider, "oanda");
    assert_eq!(bars[0].timeframe, Timeframe::OneMinute);
    assert_eq!(bars[0].open, dec!(1.10000));
    assert_eq!(bars[0].high, dec!(1.10500));
    assert_eq!(bars[0].low, dec!(1.09500));
    assert_eq!(bars[0].close, dec!(1.10200));
    assert_eq!(bars[0].volume, dec!(1000));
}

#[tokio::test]
async fn get_bars_with_params() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/instruments/EUR_USD/candles",
        200,
        oanda_candles_json(),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneHour,
        limit: Some(100),
        start: None,
        end: None,
    };

    let bars = adapter.get_bars("EUR_USD", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].timeframe, Timeframe::OneHour);
}

#[tokio::test]
async fn get_bars_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/instruments/EUR_USD/candles",
        200,
        json!({"candles": []}),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: None,
        start: None,
        end: None,
    };

    let bars = adapter.get_bars("EUR_USD", &params).await.unwrap();
    assert!(bars.is_empty());
}

#[tokio::test]
async fn get_bars_no_mid_data() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Candle without mid field → filtered out
    mount_json(
        &server,
        "GET",
        "/v3/instruments/EUR_USD/candles",
        200,
        json!({
            "candles": [{
                "time": "2024-01-15T10:00:00.000000000Z",
                "volume": 1000,
                "complete": true
            }]
        }),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: None,
        start: None,
        end: None,
    };

    let bars = adapter.get_bars("EUR_USD", &params).await.unwrap();
    assert!(bars.is_empty());
}
