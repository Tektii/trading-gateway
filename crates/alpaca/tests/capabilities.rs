mod helpers;

use helpers::{alpaca_account_json, test_adapter};
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::OrderType;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_capabilities_returns_correct_features() {
    let (_server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let caps = adapter.get_capabilities().await.unwrap();

    // Check order types
    assert!(caps.supported_order_types.contains(&OrderType::Market));
    assert!(caps.supported_order_types.contains(&OrderType::Limit));
    assert!(caps.supported_order_types.contains(&OrderType::Stop));
    assert!(caps.supported_order_types.contains(&OrderType::StopLimit));
    assert!(
        caps.supported_order_types
            .contains(&OrderType::TrailingStop)
    );

    // Check features
    assert!(caps.features.contains(&"bracket_orders".to_string()));
    assert!(caps.features.contains(&"oco".to_string()));
    assert!(caps.features.contains(&"trailing_stop".to_string()));
    assert!(caps.features.contains(&"short_selling".to_string()));
    assert!(caps.features.contains(&"get_quote".to_string()));
}

#[tokio::test]
async fn get_connection_status_connected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/v2/account", 200, alpaca_account_json()).await;

    let status = adapter.get_connection_status().await.unwrap();
    assert!(status.connected);
    assert!(status.latency_ms < 5000);
}

#[tokio::test]
async fn get_connection_status_disconnected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        500,
        serde_json::json!({"message": "Internal error"}),
    )
    .await;

    let status = adapter.get_connection_status().await.unwrap();
    assert!(!status.connected);
}
