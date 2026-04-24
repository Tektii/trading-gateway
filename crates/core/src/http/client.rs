//! HTTP client helpers with retry support.

use std::future::Future;
use std::time::Instant;

use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use tracing::debug;

use super::retry::{RetryConfig, retry_with_backoff};
use crate::error::{GatewayError, GatewayResult};

/// Error mapper function type.
///
/// Maps HTTP error responses to `GatewayError`. Called for non-success
/// status codes. Adapters can provide custom mappers for provider-specific
/// error codes.
pub type ErrorMapper = fn(StatusCode, serde_json::Value, Option<u64>, &str) -> GatewayError;

/// Default error mapper for HTTP responses.
///
/// Provides generic error mapping without provider-specific logic.
pub fn default_error_mapper(
    status: StatusCode,
    body: serde_json::Value,
    retry_after: Option<u64>,
    provider: &str,
) -> GatewayError {
    match status {
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = retry_after
                .or_else(|| body.get("retry_after").and_then(serde_json::Value::as_u64))
                .or_else(|| {
                    body.get("retry_after_seconds")
                        .and_then(serde_json::Value::as_u64)
                });

            GatewayError::RateLimited {
                retry_after_seconds: retry_after,
                reset_at: None,
            }
        }

        StatusCode::SERVICE_UNAVAILABLE => GatewayError::ProviderUnavailable {
            message: extract_message(&body),
        },

        StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::GATEWAY_TIMEOUT => GatewayError::ProviderError {
            message: extract_message(&body),
            provider: Some(provider.to_string()),
            source: None,
        },

        StatusCode::BAD_REQUEST => GatewayError::InvalidRequest {
            message: extract_message(&body),
            field: None,
        },

        // 401 = bad credentials. 403 = authenticated but the requested action
        // isn't permitted — for trading APIs that's almost always an order
        // rejection (insufficient buying power, asset class disabled, cost
        // basis below minimum, etc), not an auth issue.
        StatusCode::UNAUTHORIZED => GatewayError::Unauthorized {
            reason: extract_message(&body),
            code: "AUTH_FAILED".to_string(),
        },

        StatusCode::FORBIDDEN => GatewayError::OrderRejected {
            reason: extract_message(&body),
            reject_code: body.get("code").and_then(|v| {
                v.as_str()
                    .map(String::from)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            }),
            details: Some(body),
        },

        StatusCode::NOT_FOUND => GatewayError::ProviderError {
            message: format!("Upstream returned 404: {}", extract_message(&body)),
            provider: Some(provider.to_string()),
            source: None,
        },

        StatusCode::UNPROCESSABLE_ENTITY => GatewayError::OrderRejected {
            reason: extract_message(&body),
            reject_code: body.get("code").and_then(|v| v.as_str()).map(String::from),
            details: Some(body),
        },

        _ => GatewayError::ProviderError {
            message: format!("HTTP {}: {}", status.as_u16(), extract_message(&body)),
            provider: Some(provider.to_string()),
            source: None,
        },
    }
}

/// Execute an HTTP request with retry on transient failures.
///
/// Wraps an HTTP request with exponential backoff retry logic.
/// 5xx errors and 429 are retried; 4xx errors fail immediately.
#[allow(clippy::future_not_send)]
pub async fn execute_with_retry<F, Fut>(
    build_request: F,
    provider: &'static str,
    config: Option<&RetryConfig>,
    error_mapper: Option<ErrorMapper>,
) -> GatewayResult<Response>
where
    F: Fn() -> Fut,
    Fut: Future<Output = RequestBuilder>,
{
    let default_config = RetryConfig::default();
    let config = config.unwrap_or(&default_config);
    let mapper = error_mapper.unwrap_or(default_error_mapper);

    retry_with_backoff(config, || {
        let request_future = build_request();
        async move {
            let downstream_start = Instant::now();

            let request = request_future.await;
            let response = request
                .send()
                .await
                .map_err(|e| GatewayError::ProviderError {
                    message: format!("Request failed: {e}"),
                    provider: Some(provider.to_string()),
                    source: None,
                })?;

            let downstream_time = downstream_start.elapsed();
            debug!(
                downstream_ms = downstream_time.as_secs_f64() * 1000.0,
                status = %response.status(),
                "Downstream API call completed"
            );

            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }

            map_error_response(response, provider, mapper).await
        }
    })
    .await
}

/// Map an error HTTP response to the appropriate `GatewayError`.
async fn map_error_response(
    response: Response,
    provider: &str,
    mapper: ErrorMapper,
) -> GatewayResult<Response> {
    let status = response.status();

    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let body = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

    Err(mapper(status, body, retry_after, provider))
}

/// Extract error message from response body.
///
/// Checks common error message field names: `message`, `error`, `msg`.
pub fn extract_message(body: &serde_json::Value) -> String {
    body.get("message")
        .or_else(|| body.get("error"))
        .or_else(|| body.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error")
        .to_string()
}

/// Parse JSON response body with proper error handling.
pub async fn parse_json<T: DeserializeOwned>(
    response: Response,
    provider: &str,
) -> GatewayResult<T> {
    response
        .json()
        .await
        .map_err(|e| GatewayError::internal(format!("Failed to parse {provider} response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_message_finds_message_field() {
        let body = serde_json::json!({"message": "Test error"});
        assert_eq!(extract_message(&body), "Test error");
    }

    #[test]
    fn extract_message_finds_error_field() {
        let body = serde_json::json!({"error": "Another error"});
        assert_eq!(extract_message(&body), "Another error");
    }

    #[test]
    fn extract_message_finds_msg_field() {
        let body = serde_json::json!({"msg": "Binance error"});
        assert_eq!(extract_message(&body), "Binance error");
    }

    #[test]
    fn extract_message_defaults_to_unknown() {
        let body = serde_json::json!({"code": 123});
        assert_eq!(extract_message(&body), "Unknown error");
    }

    // =====================================================================
    // default_error_mapper
    // =====================================================================

    #[test]
    fn default_mapper_429_header_takes_priority() {
        let body = serde_json::json!({"retry_after": 10});
        let err = default_error_mapper(StatusCode::TOO_MANY_REQUESTS, body, Some(5), "test");
        match err {
            GatewayError::RateLimited {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, Some(5)),
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_429_body_retry_after() {
        let body = serde_json::json!({"retry_after": 10});
        let err = default_error_mapper(StatusCode::TOO_MANY_REQUESTS, body, None, "test");
        match err {
            GatewayError::RateLimited {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, Some(10)),
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_429_body_retry_after_seconds() {
        let body = serde_json::json!({"retry_after_seconds": 15});
        let err = default_error_mapper(StatusCode::TOO_MANY_REQUESTS, body, None, "test");
        match err {
            GatewayError::RateLimited {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, Some(15)),
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_429_no_retry_info() {
        let body = serde_json::json!({});
        let err = default_error_mapper(StatusCode::TOO_MANY_REQUESTS, body, None, "test");
        match err {
            GatewayError::RateLimited {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, None),
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_503_provider_unavailable() {
        let body = serde_json::json!({"message": "Maintenance"});
        let err = default_error_mapper(StatusCode::SERVICE_UNAVAILABLE, body, None, "test");
        match err {
            GatewayError::ProviderUnavailable { message } => {
                assert_eq!(message, "Maintenance");
            }
            other => panic!("Expected ProviderUnavailable, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_500_provider_error() {
        let body = serde_json::json!({"message": "Crash"});
        let err = default_error_mapper(StatusCode::INTERNAL_SERVER_ERROR, body, None, "broker");
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert_eq!(message, "Crash");
                assert_eq!(provider.as_deref(), Some("broker"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_502_provider_error() {
        let body = serde_json::json!({"message": "Bad gateway"});
        let err = default_error_mapper(StatusCode::BAD_GATEWAY, body, None, "test");
        assert!(matches!(err, GatewayError::ProviderError { .. }));
    }

    #[test]
    fn default_mapper_504_provider_error() {
        let body = serde_json::json!({"message": "Timeout"});
        let err = default_error_mapper(StatusCode::GATEWAY_TIMEOUT, body, None, "test");
        assert!(matches!(err, GatewayError::ProviderError { .. }));
    }

    #[test]
    fn default_mapper_400_invalid_request() {
        let body = serde_json::json!({"message": "Bad params"});
        let err = default_error_mapper(StatusCode::BAD_REQUEST, body, None, "test");
        match err {
            GatewayError::InvalidRequest { message, field } => {
                assert_eq!(message, "Bad params");
                assert_eq!(field, None);
            }
            other => panic!("Expected InvalidRequest, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_401_unauthorized() {
        let body = serde_json::json!({"message": "Bad token"});
        let err = default_error_mapper(StatusCode::UNAUTHORIZED, body, None, "test");
        match err {
            GatewayError::Unauthorized { reason, code } => {
                assert_eq!(reason, "Bad token");
                assert_eq!(code, "AUTH_FAILED");
            }
            other => panic!("Expected Unauthorized, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_403_order_rejected() {
        // 403 = action forbidden — for brokers this is order-level (insufficient
        // funds, cost basis below minimum, etc), not bad credentials. Should
        // surface as OrderRejected so SDK callers don't misread it as auth.
        let body = serde_json::json!({
            "message": "cost basis must be >= minimal amount of order 10",
            "code": 40310000
        });
        let err = default_error_mapper(StatusCode::FORBIDDEN, body, None, "test");
        match err {
            GatewayError::OrderRejected {
                reason,
                reject_code,
                ..
            } => {
                assert!(reason.contains("cost basis"), "got: {reason}");
                assert_eq!(reject_code.as_deref(), Some("40310000"));
            }
            other => panic!("Expected OrderRejected, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_404_provider_error() {
        let body = serde_json::json!({"message": "Not found"});
        let err = default_error_mapper(StatusCode::NOT_FOUND, body, None, "test");
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("404"), "message should mention 404");
                assert_eq!(provider.as_deref(), Some("test"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_422_order_rejected() {
        let body = serde_json::json!({"message": "Rejected", "code": "LIMIT_EXCEEDED"});
        let err = default_error_mapper(StatusCode::UNPROCESSABLE_ENTITY, body, None, "test");
        match err {
            GatewayError::OrderRejected {
                reason,
                reject_code,
                details,
            } => {
                assert_eq!(reason, "Rejected");
                assert_eq!(reject_code.as_deref(), Some("LIMIT_EXCEEDED"));
                assert!(details.is_some());
            }
            other => panic!("Expected OrderRejected, got: {other:?}"),
        }
    }

    #[test]
    fn default_mapper_unknown_status() {
        let body = serde_json::json!({"message": "Teapot"});
        let err = default_error_mapper(StatusCode::IM_A_TEAPOT, body, None, "test");
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("HTTP 418"), "got: {message}");
                assert_eq!(provider.as_deref(), Some("test"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }
}
