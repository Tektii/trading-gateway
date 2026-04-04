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
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/account",
        429,
        json!({"error": "rate limited"}),
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
async fn error_429_with_retry_after_header() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_with_headers(
        &server,
        "GET",
        "/api/v1/account",
        429,
        json!({"error": "rate limited"}),
        &[("Retry-After", "60")],
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(
        err,
        GatewayError::RateLimited {
            retry_after_seconds: Some(60),
            ..
        }
    ));
}

#[tokio::test]
async fn error_422_order_rejected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/account",
        422,
        json!({"code": "INSUFFICIENT_MARGIN", "message": "Not enough margin"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code.as_deref(), Some("INSUFFICIENT_MARGIN"));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn error_422_no_code_field() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/account",
        422,
        json!({"message": "bad request"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, None);
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn error_503_provider_unavailable() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/account",
        503,
        json!({"error": "service unavailable"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderUnavailable { .. }));
}

#[tokio::test]
async fn error_unknown_status() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v1/account",
        418,
        json!({"error": "I'm a teapot"}),
    )
    .await;

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderError { provider, .. } => {
            assert_eq!(provider.as_deref(), Some("tektii"));
        }
        other => panic!("Expected ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn error_network_failure() {
    // Point adapter at a port with nothing listening
    let adapter = test_adapter("http://127.0.0.1:1");

    let err = adapter.get_account().await.unwrap_err();
    match err {
        GatewayError::ProviderError { provider, .. } => {
            assert_eq!(provider.as_deref(), Some("tektii"));
        }
        other => panic!("Expected ProviderError, got {other:?}"),
    }
}

#[tokio::test]
async fn error_malformed_json_non_json_body() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_text(
        &server,
        "GET",
        "/api/v1/account",
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

    mount_json(&server, "GET", "/api/v1/account", 200, json!({})).await;

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
        "/api/v1/account",
        200,
        json!({"balance": true, "currency": 42}),
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
