//! Authenticated HTTP client for the Saxo Bank OpenAPI.
//!
//! Wraps `reqwest::Client` with:
//! - Automatic Bearer token injection from [`SaxoAuth`]
//! - 401 retry with token refresh (once)
//! - 429 rate limit detection with `Retry-After` parsing
//! - Saxo error body parsing for 4xx/5xx responses

use std::sync::Arc;

use reqwest::header::{AUTHORIZATION, RETRY_AFTER};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::debug;

use super::auth::SaxoAuth;
use super::error::SaxoError;
use crate::credentials::SaxoCredentials;
use tektii_gateway_core::models::TradingPlatform;

/// Base URL for Saxo SIM environment.
const SAXO_SIM_REST_URL: &str = "https://gateway.saxobank.com/sim/openapi";
/// Base URL for Saxo LIVE environment.
const SAXO_LIVE_REST_URL: &str = "https://gateway.saxobank.com/openapi";

/// Authenticated HTTP client for the Saxo Bank OpenAPI.
pub struct SaxoHttpClient {
    client: reqwest::Client,
    base_url: String,
    auth: Arc<SaxoAuth>,
}

impl SaxoHttpClient {
    /// Create a new authenticated HTTP client.
    ///
    /// # Errors
    ///
    /// Returns `SaxoError::Config` if the auth manager cannot be created.
    pub fn new(
        credentials: &SaxoCredentials,
        platform: TradingPlatform,
    ) -> Result<Self, SaxoError> {
        let auth = SaxoAuth::new(credentials, platform)?;

        let base_url = credentials
            .rest_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::SaxoLive => SAXO_LIVE_REST_URL.to_string(),
                _ => SAXO_SIM_REST_URL.to_string(),
            });

        Ok(Self {
            client: reqwest::Client::new(),
            base_url,
            auth: Arc::new(auth),
        })
    }

    /// Get a reference to the auth manager (for WebSocket token access).
    pub fn auth(&self) -> &Arc<SaxoAuth> {
        &self.auth
    }

    /// Perform a GET request and deserialize the response.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, SaxoError> {
        let url = format!("{}{}", self.base_url, path);
        self.request_with_retry(reqwest::Method::GET, &url, Option::<&()>::None)
            .await
    }

    /// Perform a POST request with a JSON body and deserialize the response.
    pub async fn post<B: Serialize + Send + Sync, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, SaxoError> {
        let url = format!("{}{}", self.base_url, path);
        self.request_with_retry(reqwest::Method::POST, &url, Some(body))
            .await
    }

    /// Perform a PATCH request with a JSON body and deserialize the response.
    #[allow(dead_code)]
    pub async fn patch<B: Serialize + Send + Sync, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, SaxoError> {
        let url = format!("{}{}", self.base_url, path);
        self.request_with_retry(reqwest::Method::PATCH, &url, Some(body))
            .await
    }

    /// Perform a DELETE request.
    #[allow(dead_code)]
    pub async fn delete(&self, path: &str) -> Result<(), SaxoError> {
        let url = format!("{}{}", self.base_url, path);
        let token = self.auth.access_token().await?;

        let response = self
            .client
            .delete(&url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .map_err(SaxoError::Http)?;

        let status = response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            debug!("Saxo DELETE got 401, refreshing token and retrying");
            let new_token = self.auth.force_refresh().await?;
            let retry_response = self
                .client
                .delete(&url)
                .header(AUTHORIZATION, format!("Bearer {new_token}"))
                .send()
                .await
                .map_err(SaxoError::Http)?;

            if retry_response.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(SaxoError::AuthFailed {
                    message: "Authentication failed after token refresh".to_string(),
                });
            }

            return Self::check_error_status(retry_response).await.map(|_| ());
        }

        Self::check_error_status(response).await.map(|_| ())
    }

    /// Core request method with 401 retry logic.
    async fn request_with_retry<B: Serialize + Send + Sync, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&B>,
    ) -> Result<T, SaxoError> {
        let token = self.auth.access_token().await?;
        let response = self.send_request(&method, url, &token, body).await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            debug!(method = %method, url = %url, "Saxo got 401, refreshing token and retrying");
            let new_token = self.auth.force_refresh().await?;
            let retry_response = self.send_request(&method, url, &new_token, body).await?;

            if retry_response.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(SaxoError::AuthFailed {
                    message: "Authentication failed after token refresh".to_string(),
                });
            }

            let body_text = Self::check_error_status(retry_response).await?;
            return serde_json::from_str(&body_text).map_err(SaxoError::Deserialization);
        }

        let body_text = Self::check_error_status(response).await?;
        serde_json::from_str(&body_text).map_err(SaxoError::Deserialization)
    }

    /// Send a single HTTP request with Bearer token.
    async fn send_request<B: Serialize + Send + Sync>(
        &self,
        method: &reqwest::Method,
        url: &str,
        token: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response, SaxoError> {
        let mut builder = self
            .client
            .request(method.clone(), url)
            .header(AUTHORIZATION, format!("Bearer {token}"));

        if let Some(body) = body {
            builder = builder.json(body);
        }

        builder.send().await.map_err(SaxoError::Http)
    }

    /// Check response status and return body text, or return appropriate error.
    async fn check_error_status(response: reqwest::Response) -> Result<String, SaxoError> {
        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            return Err(SaxoError::RateLimited {
                retry_after_seconds: retry_after,
            });
        }

        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            return Ok(body);
        }

        // Try to parse Saxo error body
        if let Ok(error_body) = serde_json::from_str::<serde_json::Value>(&body) {
            let message = error_body["Message"]
                .as_str()
                .or_else(|| error_body["ErrorInfo"]["Message"].as_str())
                .unwrap_or(&body)
                .to_string();
            let error_code = error_body["ErrorCode"]
                .as_str()
                .or_else(|| error_body["ErrorInfo"]["ErrorCode"].as_str())
                .map(ToString::to_string);

            return Err(SaxoError::ApiError {
                status: status.as_u16(),
                message,
                error_code,
            });
        }

        Err(SaxoError::ApiError {
            status: status.as_u16(),
            message: body,
            error_code: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretBox;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_credentials(rest_url: &str, auth_url: &str) -> SaxoCredentials {
        SaxoCredentials {
            client_id: "test-client-id".to_string(),
            client_secret: SecretBox::new(Box::new("test-client-secret".to_string())),
            account_key: "test-account-key".to_string(),
            access_token: Some(SecretBox::new(Box::new("test-token".to_string()))),
            refresh_token: Some(SecretBox::new(Box::new("test-refresh-token".to_string()))),
            rest_url: Some(rest_url.to_string()),
            auth_url: Some(auth_url.to_string()),
            ws_url: None,
            precheck_orders: None,
        }
    }

    #[tokio::test]
    async fn get_sends_bearer_token() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/ref/v1/instruments"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"Data": []})))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = SaxoHttpClient::new(
            &test_credentials(&mock_server.uri(), "http://localhost:9999"),
            TradingPlatform::SaxoSim,
        )
        .unwrap();

        let result: serde_json::Value = client.get("/ref/v1/instruments").await.unwrap();
        assert!(result["Data"].is_array());
    }

    #[tokio::test]
    async fn returns_rate_limited_on_429() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/ref/v1/instruments"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "30")
                    .set_body_string(""),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = SaxoHttpClient::new(
            &test_credentials(&mock_server.uri(), "http://localhost:9999"),
            TradingPlatform::SaxoSim,
        )
        .unwrap();

        let result: Result<serde_json::Value, _> = client.get("/ref/v1/instruments").await;
        match result {
            Err(SaxoError::RateLimited {
                retry_after_seconds,
            }) => assert_eq!(retry_after_seconds, 30),
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }
}
