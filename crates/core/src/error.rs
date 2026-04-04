//! Gateway error types and conversions.

use std::sync::LazyLock;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use utoipa::ToSchema;

/// Matches credential-like `key=value` or `key: value` patterns in strings.
static CREDENTIAL_VALUE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(api_?key|secret|token|password|authorization)\s*[=:]\s*\S+")
        .expect("credential value regex is valid")
});

/// Matches JSON object keys that are credential field names.
static CREDENTIAL_KEY_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(api_?key|secret|token|password|authorization)$")
        .expect("credential key regex is valid")
});

/// Scrub credential-like patterns from a string, replacing values with `***REDACTED***`.
fn scrub_credentials(input: &str) -> String {
    CREDENTIAL_VALUE_PATTERN
        .replace_all(input, "$1=***REDACTED***")
        .into_owned()
}

/// Recursively scrub credential patterns from all string values in a JSON tree.
fn scrub_json_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = scrub_credentials(s);
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                scrub_json_values(item);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if CREDENTIAL_KEY_PATTERN.is_match(key)
                    && let serde_json::Value::String(_) = val
                {
                    *val = serde_json::Value::String("***REDACTED***".to_string());
                }
                scrub_json_values(val);
            }
        }
        _ => {}
    }
}

/// Machine-readable error codes for API error responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidValue,
    ValidationError,
    Unauthorized,
    OrderNotFound,
    PositionNotFound,
    SymbolNotFound,
    OcoGroupNotFound,
    OrderNotModifiable,
    ResetCooldown,
    OrderRejected,
    RateLimited,
    InternalError,
    UnsupportedOperation,
    ProviderError,
    ShuttingDown,
    ProviderUnavailable,
}

impl ErrorCode {
    /// Returns the error code as a static string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::InvalidValue => "INVALID_VALUE",
            Self::ValidationError => "VALIDATION_ERROR",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::OrderNotFound => "ORDER_NOT_FOUND",
            Self::PositionNotFound => "POSITION_NOT_FOUND",
            Self::SymbolNotFound => "SYMBOL_NOT_FOUND",
            Self::OcoGroupNotFound => "OCO_GROUP_NOT_FOUND",
            Self::OrderNotModifiable => "ORDER_NOT_MODIFIABLE",
            Self::ResetCooldown => "RESET_COOLDOWN",
            Self::OrderRejected => "ORDER_REJECTED",
            Self::RateLimited => "RATE_LIMITED",
            Self::InternalError => "INTERNAL_ERROR",
            Self::UnsupportedOperation => "UNSUPPORTED_OPERATION",
            Self::ProviderError => "PROVIDER_ERROR",
            Self::ShuttingDown => "SHUTTING_DOWN",
            Self::ProviderUnavailable => "PROVIDER_UNAVAILABLE",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// API error response body (JSON wire format).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct ApiError {
    /// Machine-readable error code (e.g., `ORDER_NOT_FOUND`, `RATE_LIMITED`)
    pub code: ErrorCode,

    /// Human-readable error message
    pub message: String,

    /// Additional error context (varies by error type)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Gateway error types.
#[derive(Error, Debug)]
pub enum GatewayError {
    // === 400 Bad Request ===
    /// Invalid request format or parameters
    #[error("Invalid request: {message}")]
    InvalidRequest {
        /// Error description
        message: String,
        /// Field that caused the error (if applicable)
        field: Option<String>,
    },

    /// Invalid value for a specific field
    #[error("Invalid value for {field}: {message}")]
    InvalidValue {
        /// Field name with invalid value
        field: String,
        /// Description of what's wrong
        message: String,
        /// The invalid value that was provided
        provided: Option<String>,
    },

    /// Validation error from the validator crate
    #[error("Validation failed: {0}")]
    ValidationError(#[from] validator::ValidationErrors),

    // === 401 Unauthorized ===
    /// Authentication failed or credentials invalid
    #[error("Unauthorized: {reason}")]
    Unauthorized {
        /// Reason for authentication failure
        reason: String,
        /// Error code for programmatic handling
        code: String,
    },

    // === 404 Not Found ===
    /// Order not found
    #[error("Order not found: {id}")]
    OrderNotFound {
        /// Order ID that was not found
        id: String,
    },

    /// Position not found
    #[error("Position not found: {id}")]
    PositionNotFound {
        /// Position ID that was not found
        id: String,
    },

    /// Symbol not found or not supported
    #[error("Symbol not found: {symbol}")]
    SymbolNotFound {
        /// Symbol that was not found
        symbol: String,
    },

    /// OCO group not found
    #[error("OCO group not found: {id}")]
    OcoGroupNotFound {
        /// OCO group ID that was not found
        id: String,
    },

    // === 409 Conflict ===
    /// Order cannot be modified (already filled, cancelled, etc.)
    #[error("Order cannot be modified: {reason}")]
    OrderNotModifiable {
        /// Order ID that cannot be modified
        order_id: String,
        /// Reason the order cannot be modified
        reason: String,
    },

    /// Circuit breaker reset rejected due to cooldown period.
    #[error("{message}")]
    ResetCooldown {
        /// Cooldown reason message.
        message: String,
    },

    // === 422 Unprocessable Entity ===
    /// Order rejected by the exchange/broker
    #[error("Order rejected: {reason}")]
    OrderRejected {
        /// Rejection reason
        reason: String,
        /// Provider-specific rejection code
        reject_code: Option<String>,
        /// Additional rejection details
        details: Option<serde_json::Value>,
    },

    // === 429 Rate Limited ===
    /// Rate limit exceeded
    #[error("Rate limit exceeded")]
    RateLimited {
        /// Seconds to wait before retrying
        retry_after_seconds: Option<u64>,
        /// Timestamp when rate limit resets
        reset_at: Option<chrono::DateTime<chrono::Utc>>,
    },

    // === 500 Internal Server Error ===
    /// Internal server error
    #[error("Internal error: {message}")]
    Internal {
        /// Error description
        message: String,
        /// Underlying error (if any)
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    // === 502 Bad Gateway ===
    /// Error from upstream provider
    #[error("Provider error: {message}")]
    ProviderError {
        /// Error description
        message: String,
        /// Provider name (e.g., "alpaca", "binance")
        provider: Option<String>,
        /// Underlying error (if any)
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    // === 503 Service Unavailable ===
    /// Gateway is shutting down — reject new orders
    #[error("Gateway is shutting down")]
    ShuttingDown,

    /// Provider is unavailable
    #[error("Provider unavailable: {message}")]
    ProviderUnavailable {
        /// Reason provider is unavailable
        message: String,
    },

    // === 501 Not Implemented ===
    /// Operation not supported by this provider
    #[error("{operation} not supported on {provider}")]
    UnsupportedOperation {
        /// The operation that was attempted
        operation: String,
        /// The provider that doesn't support it
        provider: String,
    },
}

impl GatewayError {
    /// Create an internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
            source: None,
        }
    }

    /// Create an internal error wrapping another error.
    pub fn internal_with_source<E>(message: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Internal {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Create a provider error with source.
    pub fn provider_error<E>(
        message: impl Into<String>,
        provider: impl Into<String>,
        source: E,
    ) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::ProviderError {
            message: message.into(),
            provider: Some(provider.into()),
            source: Some(Box::new(source)),
        }
    }

    /// Create an unauthorized error.
    pub fn unauthorized(reason: impl Into<String>, code: impl Into<String>) -> Self {
        Self::Unauthorized {
            reason: reason.into(),
            code: code.into(),
        }
    }

    /// Create an unsupported operation error.
    pub fn unsupported(operation: impl Into<String>, provider: impl Into<String>) -> Self {
        Self::UnsupportedOperation {
            operation: operation.into(),
            provider: provider.into(),
        }
    }

    /// HTTP status code for this error.
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidRequest { .. } | Self::InvalidValue { .. } | Self::ValidationError(_) => {
                StatusCode::BAD_REQUEST
            }

            Self::Unauthorized { .. } => StatusCode::UNAUTHORIZED,

            Self::OrderNotFound { .. }
            | Self::PositionNotFound { .. }
            | Self::SymbolNotFound { .. }
            | Self::OcoGroupNotFound { .. } => StatusCode::NOT_FOUND,

            Self::OrderNotModifiable { .. } | Self::ResetCooldown { .. } => StatusCode::CONFLICT,

            Self::OrderRejected { .. } => StatusCode::UNPROCESSABLE_ENTITY,

            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,

            Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,

            Self::UnsupportedOperation { .. } => StatusCode::NOT_IMPLEMENTED,

            Self::ProviderError { .. } => StatusCode::BAD_GATEWAY,

            Self::ShuttingDown | Self::ProviderUnavailable { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }
    }

    /// Machine-readable error code.
    #[must_use]
    pub const fn error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest { .. } => ErrorCode::InvalidRequest,
            Self::InvalidValue { .. } => ErrorCode::InvalidValue,
            Self::ValidationError(_) => ErrorCode::ValidationError,
            Self::Unauthorized { .. } => ErrorCode::Unauthorized,
            Self::OrderNotFound { .. } => ErrorCode::OrderNotFound,
            Self::PositionNotFound { .. } => ErrorCode::PositionNotFound,
            Self::SymbolNotFound { .. } => ErrorCode::SymbolNotFound,
            Self::OcoGroupNotFound { .. } => ErrorCode::OcoGroupNotFound,
            Self::OrderNotModifiable { .. } => ErrorCode::OrderNotModifiable,
            Self::ResetCooldown { .. } => ErrorCode::ResetCooldown,
            Self::OrderRejected { .. } => ErrorCode::OrderRejected,
            Self::RateLimited { .. } => ErrorCode::RateLimited,
            Self::Internal { .. } => ErrorCode::InternalError,
            Self::UnsupportedOperation { .. } => ErrorCode::UnsupportedOperation,
            Self::ProviderError { .. } => ErrorCode::ProviderError,
            Self::ShuttingDown => ErrorCode::ShuttingDown,
            Self::ProviderUnavailable { .. } => ErrorCode::ProviderUnavailable,
        }
    }

    /// Convert to API error response body.
    #[must_use]
    pub fn to_api_error(&self) -> ApiError {
        let details = match self {
            Self::InvalidRequest { field, .. } => field.as_ref().map(|f| json!({ "field": f })),

            Self::InvalidValue {
                field, provided, ..
            } => Some(json!({
                "field": field,
                "provided": provided,
            })),

            Self::ValidationError(errors) => Some(json!(errors)),

            Self::OrderNotFound { id } => Some(json!({ "resource": "order", "id": id })),

            Self::PositionNotFound { id } => Some(json!({ "resource": "position", "id": id })),

            Self::SymbolNotFound { symbol } => Some(json!({ "symbol": symbol })),

            Self::OcoGroupNotFound { id } => Some(json!({ "resource": "oco_group", "id": id })),

            Self::OrderNotModifiable { order_id, .. } => Some(json!({ "order_id": order_id })),

            Self::OrderRejected {
                reject_code,
                details,
                ..
            } => {
                let mut d = details.clone().unwrap_or_else(|| json!({}));
                if let Some(code) = reject_code
                    && let Some(obj) = d.as_object_mut()
                {
                    obj.insert("reject_code".to_string(), json!(code));
                }
                Some(d)
            }

            Self::RateLimited {
                retry_after_seconds,
                reset_at,
            } => Some(json!({
                "retry_after_seconds": retry_after_seconds,
                "reset_at": reset_at,
            })),

            Self::ProviderError { provider, .. } => {
                provider.as_ref().map(|p| json!({ "provider": p }))
            }

            Self::Unauthorized { code, .. } => Some(json!({ "code": code })),

            Self::UnsupportedOperation {
                operation,
                provider,
            } => Some(json!({
                "operation": operation,
                "provider": provider,
            })),

            Self::ResetCooldown { .. }
            | Self::ShuttingDown
            | Self::Internal { .. }
            | Self::ProviderUnavailable { .. } => None,
        };

        // Log the unscrubbed error at TRACE for operator debugging
        tracing::trace!(
            error_code = self.error_code().as_str(),
            error_message = %self,
            ?details,
            "API error response (unscrubbed)",
        );

        // Scrub credentials from client-facing fields
        let scrubbed_message = scrub_credentials(&self.to_string());
        let scrubbed_details = details.map(|mut d| {
            scrub_json_values(&mut d);
            d
        });

        ApiError {
            code: self.error_code(),
            message: scrubbed_message,
            details: scrubbed_details,
        }
    }

    /// Check if this is a wash trade / self-trade prevention error.
    #[must_use]
    pub fn is_wash_trade_error(&self) -> bool {
        match self {
            Self::OrderRejected { reason, .. } => {
                let reason_lower = reason.to_lowercase();
                reason_lower.contains("self-trade") || reason_lower.contains("wash")
            }
            _ => false,
        }
    }

    /// Check if this error is potentially retryable.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. }
                | Self::ProviderUnavailable { .. }
                | Self::ProviderError { .. }
        ) || self.is_wash_trade_error()
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = Json(self.to_api_error());
        (status, body).into_response()
    }
}

/// Convenient result type alias for gateway operations.
pub type GatewayResult<T> = Result<T, GatewayError>;

/// Canonical reject codes for `OrderRejected::reject_code`.
///
/// Adapters should use these constants when the broker provides enough
/// information to classify the rejection reason. Unlisted broker-specific
/// codes can still be passed through as free-form strings.
pub mod reject_codes {
    pub const INSUFFICIENT_FUNDS: &str = "INSUFFICIENT_FUNDS";
    pub const MARKET_CLOSED: &str = "MARKET_CLOSED";
    pub const INVALID_QUANTITY: &str = "INVALID_QUANTITY";
    pub const INVALID_SYMBOL: &str = "INVALID_SYMBOL";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_credentials_redacts_api_key() {
        let input = "Failed: apiKey=sk_live_123abc something";
        let result = scrub_credentials(input);
        assert!(
            !result.contains("sk_live_123abc"),
            "API key should be redacted"
        );
        assert!(result.contains("***REDACTED***"));
    }

    #[test]
    fn scrub_credentials_redacts_api_underscore_key() {
        let input = "Error with api_key=my_secret_key_123";
        let result = scrub_credentials(input);
        assert!(!result.contains("my_secret_key_123"));
        assert!(result.contains("***REDACTED***"));
    }

    #[test]
    fn scrub_credentials_redacts_authorization_header() {
        // Single-token value: fully redacted
        let input = "Authorization=eyJhbGciOiJIUzI1NiJ9";
        let result = scrub_credentials(input);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(result.contains("***REDACTED***"));
    }

    #[test]
    fn scrub_credentials_redacts_token_equals() {
        let input = "token=abc123def456 remaining";
        let result = scrub_credentials(input);
        assert!(!result.contains("abc123def456"));
    }

    #[test]
    fn scrub_credentials_preserves_normal_text() {
        let input = "Order not found: 12345";
        let result = scrub_credentials(input);
        assert_eq!(result, input);
    }

    #[test]
    fn scrub_json_values_redacts_nested_strings() {
        let mut value = json!({
            "error": "apiKey=secret123 in URL",
            "nested": {
                "msg": "password: hunter2"
            }
        });
        scrub_json_values(&mut value);
        let error = value["error"].as_str().unwrap();
        assert!(!error.contains("secret123"));
        let msg = value["nested"]["msg"].as_str().unwrap();
        assert!(!msg.contains("hunter2"));
    }

    #[test]
    fn scrub_json_values_redacts_credential_keys() {
        let mut value = json!({
            "api_key": "sk_live_abc123",
            "message": "normal text"
        });
        scrub_json_values(&mut value);
        assert_eq!(value["api_key"], "***REDACTED***");
        assert_eq!(value["message"], "normal text");
    }

    #[test]
    fn scrub_json_values_redacts_in_arrays() {
        let mut value = json!(["token=xyz123", "normal"]);
        scrub_json_values(&mut value);
        let first = value[0].as_str().unwrap();
        assert!(!first.contains("xyz123"));
        assert_eq!(value[1], "normal");
    }

    #[test]
    fn to_api_error_scrubs_provider_error_message() {
        let error = GatewayError::ProviderError {
            message: "Request failed: apiKey=sk_live_secret123 in query".to_string(),
            provider: Some("binance".to_string()),
            source: None,
        };
        let api_error = error.to_api_error();
        assert!(!api_error.message.contains("sk_live_secret123"));
        assert!(api_error.message.contains("***REDACTED***"));
    }

    #[test]
    fn to_api_error_scrubs_order_rejected_details() {
        let error = GatewayError::OrderRejected {
            reason: "Rejected".to_string(),
            reject_code: None,
            details: Some(json!({
                "url": "https://api.example.com?apiKey=secret_value",
                "authorization": "Bearer token123"
            })),
        };
        let api_error = error.to_api_error();
        let details = api_error.details.unwrap();
        let url = details["url"].as_str().unwrap();
        assert!(!url.contains("secret_value"));
        assert_eq!(details["authorization"], "***REDACTED***");
    }

    #[test]
    fn error_code_serializes_to_screaming_snake_case() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::InvalidRequest).unwrap(),
            r#""INVALID_REQUEST""#
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::InternalError).unwrap(),
            r#""INTERNAL_ERROR""#
        );
    }

    #[test]
    fn error_code_deserializes_from_screaming_snake_case() {
        let code: ErrorCode = serde_json::from_str(r#""ORDER_NOT_FOUND""#).unwrap();
        assert_eq!(code, ErrorCode::OrderNotFound);
    }

    #[test]
    fn error_code_display_matches_serialization() {
        assert_eq!(ErrorCode::RateLimited.to_string(), "RATE_LIMITED");
        assert_eq!(ErrorCode::ShuttingDown.to_string(), "SHUTTING_DOWN");
    }

    #[test]
    fn api_error_code_field_serializes_correctly() {
        let api_error = ApiError {
            code: ErrorCode::OrderNotFound,
            message: "Not found".to_string(),
            details: None,
        };
        let json = serde_json::to_value(&api_error).unwrap();
        assert_eq!(json["code"], "ORDER_NOT_FOUND");
    }

    // --- Error envelope format tests (6.4.1) ---

    fn check_envelope(err: &GatewayError) -> (StatusCode, serde_json::Value) {
        let status = err.status_code();
        let api_error = err.to_api_error();
        let json = serde_json::to_value(&api_error).unwrap();
        (status, json)
    }

    #[test]
    fn envelope_invalid_request_with_field() {
        let err = GatewayError::InvalidRequest {
            message: "bad symbol format".into(),
            field: Some("symbol".into()),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "INVALID_REQUEST");
        assert!(!json["message"].as_str().unwrap().is_empty());
        assert_eq!(json["details"]["field"], "symbol");
    }

    #[test]
    fn envelope_invalid_request_without_field() {
        let err = GatewayError::InvalidRequest {
            message: "bad request".into(),
            field: None,
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "INVALID_REQUEST");
        assert!(json.get("details").is_none() || json["details"].is_null());
    }

    #[test]
    fn envelope_invalid_value_with_provided() {
        let err = GatewayError::InvalidValue {
            field: "quantity".into(),
            message: "must be positive".into(),
            provided: Some("abc".into()),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "INVALID_VALUE");
        assert_eq!(json["details"]["field"], "quantity");
        assert_eq!(json["details"]["provided"], "abc");
    }

    #[test]
    fn envelope_invalid_value_without_provided() {
        let err = GatewayError::InvalidValue {
            field: "quantity".into(),
            message: "must be positive".into(),
            provided: None,
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "INVALID_VALUE");
        assert_eq!(json["details"]["field"], "quantity");
        assert!(json["details"]["provided"].is_null());
    }

    #[test]
    fn envelope_validation_error() {
        let mut errors = validator::ValidationErrors::new();
        errors.add("email", validator::ValidationError::new("invalid_email"));
        let err = GatewayError::ValidationError(errors);
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "VALIDATION_ERROR");
        assert!(json["details"]["email"].is_array());
    }

    #[test]
    fn envelope_unauthorized() {
        let err = GatewayError::Unauthorized {
            reason: "token expired".into(),
            code: "AUTH_FAILED".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["code"], "UNAUTHORIZED");
        assert_eq!(json["details"]["code"], "AUTH_FAILED");
    }

    #[test]
    fn envelope_order_not_found() {
        let err = GatewayError::OrderNotFound {
            id: "ord_123".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], "ORDER_NOT_FOUND");
        assert_eq!(json["details"]["resource"], "order");
        assert_eq!(json["details"]["id"], "ord_123");
    }

    #[test]
    fn envelope_position_not_found() {
        let err = GatewayError::PositionNotFound {
            id: "pos_456".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], "POSITION_NOT_FOUND");
        assert_eq!(json["details"]["resource"], "position");
        assert_eq!(json["details"]["id"], "pos_456");
    }

    #[test]
    fn envelope_symbol_not_found() {
        let err = GatewayError::SymbolNotFound {
            symbol: "AAPL".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], "SYMBOL_NOT_FOUND");
        assert_eq!(json["details"]["symbol"], "AAPL");
    }

    #[test]
    fn envelope_oco_group_not_found() {
        let err = GatewayError::OcoGroupNotFound {
            id: "oco_789".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], "OCO_GROUP_NOT_FOUND");
        assert_eq!(json["details"]["resource"], "oco_group");
        assert_eq!(json["details"]["id"], "oco_789");
    }

    #[test]
    fn envelope_order_not_modifiable() {
        let err = GatewayError::OrderNotModifiable {
            order_id: "ord_123".into(),
            reason: "already filled".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["code"], "ORDER_NOT_MODIFIABLE");
        assert_eq!(json["details"]["order_id"], "ord_123");
    }

    #[test]
    fn envelope_order_rejected_with_code_and_details() {
        let err = GatewayError::OrderRejected {
            reason: "insufficient funds".into(),
            reject_code: Some("INSUFFICIENT_FUNDS".into()),
            details: Some(json!({"extra": "data"})),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json["code"], "ORDER_REJECTED");
        assert_eq!(json["details"]["reject_code"], "INSUFFICIENT_FUNDS");
        assert_eq!(json["details"]["extra"], "data");
    }

    #[test]
    fn envelope_order_rejected_without_code_or_details() {
        let err = GatewayError::OrderRejected {
            reason: "rejected".into(),
            reject_code: None,
            details: None,
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json["code"], "ORDER_REJECTED");
        // details is {} (empty object from unwrap_or(json!({})))
        assert!(json["details"].is_object());
        assert!(json["details"].as_object().unwrap().is_empty());
    }

    #[test]
    fn envelope_rate_limited_with_fields() {
        let reset = chrono::DateTime::parse_from_rfc3339("2026-03-24T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let err = GatewayError::RateLimited {
            retry_after_seconds: Some(60),
            reset_at: Some(reset),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(json["code"], "RATE_LIMITED");
        assert_eq!(json["details"]["retry_after_seconds"], 60);
        assert!(
            json["details"]["reset_at"]
                .as_str()
                .unwrap()
                .contains("2026-03-24")
        );
    }

    #[test]
    fn envelope_rate_limited_without_fields() {
        let err = GatewayError::RateLimited {
            retry_after_seconds: None,
            reset_at: None,
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(json["code"], "RATE_LIMITED");
        assert!(json["details"]["retry_after_seconds"].is_null());
        assert!(json["details"]["reset_at"].is_null());
    }

    #[test]
    fn envelope_internal() {
        let err = GatewayError::Internal {
            message: "something broke".into(),
            source: None,
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json["code"], "INTERNAL_ERROR");
        assert!(json.get("details").is_none() || json["details"].is_null());
    }

    #[test]
    fn envelope_unsupported_operation() {
        let err = GatewayError::UnsupportedOperation {
            operation: "shorting".into(),
            provider: "alpaca".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(json["code"], "UNSUPPORTED_OPERATION");
        assert_eq!(json["details"]["operation"], "shorting");
        assert_eq!(json["details"]["provider"], "alpaca");
    }

    #[test]
    fn envelope_provider_error_with_provider() {
        let err = GatewayError::ProviderError {
            message: "connection refused".into(),
            provider: Some("binance".into()),
            source: None,
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(json["code"], "PROVIDER_ERROR");
        assert_eq!(json["details"]["provider"], "binance");
    }

    #[test]
    fn envelope_provider_error_without_provider() {
        let err = GatewayError::ProviderError {
            message: "connection refused".into(),
            provider: None,
            source: None,
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(json["code"], "PROVIDER_ERROR");
        assert!(json.get("details").is_none() || json["details"].is_null());
    }

    #[test]
    fn envelope_shutting_down() {
        let err = GatewayError::ShuttingDown;
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["code"], "SHUTTING_DOWN");
        assert!(json.get("details").is_none() || json["details"].is_null());
    }

    #[test]
    fn envelope_provider_unavailable() {
        let err = GatewayError::ProviderUnavailable {
            message: "broker down".into(),
        };
        let (status, json) = check_envelope(&err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["code"], "PROVIDER_UNAVAILABLE");
        assert!(json.get("details").is_none() || json["details"].is_null());
    }

    // --- Provider error passthrough tests (6.4.4) ---

    #[test]
    fn provider_error_preserves_adapter_context_in_json() {
        let err = GatewayError::ProviderError {
            message: "HTTP 500: Internal error; check broker status".into(),
            provider: Some("alpaca".into()),
            source: None,
        };
        let api_error = err.to_api_error();
        assert_eq!(api_error.code, ErrorCode::ProviderError);
        assert!(api_error.message.contains("HTTP 500"));
        assert!(api_error.message.contains("check broker status"));
        let details = api_error.details.unwrap();
        assert_eq!(details["provider"], "alpaca");
    }

    #[test]
    fn order_rejected_preserves_binance_details_in_json() {
        // Simulate a Binance -2010 rejection with body preserved in details
        let err = GatewayError::OrderRejected {
            reason: "Account has insufficient balance".into(),
            reject_code: Some("INSUFFICIENT_FUNDS".into()),
            details: Some(json!({
                "code": -2010,
                "msg": "Account has insufficient balance for requested action."
            })),
        };
        let api_error = err.to_api_error();
        assert_eq!(api_error.code, ErrorCode::OrderRejected);
        let details = api_error.details.unwrap();
        assert_eq!(details["reject_code"], "INSUFFICIENT_FUNDS");
        // Original Binance fields preserved alongside reject_code
        assert_eq!(details["code"], -2010);
        assert_eq!(
            details["msg"],
            "Account has insufficient balance for requested action."
        );
    }

    #[test]
    fn provider_error_source_not_in_json() {
        let source_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = GatewayError::ProviderError {
            message: "Connection refused".into(),
            provider: Some("saxo".into()),
            source: Some(Box::new(source_err)),
        };
        let api_error = err.to_api_error();
        let json = serde_json::to_value(&api_error).unwrap();
        // source must not leak into the JSON envelope
        assert!(json.get("source").is_none());
        let details = json.get("details").unwrap();
        assert!(details.get("source").is_none());
        // But provider is still there
        assert_eq!(details["provider"], "saxo");
    }
}
