mod helpers;

use helpers::{oanda_account_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

// =========================================================================
// Account: Success
// =========================================================================

#[tokio::test]
async fn get_account_success() {
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

    let account = adapter.get_account().await.unwrap();
    assert_eq!(account.balance, dec!(100000.00));
    assert_eq!(account.equity, dec!(100500.00));
    assert_eq!(account.margin_used, dec!(5000.00));
    assert_eq!(account.margin_available, dec!(95000.00));
    assert_eq!(account.unrealized_pnl, dec!(500.00));
    assert_eq!(account.currency, "USD");
}

// =========================================================================
// Account: Error cases
// =========================================================================

#[tokio::test]
async fn get_account_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        401,
        json!({"errorMessage": "Invalid token"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::Unauthorized { .. }));
}

#[tokio::test]
async fn get_account_server_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        500,
        json!({"errorMessage": "Internal server error"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}

#[tokio::test]
async fn get_account_rate_limited() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        429,
        json!({"errorMessage": "Rate limit exceeded"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(
        err,
        GatewayError::RateLimited {
            retry_after_seconds: None,
            ..
        }
    ));
}
