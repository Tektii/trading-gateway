mod helpers;

use helpers::test_adapter;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{OrderType, PositionMode};
use tektii_gateway_test_support::wiremock_helpers::{mount_empty, start_mock_server};

#[tokio::test]
async fn capabilities_features_and_order_types() {
    let adapter = test_adapter("http://unused:9999");

    let capabilities = adapter.get_capabilities().await.unwrap();
    assert!(
        capabilities
            .supported_order_types
            .contains(&OrderType::Market)
    );
    assert!(
        capabilities
            .supported_order_types
            .contains(&OrderType::Limit)
    );
    assert!(
        capabilities
            .supported_order_types
            .contains(&OrderType::Stop)
    );
    assert_eq!(capabilities.position_mode, PositionMode::Hedging);
    assert!(capabilities.features.contains(&"hedging".to_string()));
    assert!(capabilities.features.contains(&"reduce_only".to_string()));
    assert!(capabilities.features.contains(&"short_selling".to_string()));

    // Bracket/OCO/OTO not supported natively
    let caps = adapter.capabilities();
    assert!(!caps.supports_bracket_orders("AAPL"));
    assert!(!caps.supports_oco_orders("AAPL"));
    assert!(!caps.supports_oto_orders("AAPL"));
}

#[tokio::test]
async fn connection_status_connected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_empty(&server, "GET", "/health", 200).await;

    let status = adapter.get_connection_status().await.unwrap();
    assert!(status.connected);
}

#[tokio::test]
async fn connection_status_disconnected() {
    // Point at a port with nothing listening
    let adapter = test_adapter("http://127.0.0.1:1");

    let status = adapter.get_connection_status().await.unwrap();
    assert!(!status.connected);
    assert_eq!(status.latency_ms, 0);
}
