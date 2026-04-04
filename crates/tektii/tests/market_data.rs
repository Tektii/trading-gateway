mod helpers;

use helpers::{engine_bar_json, engine_quote_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::BarParams;
use tektii_gateway_core::models::Timeframe;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_quote_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/quote",
        200,
        engine_quote_json(&json!({})),
    )
    .await;

    let quote = adapter.get_quote("AAPL").await.unwrap();
    assert_eq!(quote.symbol, "AAPL");
    assert_eq!(quote.bid, dec!(150.00));
    assert_eq!(quote.ask, dec!(150.50));
    // last = bid (Tektii approximation)
    assert_eq!(quote.last, quote.bid);
    assert_eq!(quote.bid_size, None);
    assert_eq!(quote.ask_size, None);
    assert_eq!(quote.provider, "tektii");
}

#[tokio::test]
async fn get_quote_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/quote",
        404,
        json!({"error": "not found"}),
    )
    .await;

    let err = adapter.get_quote("UNKNOWN").await.unwrap_err();
    assert!(matches!(err, GatewayError::SymbolNotFound { .. }));
}

#[tokio::test]
async fn get_quote_invalid_timestamp() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // i64::MAX milliseconds overflows chrono's DateTime range → error
    mount_json(
        &server,
        "GET",
        "/api/v1/quote",
        200,
        engine_quote_json(&json!({"timestamp": i64::MAX as u64})),
    )
    .await;

    let err = adapter.get_quote("AAPL").await.unwrap_err();
    assert!(matches!(err, GatewayError::Internal { .. }));
}

#[tokio::test]
async fn get_bars_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/bars",
        200,
        json!({
            "bars": [
                engine_bar_json(&json!({})),
                engine_bar_json(&json!({"timestamp": 1_704_067_260_000u64, "close": "152.00"})),
            ]
        }),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: None,
        start: None,
        end: None,
    };
    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].open, dec!(150.00));
    assert_eq!(bars[0].high, dec!(152.00));
    assert_eq!(bars[0].low, dec!(149.50));
    assert_eq!(bars[0].close, dec!(151.00));
    assert_eq!(bars[0].symbol, "AAPL");
    assert_eq!(bars[0].provider, "tektii");
    assert_eq!(bars[0].timeframe, Timeframe::OneMinute);
}

#[tokio::test]
async fn get_bars_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/bars",
        404,
        json!({"error": "not found"}),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: None,
        start: None,
        end: None,
    };
    let err = adapter.get_bars("UNKNOWN", &params).await.unwrap_err();
    assert!(matches!(err, GatewayError::SymbolNotFound { .. }));
}

#[tokio::test]
async fn get_bars_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/api/v1/bars", 200, json!({"bars": []})).await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: None,
        start: None,
        end: None,
    };
    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    assert!(bars.is_empty());
}

#[tokio::test]
async fn get_bars_custom_count() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/bars",
        200,
        json!({"bars": [engine_bar_json(&json!({}))]}),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneHour,
        limit: Some(50),
        start: None,
        end: None,
    };
    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    assert_eq!(bars.len(), 1);
}

#[tokio::test]
async fn get_bars_volume_conversion() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/bars",
        200,
        json!({"bars": [engine_bar_json(&json!({"volume": 12345.67}))]}),
    )
    .await;

    let params = BarParams {
        timeframe: Timeframe::OneMinute,
        limit: None,
        start: None,
        end: None,
    };
    let bars = adapter.get_bars("AAPL", &params).await.unwrap();
    // f64 12345.67 → Decimal (via from_f64)
    assert_eq!(bars[0].volume, dec!(12345.67));
}
