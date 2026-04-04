mod helpers;

use helpers::test_margin_adapter;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::PositionMode;
use tektii_gateway_test_support::wiremock_helpers::{mount_empty, start_mock_server};

#[tokio::test]
async fn capabilities_returns_margin_features() {
    let (_, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    let caps = adapter.get_capabilities().await.unwrap();
    assert_eq!(caps.position_mode, PositionMode::Netting);
    assert!(caps.supported_order_types.len() >= 3);
    assert!(caps.features.iter().any(|f| f == "cross_margin"));
    assert!(caps.features.iter().any(|f| f == "isolated_margin"));
    assert!(caps.features.iter().any(|f| f == "short_selling"));
}

#[tokio::test]
async fn connection_status() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_empty(&server, "GET", "/api/v3/ping", 200).await;

    let status = adapter.get_connection_status().await.unwrap();
    assert!(status.connected);
}
