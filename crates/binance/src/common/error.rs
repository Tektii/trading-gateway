//! Binance error mapping utilities.
//!
//! Maps Binance API error responses to `GatewayError` types.
//! Used by all Binance product adapters (Spot, Futures, Margin, etc.).

use reqwest::StatusCode;
use tektii_gateway_core::error::{GatewayError, reject_codes};
use tektii_gateway_core::http::{default_error_mapper, extract_message};

/// Map a Binance `-2010` rejection message to a canonical reject code.
///
/// Binance reuses `-2010` for many rejection reasons — the `msg` field
/// distinguishes them. We parse known patterns; unknown messages fall
/// back to `"-2010"`.
fn reject_code_from_2010_msg(msg: &str) -> &'static str {
    let lower = msg.to_lowercase();
    if lower.contains("insufficient balance") {
        reject_codes::INSUFFICIENT_FUNDS
    } else if lower.contains("market is closed") {
        reject_codes::MARKET_CLOSED
    } else {
        // Fall back to the raw Binance code so clients still see something
        "-2010"
    }
}

/// Binance-specific error mapper for HTTP responses.
///
/// Handles Binance's convention of returning HTTP 400 with specific error codes
/// in the body for semantic errors (order rejection, not found, etc.).
///
/// # Error Codes
///
/// - `-1013`: Filter failure (`LOT_SIZE`, `MIN_NOTIONAL`, etc.)
/// - `-1015`: Too many orders / rate limited
/// - `-1121`: Invalid symbol
/// - `-2010`: Order rejected (insufficient balance, market closed, etc.)
/// - `-2013`: Order does not exist
/// - `-2014`/`-2015`: Invalid API key
pub fn binance_error_mapper(
    status: StatusCode,
    body: serde_json::Value,
    retry_after: Option<u64>,
    provider: &str,
) -> GatewayError {
    // Handle Binance-specific HTTP 400 error codes
    if status == StatusCode::BAD_REQUEST
        && let Some(code) = body.get("code").and_then(serde_json::Value::as_i64)
    {
        let msg = extract_message(&body);
        return match code {
            // -1013: Filter failure (LOT_SIZE, MIN_NOTIONAL, etc.)
            -1013 => GatewayError::OrderRejected {
                reason: msg,
                reject_code: Some(reject_codes::INVALID_QUANTITY.to_string()),
                details: Some(body),
            },
            // -1015: Too many orders
            -1015 => GatewayError::RateLimited {
                retry_after_seconds: retry_after,
                reset_at: None,
            },
            // -1121: Invalid symbol
            -1121 => GatewayError::InvalidRequest {
                message: msg,
                field: Some("symbol".to_string()),
            },
            // -2010: Order rejected — parse msg for specific reason
            -2010 => {
                let reject_code = reject_code_from_2010_msg(&msg);
                GatewayError::OrderRejected {
                    reason: msg,
                    reject_code: Some(reject_code.to_string()),
                    details: Some(body),
                }
            }
            // -2013: Order does not exist
            -2013 => GatewayError::OrderNotFound {
                id: "unknown".to_string(),
            },
            // -2014/-2015: Invalid API key
            -2014 | -2015 => GatewayError::Unauthorized {
                reason: msg,
                code: code.to_string(),
            },
            // Other Binance error codes - fall through to default
            _ => default_error_mapper(status, body, retry_after, provider),
        };
    }

    // All other status codes use the default mapper
    default_error_mapper(status, body, retry_after, provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -2010 with message parsing
    #[test]
    fn maps_2010_insufficient_balance_to_canonical_code() {
        let body = json!({
            "code": -2010,
            "msg": "Account has insufficient balance for requested action."
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        match error {
            GatewayError::OrderRejected {
                reject_code,
                reason,
                ..
            } => {
                assert_eq!(
                    reject_code.as_deref(),
                    Some(reject_codes::INSUFFICIENT_FUNDS)
                );
                assert!(reason.contains("insufficient balance"));
            }
            _ => panic!("Expected OrderRejected, got {error:?}"),
        }
    }

    #[test]
    fn maps_2010_market_closed_to_canonical_code() {
        let body = json!({
            "code": -2010,
            "msg": "Market is closed."
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        match error {
            GatewayError::OrderRejected { reject_code, .. } => {
                assert_eq!(reject_code.as_deref(), Some(reject_codes::MARKET_CLOSED));
            }
            _ => panic!("Expected OrderRejected, got {error:?}"),
        }
    }

    #[test]
    fn maps_2010_unknown_msg_to_raw_code() {
        let body = json!({
            "code": -2010,
            "msg": "Some other rejection reason"
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        match error {
            GatewayError::OrderRejected { reject_code, .. } => {
                assert_eq!(reject_code.as_deref(), Some("-2010"));
            }
            _ => panic!("Expected OrderRejected, got {error:?}"),
        }
    }

    // -1013: filter failure
    #[test]
    fn maps_1013_to_invalid_quantity() {
        let body = json!({
            "code": -1013,
            "msg": "Filter failure: LOT_SIZE"
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        match error {
            GatewayError::OrderRejected { reject_code, .. } => {
                assert_eq!(reject_code.as_deref(), Some(reject_codes::INVALID_QUANTITY));
            }
            _ => panic!("Expected OrderRejected, got {error:?}"),
        }
    }

    // -1015: rate limited
    #[test]
    fn maps_1015_to_rate_limited() {
        let body = json!({
            "code": -1015,
            "msg": "Too many new orders."
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        assert!(matches!(error, GatewayError::RateLimited { .. }));
    }

    // -1121: invalid symbol
    #[test]
    fn maps_1121_to_invalid_request() {
        let body = json!({
            "code": -1121,
            "msg": "Invalid symbol."
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        match error {
            GatewayError::InvalidRequest { field, .. } => {
                assert_eq!(field.as_deref(), Some("symbol"));
            }
            _ => panic!("Expected InvalidRequest, got {error:?}"),
        }
    }

    // Existing tests (preserved)
    #[test]
    fn maps_order_not_found_error() {
        let body = json!({
            "code": -2013,
            "msg": "Order does not exist"
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        assert!(matches!(error, GatewayError::OrderNotFound { .. }));
    }

    #[test]
    fn maps_unauthorized_error() {
        let body = json!({
            "code": -2015,
            "msg": "Invalid API-key"
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        match error {
            GatewayError::Unauthorized { reason, code } => {
                assert_eq!(reason, "Invalid API-key");
                assert_eq!(code, "-2015");
            }
            _ => panic!("Expected Unauthorized, got {error:?}"),
        }
    }

    #[test]
    fn falls_back_to_default_for_unknown_codes() {
        let body = json!({
            "code": -9999,
            "msg": "Unknown error"
        });

        let error = binance_error_mapper(StatusCode::BAD_REQUEST, body, None, "binance");

        assert!(matches!(error, GatewayError::InvalidRequest { .. }));
    }

    #[test]
    fn falls_back_to_default_for_non_400() {
        let body = json!({
            "msg": "Service unavailable"
        });

        let error = binance_error_mapper(
            StatusCode::SERVICE_UNAVAILABLE,
            body,
            None,
            "binance-futures",
        );

        assert!(matches!(error, GatewayError::ProviderUnavailable { .. }));
    }
}
