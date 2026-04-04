//! Saxo Bank OAuth2 token management with lazy refresh.
//!
//! Manages access tokens for the Saxo Bank OpenAPI. Tokens are refreshed
//! lazily (before each request when near expiry) rather than via a background task.

use super::error::SaxoError;
use super::types::{SaxoTokenErrorResponse, SaxoTokenResponse};
use crate::credentials::SaxoCredentials;
use secrecy::{ExposeSecret, SecretBox};
use tektii_gateway_core::models::TradingPlatform;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, warn};

/// Token URL for SIM environment.
const SAXO_SIM_TOKEN_URL: &str = "https://sim.logonvalidation.net";
/// Token URL for LIVE environment.
const SAXO_LIVE_TOKEN_URL: &str = "https://live.logonvalidation.net";

/// Buffer before expiry to trigger refresh (60 seconds).
const EXPIRY_BUFFER_SECS: u64 = 60;

/// Internal token state.
struct TokenState {
    access_token: SecretBox<String>,
    refresh_token: Option<SecretBox<String>>,
    expires_at: Instant,
}

/// Saxo Bank OAuth2 auth manager.
///
/// Holds token state behind a `Mutex` and refreshes lazily when the token
/// is within 60 seconds of expiry.
pub struct SaxoAuth {
    token_url: String,
    client_id: String,
    client_secret: SecretBox<String>,
    http_client: reqwest::Client,
    state: Mutex<TokenState>,
}

impl SaxoAuth {
    /// Create a new auth manager from pre-seeded credentials.
    ///
    /// The credentials must include an `access_token`. If no access token is provided,
    /// returns `SaxoError::Config`.
    ///
    /// # Errors
    ///
    /// Returns `SaxoError::Config` if no access token is available in credentials.
    pub fn new(
        credentials: &SaxoCredentials,
        platform: TradingPlatform,
    ) -> Result<Self, SaxoError> {
        let access_token_str = credentials.access_token_str().ok_or_else(|| {
            SaxoError::Config("No access_token provided in credentials".to_string())
        })?;
        let access_token = SecretBox::new(Box::new(access_token_str.to_string()));

        let token_url = credentials
            .auth_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::SaxoLive => SAXO_LIVE_TOKEN_URL.to_string(),
                _ => SAXO_SIM_TOKEN_URL.to_string(),
            });

        // Default token validity: assume 20 minutes if we don't know the real expiry
        let expires_at = Instant::now() + std::time::Duration::from_secs(1200);

        Ok(Self {
            token_url,
            client_id: credentials.client_id.clone(),
            client_secret: SecretBox::new(Box::new(credentials.client_secret_str().to_string())),
            http_client: reqwest::Client::new(),
            state: Mutex::new(TokenState {
                access_token,
                refresh_token: credentials
                    .refresh_token_str()
                    .map(|s| SecretBox::new(Box::new(s.to_string()))),
                expires_at,
            }),
        })
    }

    /// Get a valid access token, refreshing if near expiry.
    ///
    /// # Errors
    ///
    /// Returns `SaxoError::TokenExpired` if the token is expired and no refresh token exists.
    /// Returns `SaxoError::TokenRefreshFailed` if the refresh request fails.
    pub async fn access_token(&self) -> Result<String, SaxoError> {
        let mut state = self.state.lock().await;

        if state.expires_at > Instant::now() + std::time::Duration::from_secs(EXPIRY_BUFFER_SECS) {
            return Ok(state.access_token.expose_secret().clone());
        }

        debug!("Saxo access token near expiry, attempting refresh");

        let refresh_token = state
            .refresh_token
            .as_ref()
            .ok_or(SaxoError::TokenExpired)?
            .expose_secret()
            .clone();

        let new_state = self.exchange_refresh_token(&refresh_token).await?;
        state.access_token = SecretBox::new(Box::new(new_state.access_token.clone()));
        state.refresh_token = new_state
            .refresh_token
            .map(|t| SecretBox::new(Box::new(t)))
            .or_else(|| Some(SecretBox::new(Box::new(refresh_token))));
        state.expires_at = Instant::now() + std::time::Duration::from_secs(new_state.expires_in);

        Ok(state.access_token.expose_secret().clone())
    }

    /// Force a token refresh (called on 401 responses).
    ///
    /// # Errors
    ///
    /// Returns `SaxoError::TokenExpired` if no refresh token is available.
    /// Returns `SaxoError::TokenRefreshFailed` if the refresh request fails.
    pub async fn force_refresh(&self) -> Result<String, SaxoError> {
        let mut state = self.state.lock().await;

        let refresh_token = state
            .refresh_token
            .as_ref()
            .ok_or(SaxoError::TokenExpired)?
            .expose_secret()
            .clone();

        let new_state = self.exchange_refresh_token(&refresh_token).await?;
        state.access_token = SecretBox::new(Box::new(new_state.access_token.clone()));
        state.refresh_token = new_state
            .refresh_token
            .map(|t| SecretBox::new(Box::new(t)))
            .or_else(|| Some(SecretBox::new(Box::new(refresh_token))));
        state.expires_at = Instant::now() + std::time::Duration::from_secs(new_state.expires_in);

        Ok(state.access_token.expose_secret().clone())
    }

    /// Exchange a refresh token for a new access token.
    async fn exchange_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<SaxoTokenResponse, SaxoError> {
        let url = format!("{}/connect/token", self.token_url);

        let response = self
            .http_client
            .post(&url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &self.client_id),
                ("client_secret", self.client_secret.expose_secret()),
            ])
            .send()
            .await
            .map_err(SaxoError::Http)?;

        if response.status().is_success() {
            let token_response: SaxoTokenResponse =
                response.json().await.map_err(SaxoError::Http)?;
            debug!(
                expires_in = token_response.expires_in,
                has_refresh = token_response.refresh_token.is_some(),
                "Saxo token refreshed"
            );
            Ok(token_response)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let error_msg = serde_json::from_str::<SaxoTokenErrorResponse>(&body).map_or_else(
                |_| format!("HTTP {status}: {body}"),
                |e| format!("{}: {}", e.error, e.error_description.unwrap_or_default()),
            );

            warn!(status = %status, error = %error_msg, "Saxo token refresh failed");
            Err(SaxoError::TokenRefreshFailed { message: error_msg })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretBox;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_credentials(mock_url: &str) -> SaxoCredentials {
        SaxoCredentials {
            client_id: "test-client-id".to_string(),
            client_secret: SecretBox::new(Box::new("test-client-secret".to_string())),
            account_key: "test-account-key".to_string(),
            access_token: Some(SecretBox::new(Box::new("initial-token".to_string()))),
            refresh_token: Some(SecretBox::new(Box::new(
                "initial-refresh-token".to_string(),
            ))),
            rest_url: None,
            auth_url: Some(mock_url.to_string()),
            ws_url: None,
            precheck_orders: None,
        }
    }

    #[tokio::test]
    async fn new_with_preseeded_token() {
        let auth = SaxoAuth::new(
            &test_credentials("http://localhost:9999"),
            TradingPlatform::SaxoSim,
        )
        .unwrap();
        let token = auth.access_token().await.unwrap();
        assert_eq!(token, "initial-token");
    }

    #[tokio::test]
    async fn new_without_access_token_returns_config_error() {
        let mut creds = test_credentials("http://localhost:9999");
        creds.access_token = None;
        let result = SaxoAuth::new(&creds, TradingPlatform::SaxoSim);
        assert!(matches!(result, Err(SaxoError::Config(_))));
    }

    #[tokio::test]
    async fn force_refresh_exchanges_token() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/connect/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("client_id=test-client-id"))
            .and(body_string_contains("client_secret=test-client-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new-access-token",
                "token_type": "Bearer",
                "expires_in": 1200,
                "refresh_token": "new-refresh-token"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let auth = SaxoAuth::new(
            &test_credentials(&mock_server.uri()),
            TradingPlatform::SaxoSim,
        )
        .unwrap();

        let new_token = auth.force_refresh().await.unwrap();
        assert_eq!(new_token, "new-access-token");

        let token = auth.access_token().await.unwrap();
        assert_eq!(token, "new-access-token");
    }

    #[tokio::test]
    async fn force_refresh_returns_error_on_invalid_grant() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/connect/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "The refresh token has expired"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let auth = SaxoAuth::new(
            &test_credentials(&mock_server.uri()),
            TradingPlatform::SaxoSim,
        )
        .unwrap();

        let result = auth.force_refresh().await;
        assert!(matches!(result, Err(SaxoError::TokenRefreshFailed { .. })));
    }

    #[tokio::test]
    async fn access_token_returns_token_expired_without_refresh_token() {
        let mut creds = test_credentials("http://localhost:9999");
        creds.refresh_token = None;
        let auth = SaxoAuth::new(&creds, TradingPlatform::SaxoSim).unwrap();

        // Set token as expired
        {
            let mut state = auth.state.lock().await;
            state.expires_at = Instant::now();
        }

        let result = auth.access_token().await;
        assert!(matches!(result, Err(SaxoError::TokenExpired)));
    }
}
