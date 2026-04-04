mod helpers;

use helpers::test_adapter;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, mount_text, start_mock_server};

const ACCOUNT_PATH: &str = "/v3/accounts/test-account-123/summary";

// =========================================================================
// HTTP Status Code Mapping
// =========================================================================

#[tokio::test]
async fn error_429_rate_limited() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
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

#[tokio::test]
async fn error_503_unavailable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        503,
        json!({"errorMessage": "Service unavailable"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderUnavailable { .. }));
}

#[tokio::test]
async fn error_401_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        401,
        json!({"errorMessage": "Invalid token"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::Unauthorized { .. }));
}

#[tokio::test]
async fn error_403_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        403,
        json!({"errorMessage": "Forbidden"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::Unauthorized { .. }));
}

#[tokio::test]
async fn error_404_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        404,
        json!({"errorMessage": "Not found"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}

#[tokio::test]
async fn error_400_invalid_request() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        400,
        json!({"errorMessage": "Bad request"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::InvalidRequest { .. }));
}

// =========================================================================
// 422 with Reject Reasons
// =========================================================================

#[tokio::test]
async fn error_422_insufficient_margin() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        422,
        json!({"errorMessage": "Order rejected", "rejectReason": "INSUFFICIENT_MARGIN"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, Some("INSUFFICIENT_FUNDS".to_string()));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn error_422_market_halted() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        422,
        json!({"errorMessage": "Order rejected", "rejectReason": "MARKET_HALTED"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, Some("MARKET_CLOSED".to_string()));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn error_422_empty_reason() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        422,
        json!({"errorMessage": "Order rejected", "rejectReason": ""}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, Some("ORDER_REJECTED".to_string()));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

// =========================================================================
// Unknown Status Code
// =========================================================================

#[tokio::test]
async fn error_unknown_status() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        418,
        json!({"errorMessage": "I'm a teapot"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderError {
            message, provider, ..
        } => {
            assert_eq!(provider.as_deref(), Some("oanda"));
            assert!(message.contains("HTTP 418"));
        }
        other => panic!("Expected ProviderError, got {other:?}"),
    }
}

// =========================================================================
// Circuit Breaker
// =========================================================================

#[tokio::test]
async fn circuit_breaker_trips_after_repeated_failures() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Mount 500 error
    mount_json(
        &server,
        "GET",
        ACCOUNT_PATH,
        500,
        json!({"errorMessage": "Internal server error"}),
    )
    .await;

    // Trip the circuit breaker with 3 failures
    for _ in 0..3 {
        let _ = adapter.get_account().await;
    }

    // 4th call should fail with ProviderUnavailable from circuit breaker
    // (before making HTTP request)
    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderUnavailable { .. }));
}

// =========================================================================
// Malformed Broker Responses
// =========================================================================

#[tokio::test]
async fn error_malformed_json_non_json_body() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_text(
        &server,
        "GET",
        ACCOUNT_PATH,
        200,
        "<html>Bad Gateway</html>",
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::Internal { message, .. } => {
            assert!(
                message.contains("Failed to parse Oanda response"),
                "Expected 'Failed to parse Oanda response', got: {message}"
            );
        }
        other => panic!("Expected Internal, got {other:?}"),
    }
}

#[tokio::test]
async fn error_malformed_json_missing_fields() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", ACCOUNT_PATH, 200, json!({})).await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::Internal { message, .. } => {
            assert!(
                message.contains("Failed to parse Oanda response"),
                "Expected 'Failed to parse Oanda response', got: {message}"
            );
        }
        other => panic!("Expected Internal, got {other:?}"),
    }
}

#[tokio::test]
async fn error_malformed_json_wrong_types() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", ACCOUNT_PATH, 200, json!({"account": true})).await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::Internal { message, .. } => {
            assert!(
                message.contains("Failed to parse Oanda response"),
                "Expected 'Failed to parse Oanda response', got: {message}"
            );
        }
        other => panic!("Expected Internal, got {other:?}"),
    }
}
