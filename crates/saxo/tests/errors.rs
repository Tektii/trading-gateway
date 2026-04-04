mod helpers;

use helpers::test_adapter;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_test_support::wiremock_helpers::{
    mount_json, mount_text, mount_with_headers, start_mock_server,
};

#[tokio::test]
async fn error_429_rate_limited() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_with_headers(
        &server,
        "GET",
        "/port/v1/balances/me",
        429,
        json!({"Message": "Rate limited"}),
        &[("Retry-After", "30")],
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::RateLimited { .. }));
}

#[tokio::test]
async fn error_503_provider_unavailable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        503,
        json!({"Message": "Service Unavailable"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderUnavailable { message } => {
            assert!(
                message.contains("503") || message.contains("Service Unavailable"),
                "Expected message to reference 503, got: {message}"
            );
        }
        other => panic!("Expected ProviderUnavailable, got: {other:?}"),
    }
}

#[tokio::test]
async fn error_401_unauthorized() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // 401 triggers token refresh attempt — mock that to fail too
    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        401,
        json!({"Message": "Unauthorized"}),
    )
    .await;
    mount_json(
        &server,
        "POST",
        "/connect/token",
        400,
        json!({"error": "invalid_grant", "error_description": "Token expired"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::Unauthorized { .. }));
}

#[tokio::test]
async fn error_400_invalid_request() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        400,
        json!({"Message": "Bad Request"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::InvalidRequest { .. }));
}

#[tokio::test]
async fn error_404_invalid_request() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        404,
        json!({"Message": "Not Found"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    // Saxo maps all 4xx to InvalidRequest
    assert!(matches!(err, GatewayError::InvalidRequest { .. }));
}

#[tokio::test]
async fn error_unknown_status() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        418,
        json!({"Message": "I'm a teapot"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::InvalidRequest { .. }));
}

#[tokio::test]
async fn error_saxo_body_parsed() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        400,
        json!({"Message": "Invalid account key", "ErrorCode": "InvalidAccountKey"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("Invalid account key") || msg.contains("InvalidAccountKey"),
        "Error should contain parsed body message, got: {msg}"
    );
}

#[tokio::test]
async fn circuit_breaker_trips() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // Mount 500 for all balance requests
    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        500,
        json!({"Message": "Internal Server Error"}),
    )
    .await;

    // Circuit breaker triggers after 3 failures (configured in SaxoAdapter::new)
    for _ in 0..3 {
        let _ = adapter.get_account().await;
    }

    // 4th call should be blocked by circuit breaker
    let err = adapter.get_account().await.unwrap_err();
    assert!(
        matches!(err, GatewayError::ProviderUnavailable { .. }),
        "Expected ProviderUnavailable after circuit breaker trip, got: {err:?}"
    );
}

// =========================================================================
// Malformed Broker Responses
// =========================================================================

#[tokio::test]
async fn error_malformed_json_non_json_body() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_text(
        &server,
        "GET",
        "/port/v1/balances/me",
        200,
        "<html>Bad Gateway</html>",
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderError {
            message, provider, ..
        } => {
            assert!(
                message.contains("Failed to parse Saxo response"),
                "Expected 'Failed to parse Saxo response', got: {message}"
            );
            assert_eq!(provider.as_deref(), Some("saxo"));
        }
        other => panic!("Expected ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn error_malformed_json_truncated_body() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    // Saxo uses #[serde(default)] on all balance fields, so {} succeeds.
    // Truncated JSON simulates network interruption mid-response.
    mount_text(
        &server,
        "GET",
        "/port/v1/balances/me",
        200,
        r#"{"TotalValue": 100"#,
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderError {
            message, provider, ..
        } => {
            assert!(
                message.contains("Failed to parse Saxo response"),
                "Expected 'Failed to parse Saxo response', got: {message}"
            );
            assert_eq!(provider.as_deref(), Some("saxo"));
        }
        other => panic!("Expected ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn error_malformed_json_wrong_types() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/balances/me",
        200,
        json!({"TotalValue": "not_a_number", "Currency": 42}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderError {
            message, provider, ..
        } => {
            assert!(
                message.contains("Failed to parse Saxo response"),
                "Expected 'Failed to parse Saxo response', got: {message}"
            );
            assert_eq!(provider.as_deref(), Some("saxo"));
        }
        other => panic!("Expected ProviderError, got {other:?}"),
    }
}
