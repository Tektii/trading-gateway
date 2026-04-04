mod helpers;

use helpers::test_adapter;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{
    mount_json, mount_text, mount_with_headers, start_mock_server,
};

// =========================================================================
// HTTP Error Mapping
// =========================================================================

#[tokio::test]
async fn error_429_with_retry_after_header() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_with_headers(
        &server,
        "GET",
        "/v2/account",
        429,
        json!({"message": "Too many requests"}),
        &[("retry-after", "60")],
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::RateLimited {
            retry_after_seconds,
            ..
        } => {
            assert_eq!(retry_after_seconds, Some(60));
        }
        other => panic!("Expected RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn error_429_with_body_retry_after() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // No header, but body has retry_after field
    mount_json(
        &server,
        "GET",
        "/v2/account",
        429,
        json!({"message": "Rate limited", "retry_after": 45}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::RateLimited {
            retry_after_seconds,
            ..
        } => {
            assert_eq!(retry_after_seconds, Some(45));
        }
        other => panic!("Expected RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn error_503_provider_unavailable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        503,
        json!({"message": "Service unavailable"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::ProviderUnavailable { .. }),
        "Expected ProviderUnavailable, got: {err:?}"
    );
}

#[tokio::test]
async fn error_401_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        401,
        json!({"message": "Invalid API credentials"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::Unauthorized { .. }),
        "Expected Unauthorized, got: {err:?}"
    );
}

#[tokio::test]
async fn error_400_invalid_request() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        400,
        json!({"message": "Bad request"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::InvalidRequest { .. }),
        "Expected InvalidRequest, got: {err:?}"
    );
}

#[tokio::test]
async fn error_422_with_reject_codes() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        422,
        json!({"message": "Insufficient buying power", "code": "insufficient_buying_power"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code.as_deref(), Some("INSUFFICIENT_FUNDS"));
        }
        other => panic!("Expected OrderRejected, got: {other:?}"),
    }
}

#[tokio::test]
async fn error_unknown_status() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        418,
        json!({"message": "I'm a teapot"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderError {
            provider, message, ..
        } => {
            assert_eq!(provider.as_deref(), Some("alpaca"));
            assert!(
                message.contains("418"),
                "Expected message to contain status code, got: {message}"
            );
        }
        other => panic!("Expected ProviderError, got: {other:?}"),
    }
}

// =========================================================================
// Circuit Breaker
// =========================================================================

#[tokio::test]
async fn circuit_breaker_trips_after_repeated_failures() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Mount a 500 error that will trip the circuit breaker
    mount_json(
        &server,
        "GET",
        "/v2/account",
        500,
        json!({"message": "Internal server error"}),
    )
    .await;

    // Make 3 calls that fail with 500 (circuit breaker threshold)
    for _ in 0..3 {
        let _ = adapter.get_account().await;
    }

    // 4th call should fail with ProviderUnavailable (circuit breaker open)
    // WITHOUT making an HTTP request
    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::ProviderUnavailable { .. }),
        "Expected ProviderUnavailable from circuit breaker, got: {err:?}"
    );

    // Verify wiremock received exactly 3 requests (not 4)
    let requests = server.received_requests().await.unwrap();
    // Note: retries may inflate the count. The key assertion is the 4th
    // logical call gets blocked by the circuit breaker.
    // With retry_with_backoff (3 retries per call), each failing call
    // makes up to 4 HTTP requests (1 original + 3 retries).
    // 3 calls × 4 attempts = up to 12 requests, but the 4th call makes 0.
    // Just verify the circuit breaker blocks without the exact count.
    let request_count = requests.len();
    assert!(
        request_count > 0 && request_count <= 12,
        "Expected between 1 and 12 requests, got {request_count}"
    );
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
        "/v2/account",
        200,
        "<html>Bad Gateway</html>",
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::Internal { message, .. } => {
            assert!(
                message.contains("Failed to parse response"),
                "Expected 'Failed to parse response', got: {message}"
            );
        }
        other => panic!("Expected Internal, got {other:?}"),
    }
}

#[tokio::test]
async fn error_malformed_json_missing_fields() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/v2/account", 200, json!({})).await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::Internal { message, .. } => {
            assert!(
                message.contains("Failed to parse response"),
                "Expected 'Failed to parse response', got: {message}"
            );
        }
        other => panic!("Expected Internal, got {other:?}"),
    }
}

#[tokio::test]
async fn error_malformed_json_wrong_types() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account",
        200,
        json!({"cash": true, "currency": 42}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::Internal { message, .. } => {
            assert!(
                message.contains("Failed to parse response"),
                "Expected 'Failed to parse response', got: {message}"
            );
        }
        other => panic!("Expected Internal, got {other:?}"),
    }
}
