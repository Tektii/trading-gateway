mod helpers;

use helpers::{engine_account_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_account_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/account",
        200,
        engine_account_json(),
    )
    .await;

    let account = adapter.get_account().await.unwrap();
    assert_eq!(account.balance, dec!(100000));
    assert_eq!(account.equity, dec!(105000));
    assert_eq!(account.unrealized_pnl, dec!(5000));
    assert_eq!(account.margin_used, dec!(10000));
    assert_eq!(account.margin_available, dec!(90000));
    assert_eq!(account.currency, "USD");
}

#[tokio::test]
async fn get_account_server_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/account",
        500,
        json!({"error": "internal server error"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}
