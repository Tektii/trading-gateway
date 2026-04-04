//! HTTP middleware for request logging, correlation IDs, timing, and authentication.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use secrecy::ExposeSecret;
use std::time::Instant;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::api::state::GatewayState;
use crate::error::GatewayError;

/// Maximum length for correlation IDs. Covers UUID (36), W3C traceparent (55),
/// and AWS X-Ray (~96) with headroom. Values beyond this are silently truncated.
const MAX_CORRELATION_ID_LEN: usize = 128;

/// Extension type to carry `correlation_id` through the request lifecycle.
#[derive(Clone, Debug)]
pub struct CorrelationId(pub String);

/// Extension type to carry timing information through the request lifecycle.
#[derive(Clone, Debug)]
pub struct RequestTiming {
    /// Request start time for calculating duration.
    pub start_time: Instant,
}

/// Extract or generate a correlation ID from the request, truncating to [`MAX_CORRELATION_ID_LEN`].
fn resolve_correlation_id(request: &Request) -> String {
    request
        .headers()
        .get("X-Correlation-ID")
        .and_then(|v| v.to_str().ok())
        .map_or_else(
            || Uuid::new_v4().to_string(),
            |v| truncate_to_char_boundary(v, MAX_CORRELATION_ID_LEN).to_string(),
        )
}

/// Truncate a string to at most `max_len` bytes on a valid UTF-8 boundary.
fn truncate_to_char_boundary(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    // Find the largest char boundary <= max_len
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Middleware to log incoming requests with correlation ID, method, path, and provider.
pub async fn logging_middleware(request: Request, next: Next) -> Response {
    let start_time = Instant::now();

    let correlation_id = resolve_correlation_id(&request);

    let method = request.method().to_string();
    let path = request.uri().path().to_string();

    tracing::info!(
        correlation_id = %correlation_id,
        method = %method,
        path = %path,
        "Incoming request"
    );

    let response = next.run(request).await;

    let total_ms = u64::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
    let status = response.status();

    tracing::info!(
        correlation_id = %correlation_id,
        method = %method,
        path = %path,
        status = %status.as_u16(),
        total_ms = %total_ms,
        "Request completed"
    );

    response
}

/// Middleware to inject `correlation_id` into request extensions.
pub async fn correlation_id_middleware(mut request: Request, next: Next) -> Response {
    let correlation_id = resolve_correlation_id(&request);

    request
        .extensions_mut()
        .insert(CorrelationId(correlation_id));

    next.run(request).await
}

/// Middleware to inject timing information into request extensions.
pub async fn timing_middleware(mut request: Request, next: Next) -> Response {
    let start_time = Instant::now();

    request
        .extensions_mut()
        .insert(RequestTiming { start_time });

    next.run(request).await
}

/// Middleware to enforce API key authentication when `GATEWAY_API_KEY` is set.
///
/// When the gateway has no API key configured, all requests pass through.
/// When configured, requests must provide the key via either:
///   - `Authorization: Bearer <key>`
///   - `X-API-Key: <key>`
///
/// Health and operational endpoints bypass authentication.
/// Uses constant-time comparison to prevent timing side-channels.
pub async fn auth_middleware(
    State(state): State<GatewayState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected_key) = state.api_key() else {
        return next.run(request).await;
    };

    if is_public_path(request.uri().path()) {
        return next.run(request).await;
    }

    if let Some(key) = extract_api_key(&request) {
        let expected = expected_key.expose_secret().as_bytes();
        let provided = key.as_bytes();

        if expected.len() == provided.len() && bool::from(expected.ct_eq(provided)) {
            next.run(request).await
        } else {
            tracing::warn!("Authentication failed: invalid API key");
            GatewayError::unauthorized("Invalid API key", "INVALID_API_KEY").into_response()
        }
    } else {
        tracing::warn!("Authentication failed: no API key provided");
        GatewayError::unauthorized("API key required", "MISSING_API_KEY").into_response()
    }
}

/// Extract API key from request headers.
///
/// Checks `Authorization: Bearer <key>` first, then falls back to `X-API-Key`.
fn extract_api_key(request: &Request) -> Option<&str> {
    if let Some(auth_header) = request.headers().get("authorization")
        && let Ok(value) = auth_header.to_str()
        && let Some(key) = value.strip_prefix("Bearer ")
    {
        let key = key.trim();
        if !key.is_empty() {
            return Some(key);
        }
    }

    if let Some(api_key_header) = request.headers().get("x-api-key")
        && let Ok(value) = api_key_header.to_str()
    {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value);
        }
    }

    None
}

/// Returns `true` for paths that should bypass authentication.
fn is_public_path(path: &str) -> bool {
    matches!(path, "/livez" | "/readyz" | "/health" | "/metrics")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn test_request() -> axum::http::request::Builder {
        Request::builder()
    }

    #[test]
    fn extract_api_key_from_bearer_header() {
        let request = test_request()
            .header("authorization", "Bearer test-key-1234")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_key(&request), Some("test-key-1234"));
    }

    #[test]
    fn extract_api_key_from_x_api_key_header() {
        let request = test_request()
            .header("x-api-key", "test-key-5678")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_key(&request), Some("test-key-5678"));
    }

    #[test]
    fn extract_api_key_bearer_takes_precedence() {
        let request = test_request()
            .header("authorization", "Bearer bearer-key")
            .header("x-api-key", "header-key")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_key(&request), Some("bearer-key"));
    }

    #[test]
    fn extract_api_key_empty_bearer_falls_through() {
        let request = test_request()
            .header("authorization", "Bearer ")
            .header("x-api-key", "fallback-key")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_key(&request), Some("fallback-key"));
    }

    #[test]
    fn extract_api_key_no_headers() {
        let request = test_request().body(Body::empty()).unwrap();
        assert_eq!(extract_api_key(&request), None);
    }

    #[test]
    fn extract_api_key_wrong_auth_scheme() {
        let request = test_request()
            .header("authorization", "Basic dXNlcjpwYXNz")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_api_key(&request), None);
    }

    #[test]
    fn public_paths_bypass() {
        assert!(is_public_path("/livez"));
        assert!(is_public_path("/readyz"));
        assert!(is_public_path("/health"));
        assert!(is_public_path("/metrics"));
    }

    #[test]
    fn swagger_paths_require_auth() {
        assert!(!is_public_path("/swagger-ui"));
        assert!(!is_public_path("/swagger-ui/index.html"));
        assert!(!is_public_path("/api-docs/openapi.json"));
    }

    #[test]
    fn api_paths_require_auth() {
        assert!(!is_public_path("/v1/orders"));
        assert!(!is_public_path("/v1/ws"));
        assert!(!is_public_path("/v1/account"));
        assert!(!is_public_path("/v1/positions"));
    }

    #[test]
    fn correlation_id_truncated_when_too_long() {
        let long_id = "x".repeat(300);
        let request = test_request()
            .header("X-Correlation-ID", &long_id)
            .body(Body::empty())
            .unwrap();
        let result = resolve_correlation_id(&request);
        assert_eq!(result.len(), MAX_CORRELATION_ID_LEN);
        assert_eq!(result, "x".repeat(MAX_CORRELATION_ID_LEN));
    }

    #[test]
    fn correlation_id_preserved_when_within_limit() {
        let id = "my-trace-id-12345";
        let request = test_request()
            .header("X-Correlation-ID", id)
            .body(Body::empty())
            .unwrap();
        let result = resolve_correlation_id(&request);
        assert_eq!(result, id);
    }

    #[test]
    fn correlation_id_generated_when_missing() {
        let request = test_request().body(Body::empty()).unwrap();
        let result = resolve_correlation_id(&request);
        // Should be a valid UUID v4 (36 chars with hyphens)
        assert_eq!(result.len(), 36);
        assert!(uuid::Uuid::parse_str(&result).is_ok());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // Multi-byte UTF-8: each emoji is 4 bytes
        let s = "🔥🔥🔥"; // 12 bytes
        assert_eq!(truncate_to_char_boundary(s, 5), "🔥"); // 4 bytes, not 5
        assert_eq!(truncate_to_char_boundary(s, 8), "🔥🔥");
        assert_eq!(truncate_to_char_boundary(s, 100), s);
    }
}
