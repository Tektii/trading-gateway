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
        // Check content-length header
        let has_body = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .is_none_or(|len| len > 0);

        if !has_body {
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
