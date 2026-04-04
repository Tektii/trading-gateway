mod helpers;

use helpers::test_futures_adapter;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn error_400_binance_code() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/account",
        400,
        json!({"code": -2010, "msg": "Account has insufficient balance for requested action."}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderRejected { .. }),
        "Expected OrderRejected, got: {err:?}"
    );
}

#[tokio::test]
async fn error_401_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/account",
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
async fn error_503_unavailable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/account",
        503,
        json!({"code": -1000, "msg": "Service unavailable"}),
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

#[tokio::test]
async fn circuit_breaker_trips() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_futures_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/fapi/v2/account",
        500,
        json!({"code": -1000, "msg": "Internal error"}),
    )
    .await;

    // Trip the circuit breaker with 3 failures
    for _ in 0..3 {
        let _ = adapter.get_account().await;
    }

    // 4th call should be blocked by circuit breaker
    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::ProviderUnavailable { .. }),
        "Expected ProviderUnavailable (circuit breaker), got: {err:?}"
    );
}
