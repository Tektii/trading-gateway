mod helpers;

use helpers::{saxo_balance_json, test_adapter};
use rust_decimal_macros::dec;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{
    mount_json, mount_with_headers, start_mock_server,
};

#[tokio::test]
async fn get_account_success() {
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

    let account = adapter.get_account().await.unwrap();
    assert_eq!(account.balance, dec!(100000));
    assert_eq!(account.equity, dec!(100500));
    assert_eq!(account.margin_used, dec!(5000));
    assert_eq!(account.margin_available, dec!(95000));
    assert_eq!(account.unrealized_pnl, dec!(500));
    assert_eq!(account.currency, "USD");
}

#[tokio::test]
async fn get_account_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // Mount 401 for the balance request — SaxoHttpClient will attempt a token refresh then retry.
    // Mount the token endpoint to also fail, so the retry gives up.
    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        401,
        serde_json::json!({"Message": "Unauthorized"}),
    )
    .await;
    mount_json(
        &server,
        "POST",
        "/connect/token",
        400,
        serde_json::json!({"error": "invalid_grant", "error_description": "Token expired"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::Unauthorized { .. }));
}

#[tokio::test]
async fn get_account_server_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        500,
        serde_json::json!({"Message": "Internal Server Error"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}

#[tokio::test]
async fn get_account_rate_limited() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_with_headers(
        &server,
        "GET",
        "/port/v1/balances/me",
        429,
        serde_json::json!({"Message": "Rate limited"}),
        &[("Retry-After", "30")],
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::RateLimited { .. }));
}
