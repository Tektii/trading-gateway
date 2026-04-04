mod helpers;

use helpers::{oanda_account_json, test_adapter};
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{OrderType, PositionMode};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

// =========================================================================
// Capabilities
// =========================================================================

#[tokio::test]
async fn capabilities_features_netting_account() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Mount account summary with hedgingEnabled: false
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        200,
        oanda_account_json(),
    )
    .await;

    let caps = adapter.get_capabilities().await.unwrap();

    // Supported order types
    assert!(caps.supported_order_types.contains(&OrderType::Market));
    assert!(caps.supported_order_types.contains(&OrderType::Limit));
    assert!(caps.supported_order_types.contains(&OrderType::Stop));
    assert!(caps.supported_order_types.contains(&OrderType::StopLimit));
    // TrailingStop is NOT supported as a standalone entry order
    assert!(
        !caps
            .supported_order_types
            .contains(&OrderType::TrailingStop)
    );

    // Features
    assert!(caps.features.contains(&"bracket_orders".to_string()));
    assert!(caps.features.contains(&"trailing_stop".to_string()));
    assert!(caps.features.contains(&"get_quote".to_string()));
    assert!(caps.features.contains(&"short_selling".to_string()));

    // Position mode detected from account API
    assert_eq!(caps.position_mode, PositionMode::Netting);
}

#[tokio::test]
async fn capabilities_hedging_account() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Mount account summary with hedgingEnabled: true
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        200,
        json!({
            "account": {
                "balance": "100000.00",
                "NAV": "100500.00",
                "marginUsed": "5000.00",
                "marginAvailable": "95000.00",
                "unrealizedPL": "500.00",
                "hedgingEnabled": true,
                "currency": "USD"
            }
        }),
    )
    .await;

    let caps = adapter.get_capabilities().await.unwrap();
    assert_eq!(caps.position_mode, PositionMode::Hedging);
}

#[tokio::test]
async fn capabilities_fallback_on_api_failure() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Account summary fails — should fall back to Netting
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        500,
        json!({"errorMessage": "Internal error"}),
    )
    .await;

    let caps = adapter.get_capabilities().await.unwrap();
    assert_eq!(caps.position_mode, PositionMode::Netting);
}

// =========================================================================
// Connection Status
// =========================================================================

#[tokio::test]
async fn connection_status_connected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        200,
        oanda_account_json(),
    )
    .await;

    let status = adapter.get_connection_status().await.unwrap();
    assert!(status.connected);
}

#[tokio::test]
async fn connection_status_disconnected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        500,
        json!({"errorMessage": "Internal error"}),
    )
    .await;

    // Connection check returns Ok with connected=false, not Err
    let status = adapter.get_connection_status().await.unwrap();
    assert!(!status.connected);
}
