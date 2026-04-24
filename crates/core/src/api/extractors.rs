//! Custom extractors for the gateway API routes.
//!
//! This module provides:
//! - [`ValidatedJson`] - Extracts and validates JSON request bodies
//! - [`OptionalValidatedJson`] - Extracts optional JSON request bodies

use axum::{
    Json,
    body::Body,
    extract::{FromRequest, Request},
};
use serde::de::DeserializeOwned;
use validator::Validate;

use crate::error::GatewayError;

/// Extractor that deserializes JSON and validates it.
///
/// This combines Axum's `Json` extractor with the `validator` crate.
/// If deserialization or validation fails, returns an appropriate error.
///
/// # Example
///
/// ```ignore
/// use tektii_gateway_core::api::extractors::ValidatedJson;
/// use tektii_gateway_core::models::OrderRequest;
///
/// async fn submit_order(
///     ValidatedJson(order): ValidatedJson<OrderRequest>,
/// ) -> Result<Json<OrderHandle>, GatewayError> {
///     // order is already validated
/// }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidatedJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
{
    type Rejection = GatewayError;

    async fn from_request(req: Request<Body>, state: &S) -> Result<Self, Self::Rejection> {
        // First, try to deserialize the JSON
        let Json(value) = Json::<T>::from_request(req, state).await.map_err(|e| {
            GatewayError::InvalidRequest {
                message: format!("Invalid JSON: {e}"),
                field: None,
            }
        })?;

        // Then validate
        value.validate().map_err(GatewayError::ValidationError)?;

        Ok(Self(value))
    }
}

/// Extractor for optional JSON body.
///
/// Similar to `ValidatedJson` but returns `None` for empty bodies
/// instead of an error.
///
/// # Example
///
/// ```ignore
/// use tektii_gateway_core::api::extractors::OptionalValidatedJson;
/// use tektii_gateway_core::models::ClosePositionRequest;
///
/// async fn close_position(
///     OptionalValidatedJson(request): OptionalValidatedJson<ClosePositionRequest>,
/// ) -> Result<Json<OrderHandle>, GatewayError> {
///     // request is Option<ClosePositionRequest>
/// }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct OptionalValidatedJson<T>(pub Option<T>);

impl<S, T> FromRequest<S> for OptionalValidatedJson<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
{
    type Rejection = GatewayError;

    async fn from_request(req: Request<Body>, state: &S) -> Result<Self, Self::Rejection> {
        // No Content-Type header → caller didn't send a body. Skip the inner
        // Json extractor (which would reject with "missing Content-Type").
        // No Content-Length, or Content-Length == 0 → no body either.
        let has_content_type = req.headers().contains_key(axum::http::header::CONTENT_TYPE);
        let content_length = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok());

        if !has_content_type || content_length == Some(0) {
            return Ok(Self(None));
        }

        // Try to extract and validate JSON
        match ValidatedJson::<T>::from_request(req, state).await {
            Ok(ValidatedJson(value)) => Ok(Self(Some(value))),
            // Empty body or null is fine
            Err(GatewayError::InvalidRequest { ref message, .. })
                if message.contains("EOF") || message.contains("empty") =>
            {
                Ok(Self(None))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Method, Request as HttpRequest};
    use serde::Deserialize;
    use validator::Validate;

    #[derive(Debug, Deserialize, Validate)]
    struct Body {
        #[validate(length(min = 1))]
        name: String,
    }

    fn empty_request() -> Request<axum::body::Body> {
        HttpRequest::builder()
            .method(Method::DELETE)
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn optional_json_returns_none_when_no_content_type() {
        // DELETE with no body and no Content-Type — common SDK shape — must
        // not be rejected as "missing Content-Type".
        let req = empty_request();
        let extracted = OptionalValidatedJson::<Body>::from_request(req, &()).await;
        match extracted {
            Ok(OptionalValidatedJson(None)) => {}
            other => panic!("expected None, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn optional_json_returns_none_when_content_length_zero() {
        let req = HttpRequest::builder()
            .method(Method::DELETE)
            .uri("/")
            .header("content-type", "application/json")
            .header("content-length", "0")
            .body(axum::body::Body::empty())
            .unwrap();
        let extracted = OptionalValidatedJson::<Body>::from_request(req, &()).await;
        match extracted {
            Ok(OptionalValidatedJson(None)) => {}
            other => panic!("expected None, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn optional_json_parses_body_when_present() {
        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"name":"hello"}"#))
            .unwrap();
        let extracted = OptionalValidatedJson::<Body>::from_request(req, &()).await;
        match extracted {
            Ok(OptionalValidatedJson(Some(b))) => assert_eq!(b.name, "hello"),
            other => panic!("expected Some, got: {other:?}"),
        }
    }
}
