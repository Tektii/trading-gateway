mod helpers;

use helpers::{saxo_chart_json, saxo_quote_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{BarParams, Timeframe};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_quote_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/trade/v1/infoprices",
        200,
        saxo_quote_json(),
    )
    .await;

    let quote = adapter.get_quote("EURUSD:FxSpot").await.unwrap();
    assert_eq!(quote.symbol, "EURUSD:FxSpot");
    assert_eq!(quote.provider, "saxo");
    assert_eq!(quote.bid, dec!(1.099));
    assert_eq!(quote.ask, dec!(1.101));
    assert_eq!(quote.last, dec!(1.1)); // mid price
}

#[tokio::test]
async fn get_quote_unknown_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // No HTTP call needed — symbol resolution fails before request
    let err = adapter.get_quote("UNKNOWN:FxSpot").await.unwrap_err();
    assert!(matches!(err, GatewayError::SymbolNotFound { .. }));
}

#[tokio::test]
async fn get_quote_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/trade/v1/infoprices",
        500,
        json!({"Message": "Server Error"}),
    )
    .await;

    let err = adapter.get_quote("EURUSD:FxSpot").await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}

#[tokio::test]
async fn get_bars_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(&server, "GET", "/chart/v3/charts", 200, saxo_chart_json()).await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        start: None,
        end: None,
        limit: None,
    };
    let bars = adapter.get_bars("EURUSD:FxSpot", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].symbol, "EURUSD:FxSpot");
    assert_eq!(bars[0].provider, "saxo");
    assert_eq!(bars[0].open, dec!(1.1));
    assert_eq!(bars[0].high, dec!(1.105));
    assert_eq!(bars[0].low, dec!(1.095));
    assert_eq!(bars[0].close, dec!(1.102));
    assert_eq!(bars[0].timeframe, Timeframe::OneMinute);
}

#[tokio::test]
async fn get_bars_with_count() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(&server, "GET", "/chart/v3/charts", 200, saxo_chart_json()).await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        start: None,
        end: None,
        limit: Some(50),
    };
    let bars = adapter.get_bars("EURUSD:FxSpot", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
}

#[tokio::test]
async fn get_bars_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/chart/v3/charts",
        200,
        json!({ "Data": [] }),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        start: None,
        end: None,
        limit: None,
    };
    let bars = adapter.get_bars("EURUSD:FxSpot", &params).await.unwrap();
    assert!(bars.is_empty());
}

#[tokio::test]
async fn get_bars_different_timeframe() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(&server, "GET", "/chart/v3/charts", 200, saxo_chart_json()).await;

    let params = BarParams {
        timeframe: Timeframe::OneHour,
        start: None,
        end: None,
        limit: None,
    };
    let bars = adapter.get_bars("EURUSD:FxSpot", &params).await.unwrap();
    assert_eq!(bars[0].timeframe, Timeframe::OneHour);
}
