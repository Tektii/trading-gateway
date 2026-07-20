mod helpers;

use helpers::test_adapter;
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{OrderRequest, OrderType};
use tektii_gateway_test_support::models::test_order_request;
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
async fn error_422_surfaces_engine_message_and_code() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        422,
        json!({
            "error": {
                "code": "invalid_order",
                "message": "Stop price 1.10235090 is not aligned to tick size 0.00001",
                "details": {"field": "stop_price"}
            }
        }),
    )
    .await;

    let request = OrderRequest {
        order_type: OrderType::Stop,
        stop_price: Some(dec!(1.10235090)),
        ..test_order_request()
    };

    let err = adapter.submit_order(&request).await.unwrap_err();
    match err {
        GatewayError::OrderRejected {
            reason,
            reject_code,
            details,
        } => {
            assert_eq!(
                reason,
                "Stop price 1.10235090 is not aligned to tick size 0.00001"
            );
            assert_eq!(reject_code.as_deref(), Some("invalid_order"));
            assert_eq!(
                details
                    .as_ref()
                    .and_then(|d| d.pointer("/error/details/field")),
                Some(&json!("stop_price"))
            );
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn error_422_envelope_without_code() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/api/v1/orders",
        422,
        json!({"error": {"message": "Insufficient margin"}}),
    )
    .await;

    let err = adapter
        .submit_order(&test_order_request())
        .await
        .unwrap_err();
    match err {
        GatewayError::OrderRejected {
            reason,
            reject_code,
            ..
        } => {
            assert_eq!(reason, "Insufficient margin");
            assert_eq!(reject_code, None);
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn error_422_non_envelope_body_falls_back_to_raw() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_text(
        &server,
        "POST",
        "/api/v1/orders",
        422,
        "<html>Unprocessable</html>",
    )
    .await;

    let err = adapter
        .submit_order(&test_order_request())
        .await
        .unwrap_err();
    match err {
        GatewayError::OrderRejected {
            reason,
            reject_code,
            ..
        } => {
            assert_eq!(reason, "<html>Unprocessable</html>");
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
