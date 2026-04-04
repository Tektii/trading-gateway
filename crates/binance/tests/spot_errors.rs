mod helpers;

use helpers::test_spot_adapter;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

// =========================================================================
// Binance error code mapping (exercised through integration paths)
// =========================================================================

#[tokio::test]
async fn error_binance_1013_invalid_quantity() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        400,
        json!({"code": -1013, "msg": "Filter failure: LOT_SIZE"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderRejected { .. }),
        "Expected OrderRejected, got: {err:?}"
    );
}

#[tokio::test]
async fn error_binance_1015_rate_limited() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        400,
        json!({"code": -1015, "msg": "Too many new orders; current limit is 10 orders per 10 SECOND."}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::RateLimited { .. }),
        "Expected RateLimited, got: {err:?}"
    );
}

#[tokio::test]
async fn error_binance_1121_invalid_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        400,
        json!({"code": -1121, "msg": "Invalid symbol."}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::InvalidRequest { .. }),
        "Expected InvalidRequest, got: {err:?}"
    );
}

#[tokio::test]
async fn error_binance_2010_insufficient_balance() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
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
async fn error_binance_2013_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        400,
        json!({"code": -2013, "msg": "Order does not exist."}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotFound { .. }),
        "Expected OrderNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn error_binance_2015_invalid_api_key() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        400,
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
async fn circuit_breaker_trips_after_repeated_failures() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
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
