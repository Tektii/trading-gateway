mod helpers;

use helpers::{alpaca_account_json, test_adapter};
use rust_decimal_macros::dec;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{
    mount_json, mount_with_headers, start_mock_server,
};

#[tokio::test]
async fn get_account_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/v2/account", 200, alpaca_account_json()).await;

    let account = adapter.get_account().await.unwrap();

    assert_eq!(account.currency, "USD");
    assert_eq!(account.balance, dec!(100_000.00));
    assert_eq!(account.equity, dec!(105_000.00));
    assert_eq!(account.margin_available, dec!(200_000.00));
}

#[tokio::test]
async fn get_account_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        401,
        serde_json::json!({"message": "Invalid API key"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::Unauthorized { .. }),
        "Expected Unauthorized, got: {err:?}"
    );
}

#[tokio::test]
async fn get_account_server_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        500,
        serde_json::json!({"message": "Internal server error"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::ProviderError { .. }),
        "Expected ProviderError, got: {err:?}"
    );
}

#[tokio::test]
async fn get_account_rate_limited() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_with_headers(
        &server,
        "GET",
        "/v2/account",
        429,
        serde_json::json!({"message": "Rate limit exceeded"}),
        &[("retry-after", "30")],
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::RateLimited {
            retry_after_seconds,
            ..
        } => {
            assert_eq!(retry_after_seconds, Some(30));
        }
        other => panic!("Expected RateLimited, got: {other:?}"),
    }
}
