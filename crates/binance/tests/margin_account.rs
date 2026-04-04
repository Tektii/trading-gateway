mod helpers;

use helpers::{binance_margin_account_json, test_margin_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_account_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/account",
        200,
        binance_margin_account_json(),
    )
    .await;

    let account = adapter.get_account().await.unwrap();
    assert_eq!(account.balance, dec!(50000.0));
    assert_eq!(account.margin_used, dec!(5000.0));
    assert_eq!(account.equity, dec!(55000.0));
    assert_eq!(account.currency, "USDT");
}

#[tokio::test]
async fn get_account_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/account",
        401,
        json!({"code": -2015, "msg": "Invalid API-key, IP, or permissions for action."}),
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
    let adapter = test_margin_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/sapi/v1/margin/account",
        500,
        json!({"code": -1000, "msg": "Internal error"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(
            err,
            GatewayError::ProviderError { .. } | GatewayError::ProviderUnavailable { .. }
        ),
        "Expected ProviderError or ProviderUnavailable, got: {err:?}"
    );
}
