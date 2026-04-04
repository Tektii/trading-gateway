//! Saxo Bank error types and conversions to `GatewayError`.

use tektii_gateway_core::error::{GatewayError, reject_codes};

/// Saxo Bank API error.
#[derive(Debug, thiserror::Error)]
pub enum SaxoError {
    /// Network or HTTP transport error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON deserialization failure.
    #[error("Deserialization error: {0}")]
    Deserialization(#[from] serde_json::Error),

    /// Authentication failed (invalid credentials, revoked token, etc.).
    #[error("Auth failed: {message}")]
    AuthFailed { message: String },

    /// Token refresh exchange failed.
    #[error("Token refresh failed: {message}")]
    TokenRefreshFailed { message: String },

    /// Token expired and no refresh token available.
    #[error("Token expired, no refresh token available")]
    TokenExpired,

    /// Rate limited by Saxo API (HTTP 429).
    #[error("Rate limited, retry after {retry_after_seconds}s")]
    RateLimited { retry_after_seconds: u64 },

    /// Saxo API returned an error response.
    #[error("API error (HTTP {status}): {message}")]
    ApiError {
        status: u16,
        message: String,
        error_code: Option<String>,
    },

    /// Order rejected by Saxo.
    #[error("Order rejected: {reason}")]
    OrderRejected {
        reason: String,
        error_code: Option<String>,
    },

    /// Instrument not found (UIC lookup miss).
    #[error("Instrument not found: {0}")]
    InstrumentNotFound(String),

    /// Configuration error.
    #[error("Config error: {0}")]
    Config(String),

    /// Invalid binary streaming frame.
    #[error("Invalid streaming frame: {0}")]
    InvalidFrame(String),

    /// Unsupported payload format byte in streaming frame.
    #[error("Unsupported payload format byte: {0}")]
    UnsupportedPayloadFormat(u8),

    /// Delta applied for a reference ID with no stored snapshot.
    #[error("No snapshot for reference ID: {0}")]
    NoSnapshotForRef(String),

    /// WebSocket connection or streaming error.
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// Subscription creation failed via REST.
    #[error("Subscription failed for {reference_id}: {message}")]
    SubscriptionFailed {
        reference_id: String,
        message: String,
    },
}

/// Map a Saxo error code to a canonical reject code.
///
/// Known Saxo codes are mapped to `reject_codes::*` constants.
/// Unknown codes pass through as-is.
pub(crate) fn map_saxo_reject_code(code: Option<&str>) -> &str {
    match code {
        Some("InsufficientMargin") => reject_codes::INSUFFICIENT_FUNDS,
        Some(other) => other,
        None => "ORDER_REJECTED",
    }
}

impl From<SaxoError> for GatewayError {
    fn from(err: SaxoError) -> Self {
        match err {
            SaxoError::Http(e) => Self::ProviderError {
                message: e.to_string(),
                provider: Some("saxo".to_string()),
                source: Some(Box::new(e)),
            },
            SaxoError::Deserialization(e) => Self::ProviderError {
                message: format!("Failed to parse Saxo response: {e}"),
                provider: Some("saxo".to_string()),
                source: Some(Box::new(e)),
            },
            SaxoError::AuthFailed { message } | SaxoError::TokenRefreshFailed { message } => {
                Self::Unauthorized {
                    reason: message,
                    code: "SAXO_AUTH_FAILED".to_string(),
                }
            }
            SaxoError::TokenExpired => Self::Unauthorized {
                reason: "Token expired, no refresh token available".to_string(),
                code: "SAXO_TOKEN_EXPIRED".to_string(),
            },
            SaxoError::RateLimited {
                retry_after_seconds,
            } => Self::RateLimited {
                retry_after_seconds: Some(retry_after_seconds),
                reset_at: None,
            },
            SaxoError::ApiError {
                status,
                message,
                error_code,
            } => {
                if status == 503 {
                    Self::ProviderUnavailable {
                        message: format!("Saxo API error ({status}): {message}"),
                    }
                } else if status >= 500 {
                    Self::ProviderError {
                        message: format!("Saxo API error ({status}): {message}"),
                        provider: Some("saxo".to_string()),
                        source: None,
                    }
                } else {
                    Self::InvalidRequest {
                        message: format!(
                            "Saxo API error ({status}): {message}{}",
                            error_code
                                .map(|c| format!(" [code: {c}]"))
                                .unwrap_or_default()
                        ),
                        field: None,
                    }
                }
            }
            SaxoError::OrderRejected {
                reason, error_code, ..
            } => Self::OrderRejected {
                reason,
                reject_code: Some(map_saxo_reject_code(error_code.as_deref()).to_string()),
                details: None,
            },
            SaxoError::InstrumentNotFound(symbol) => Self::SymbolNotFound { symbol },
            SaxoError::Config(msg) => Self::internal(msg),
            SaxoError::InvalidFrame(msg) => Self::ProviderError {
                message: format!("Invalid Saxo streaming frame: {msg}"),
                provider: Some("saxo".to_string()),
                source: None,
            },
            SaxoError::UnsupportedPayloadFormat(byte) => Self::ProviderError {
                message: format!("Unsupported Saxo payload format byte: {byte}"),
                provider: Some("saxo".to_string()),
                source: None,
            },
            SaxoError::NoSnapshotForRef(ref_id) => {
                Self::internal(format!("No snapshot for Saxo subscription: {ref_id}"))
            }
            SaxoError::WebSocket(msg) => Self::ProviderError {
                message: format!("Saxo WebSocket error: {msg}"),
                provider: Some("saxo".to_string()),
                source: None,
            },
            SaxoError::SubscriptionFailed {
                reference_id,
                message,
            } => Self::ProviderError {
                message: format!("Saxo subscription failed for {reference_id}: {message}"),
                provider: Some("saxo".to_string()),
                source: None,
            },
        }
    }
}

/// Helper to create a `GatewayError` from a `SaxoError` reference for circuit breaker recording.
impl SaxoError {
    pub(crate) fn to_gateway_error_ref(err: &Self) -> GatewayError {
        GatewayError::ProviderError {
            message: err.to_string(),
            provider: Some("saxo".to_string()),
            source: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_failed_maps_to_unauthorized() {
        let err: GatewayError = SaxoError::AuthFailed {
            message: "Invalid token".to_string(),
        }
        .into();
        assert!(matches!(err, GatewayError::Unauthorized { .. }));
    }

    #[test]
    fn rate_limited_maps_correctly() {
        let err: GatewayError = SaxoError::RateLimited {
            retry_after_seconds: 30,
        }
        .into();
        match err {
            GatewayError::RateLimited {
                retry_after_seconds,
                ..
            } => {
                assert_eq!(retry_after_seconds, Some(30));
            }
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }

    #[test]
    fn api_error_500_maps_to_provider_error() {
        let err: GatewayError = SaxoError::ApiError {
            status: 500,
            message: "Internal Server Error".to_string(),
            error_code: None,
        }
        .into();
        assert!(matches!(err, GatewayError::ProviderError { .. }));
    }

    #[test]
    fn api_error_503_maps_to_provider_unavailable() {
        let err: GatewayError = SaxoError::ApiError {
            status: 503,
            message: "Service Unavailable".to_string(),
            error_code: None,
        }
        .into();
        assert!(matches!(err, GatewayError::ProviderUnavailable { .. }));
    }

    #[test]
    fn order_rejected_maps_to_order_rejected() {
        let err: GatewayError = SaxoError::OrderRejected {
            reason: "Insufficient margin".to_string(),
            error_code: Some("InsufficientMargin".to_string()),
        }
        .into();
        match err {
            GatewayError::OrderRejected {
                reason,
                reject_code,
                ..
            } => {
                assert_eq!(reason, "Insufficient margin");
                assert_eq!(
                    reject_code.as_deref(),
                    Some(reject_codes::INSUFFICIENT_FUNDS)
                );
            }
            other => panic!("Expected OrderRejected, got: {other:?}"),
        }
    }

    #[test]
    fn token_expired_maps_to_unauthorized() {
        let err: GatewayError = SaxoError::TokenExpired.into();
        assert!(matches!(err, GatewayError::Unauthorized { .. }));
    }

    #[test]
    fn websocket_error_maps_to_provider_error() {
        let err: GatewayError = SaxoError::WebSocket("connection reset".to_string()).into();
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("connection reset"), "got: {message}");
                assert_eq!(provider.as_deref(), Some("saxo"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    // =====================================================================
    // map_saxo_reject_code — remaining branches
    // =====================================================================

    #[test]
    fn reject_code_unknown_passthrough() {
        assert_eq!(map_saxo_reject_code(Some("FooBar")), "FooBar");
    }

    #[test]
    fn reject_code_none_returns_order_rejected() {
        assert_eq!(map_saxo_reject_code(None), "ORDER_REJECTED");
    }

    // =====================================================================
    // SaxoError → GatewayError — remaining conversion paths
    // =====================================================================

    #[test]
    fn api_error_below_500_maps_to_invalid_request_with_code() {
        let err: GatewayError = SaxoError::ApiError {
            status: 400,
            message: "Bad field".to_string(),
            error_code: Some("BadField".to_string()),
        }
        .into();
        match err {
            GatewayError::InvalidRequest { message, .. } => {
                assert!(message.contains("[code: BadField]"), "got: {message}");
            }
            other => panic!("Expected InvalidRequest, got: {other:?}"),
        }
    }

    #[test]
    fn api_error_below_500_no_error_code() {
        let err: GatewayError = SaxoError::ApiError {
            status: 422,
            message: "Unprocessable".to_string(),
            error_code: None,
        }
        .into();
        match err {
            GatewayError::InvalidRequest { message, .. } => {
                assert!(!message.contains("[code:"), "got: {message}");
            }
            other => panic!("Expected InvalidRequest, got: {other:?}"),
        }
    }

    #[test]
    fn instrument_not_found_maps_to_symbol_not_found() {
        let err: GatewayError = SaxoError::InstrumentNotFound("AAPL".to_string()).into();
        match err {
            GatewayError::SymbolNotFound { symbol } => {
                assert_eq!(symbol, "AAPL");
            }
            other => panic!("Expected SymbolNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn config_error_maps_to_internal() {
        let err: GatewayError = SaxoError::Config("bad config".to_string()).into();
        assert!(
            matches!(err, GatewayError::Internal { .. }),
            "Expected Internal, got: {err:?}"
        );
    }

    #[test]
    fn deserialization_maps_to_provider_error() {
        let json_err = serde_json::from_str::<String>("not json").unwrap_err();
        let err: GatewayError = SaxoError::Deserialization(json_err).into();
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("Failed to parse Saxo"), "got: {message}");
                assert_eq!(provider.as_deref(), Some("saxo"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn token_refresh_failed_maps_to_unauthorized() {
        let err: GatewayError = SaxoError::TokenRefreshFailed {
            message: "expired".to_string(),
        }
        .into();
        match err {
            GatewayError::Unauthorized { reason, code } => {
                assert_eq!(reason, "expired");
                assert_eq!(code, "SAXO_AUTH_FAILED");
            }
            other => panic!("Expected Unauthorized, got: {other:?}"),
        }
    }

    #[test]
    fn order_rejected_unknown_code_passthrough() {
        let err: GatewayError = SaxoError::OrderRejected {
            reason: "Some reason".to_string(),
            error_code: Some("UnknownCode".to_string()),
        }
        .into();
        match err {
            GatewayError::OrderRejected { reject_code, .. } => {
                assert_eq!(reject_code.as_deref(), Some("UnknownCode"));
            }
            other => panic!("Expected OrderRejected, got: {other:?}"),
        }
    }

    #[test]
    fn order_rejected_no_code() {
        let err: GatewayError = SaxoError::OrderRejected {
            reason: "Rejected".to_string(),
            error_code: None,
        }
        .into();
        match err {
            GatewayError::OrderRejected { reject_code, .. } => {
                assert_eq!(reject_code.as_deref(), Some("ORDER_REJECTED"));
            }
            other => panic!("Expected OrderRejected, got: {other:?}"),
        }
    }

    #[test]
    fn subscription_failed_maps_to_provider_error() {
        let err: GatewayError = SaxoError::SubscriptionFailed {
            reference_id: "ref-123".to_string(),
            message: "timeout".to_string(),
        }
        .into();
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("ref-123"), "got: {message}");
                assert_eq!(provider.as_deref(), Some("saxo"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn invalid_frame_maps_to_provider_error() {
        let err: GatewayError = SaxoError::InvalidFrame("bad frame".to_string()).into();
        match err {
            GatewayError::ProviderError { message, .. } => {
                assert!(
                    message.contains("Invalid Saxo streaming frame"),
                    "got: {message}"
                );
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }
}
