//! Integration tests for adapter construction, capabilities, and platform identity.
//!
//! Verifies that each adapter can be constructed with valid credentials
//! and returns correct platform identity and capabilities.

use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{OrderType, PositionMode, TradingPlatform};
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_test_support::wiremock_helpers::start_mock_server;
use tokio::sync::broadcast;

// =========================================================================
// Alpaca
// =========================================================================

fn alpaca_adapter(platform: TradingPlatform) -> impl TradingAdapter {
    let creds = tektii_gateway_alpaca::AlpacaCredentials::new("test-key", "test-secret");
    let (tx, _) = broadcast::channel::<WsMessage>(1);
    tektii_gateway_alpaca::AlpacaAdapter::new(&creds, tx, platform)
        .expect("Failed to build HTTP client")
}

#[test]
fn alpaca_live_has_correct_platform() {
    let adapter = alpaca_adapter(TradingPlatform::AlpacaLive);
    assert_eq!(adapter.platform(), TradingPlatform::AlpacaLive);
}

#[test]
fn alpaca_paper_has_correct_platform() {
    let adapter = alpaca_adapter(TradingPlatform::AlpacaPaper);
    assert_eq!(adapter.platform(), TradingPlatform::AlpacaPaper);
}

#[test]
fn alpaca_capabilities_include_brackets_and_trailing_stop() {
    let adapter = alpaca_adapter(TradingPlatform::AlpacaPaper);
    let caps = adapter.capabilities().capabilities();

    assert!(
        caps.supported_order_types
            .contains(&OrderType::TrailingStop)
    );
    assert!(caps.supports_feature("bracket_orders"));
    assert!(caps.supports_feature("trailing_stop"));
    assert_eq!(caps.position_mode, PositionMode::Netting);
}

// =========================================================================
// Binance
// =========================================================================

#[test]
fn binance_spot_has_correct_platform_and_capabilities() {
    let creds = tektii_gateway_binance::BinanceCredentials::new("test-key", "test-secret");
    let (tx, _) = broadcast::channel::<WsMessage>(1);
    let adapter = tektii_gateway_binance::BinanceSpotAdapter::new(
        &creds,
        tx,
        TradingPlatform::BinanceSpotLive,
    )
    .expect("Failed to build HTTP client");

    assert_eq!(adapter.platform(), TradingPlatform::BinanceSpotLive);

    let caps = adapter.capabilities().capabilities();
    assert!(caps.supported_order_types.contains(&OrderType::Market));
    assert!(caps.supported_order_types.contains(&OrderType::Limit));
    assert_eq!(caps.position_mode, PositionMode::Netting);
}

#[test]
fn binance_futures_has_correct_platform_and_hedging() {
    let creds = tektii_gateway_binance::BinanceCredentials::new("test-key", "test-secret");
    let (tx, _) = broadcast::channel::<WsMessage>(1);
    let adapter = tektii_gateway_binance::BinanceFuturesAdapter::new(
        &creds,
        tx,
        TradingPlatform::BinanceFuturesLive,
    )
    .expect("Failed to build HTTP client");

    assert_eq!(adapter.platform(), TradingPlatform::BinanceFuturesLive);

    let caps = adapter.capabilities().capabilities();
    assert_eq!(caps.position_mode, PositionMode::Hedging);
}

// =========================================================================
// Oanda
// =========================================================================

#[test]
fn oanda_has_correct_platform() {
    let creds = tektii_gateway_oanda::OandaCredentials::new("test-token", "test-account");
    let (tx, _) = broadcast::channel::<WsMessage>(1);
    let adapter =
        tektii_gateway_oanda::OandaAdapter::new(&creds, tx, TradingPlatform::OandaPractice)
            .expect("Failed to build HTTP client");

    assert_eq!(adapter.platform(), TradingPlatform::OandaPractice);
}

#[test]
fn oanda_capabilities_include_expected_order_types() {
    let creds = tektii_gateway_oanda::OandaCredentials::new("test-token", "test-account");
    let (tx, _) = broadcast::channel::<WsMessage>(1);
    let adapter =
        tektii_gateway_oanda::OandaAdapter::new(&creds, tx, TradingPlatform::OandaPractice)
            .expect("Failed to build HTTP client");

    let caps = adapter.capabilities().capabilities();
    assert!(caps.supported_order_types.contains(&OrderType::Market));
    assert!(caps.supported_order_types.contains(&OrderType::Limit));
    assert!(caps.supported_order_types.contains(&OrderType::Stop));
    assert_eq!(caps.position_mode, PositionMode::Netting);
}

// =========================================================================
// Saxo (async constructor — needs wiremock for startup API calls)
// =========================================================================

#[tokio::test]
async fn saxo_has_correct_platform_and_capabilities() {
    let (server, base_url) = start_mock_server().await;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let instruments = serde_json::json!({
        "Data": [
            {"Symbol": "EURUSD", "Identifier": 21, "AssetType": "FxSpot", "Description": "EUR/USD"},
        ],
        "__count": 1
    });
    Mock::given(method("GET"))
        .and(path("/ref/v1/instruments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&instruments))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/port/v1/clients/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ClientKey": "test-client-key",
            "AccountValueProtectionLimit": 0,
            "ClientId": "test-client"
        })))
        .mount(&server)
        .await;

    let creds = tektii_gateway_saxo::SaxoCredentials {
        client_id: "test-client-id".into(),
        client_secret: secrecy::SecretBox::new(Box::new("test-secret".to_string())),
        account_key: "test-account-key".into(),
        access_token: Some(secrecy::SecretBox::new(Box::new("test-token".to_string()))),
        refresh_token: None,
        rest_url: Some(base_url),
        auth_url: None,
        ws_url: None,
        precheck_orders: Some(false),
    };
    let (tx, _) = broadcast::channel::<WsMessage>(1);
    let adapter = tektii_gateway_saxo::SaxoAdapter::new(&creds, tx, TradingPlatform::SaxoLive)
        .await
        .expect("Saxo adapter construction should succeed");

    assert_eq!(adapter.platform(), TradingPlatform::SaxoLive);

    let caps = adapter.capabilities().capabilities();
    assert!(
        caps.supported_order_types
            .contains(&OrderType::TrailingStop)
    );
    assert!(caps.supports_feature("trailing_stop"));
}
