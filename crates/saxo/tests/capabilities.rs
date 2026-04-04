mod helpers;

use helpers::{saxo_balance_json, test_adapter};
use rust_decimal_macros::dec;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{OrderType, PositionMode};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn capabilities_features() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    let caps = adapter.get_capabilities().await.unwrap();

    assert!(caps.supported_order_types.contains(&OrderType::Market));
    assert!(caps.supported_order_types.contains(&OrderType::Limit));
    assert!(caps.supported_order_types.contains(&OrderType::Stop));
    assert!(caps.supported_order_types.contains(&OrderType::StopLimit));
    assert!(
        caps.supported_order_types
            .contains(&OrderType::TrailingStop)
    );

    assert!(caps.features.contains(&"bracket_orders".to_string()));
    assert!(caps.features.contains(&"trailing_stop".to_string()));
    assert!(caps.features.contains(&"get_quote".to_string()));
    assert!(caps.features.contains(&"short_selling".to_string()));

    assert_eq!(caps.position_mode, PositionMode::Netting);
    assert_eq!(caps.max_leverage, Some(dec!(30)));
}

#[tokio::test]
async fn connection_status_connected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        200,
        saxo_balance_json(),
    )
    .await;

    let status = adapter.get_connection_status().await.unwrap();
    assert!(status.connected);
}

#[tokio::test]
async fn connection_status_disconnected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        500,
        serde_json::json!({"Message": "Server Error"}),
    )
    .await;

    let status = adapter.get_connection_status().await.unwrap();
    assert!(!status.connected);
}
