//! Binance HTTP client with authenticated request support.
//!
//! Provides a reusable HTTP client for making signed requests to Binance APIs.
//! Used by all Binance product adapters (Spot, Futures, Margin, etc.).

use std::sync::Arc;

use reqwest::Client;
use secrecy::{ExposeSecret, SecretBox};
use std::time::Duration;
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::http::RetryConfig;

use super::auth::{DEFAULT_RECV_WINDOW, current_timestamp_ms, sign_query};
use super::error::binance_error_mapper;
use tektii_gateway_core::http::execute_with_retry;

/// HTTP client for authenticated Binance API requests.
///
/// Handles:
/// - HMAC-SHA256 request signing
/// - Timestamp and recvWindow parameters
/// - Retry on transient failures
/// - Provider-specific error mapping
///
/// Secrets are stored as `Arc<SecretBox<String>>` to prevent long-lived
/// plaintext in memory. Exposed only when building HTTP headers and signatures.
#[derive(Clone)]
pub struct BinanceHttpClient {
    client: Client,
    base_url: String,
    api_key: Arc<SecretBox<String>>,
    api_secret: Arc<SecretBox<String>>,
    /// Provider name for error messages (e.g., "binance", "binance-futures")
    provider: &'static str,
    /// Receive window in milliseconds
    recv_window: i64,
    /// Retry configuration for transient failure handling
    retry_config: RetryConfig,
}

impl BinanceHttpClient {
    /// Create a new Binance HTTP client.
    ///
    /// # Errors
    ///
    /// Returns `reqwest::Error` if the HTTP client fails to build.
    pub fn new(
        base_url: &str,
        api_key: Arc<SecretBox<String>>,
        api_secret: Arc<SecretBox<String>>,
        provider: &'static str,
    ) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self {
            client,
            base_url: base_url.to_string(),
            api_key,
            api_secret,
            provider,
            recv_window: DEFAULT_RECV_WINDOW,
            retry_config: RetryConfig::default(),
        })
    }

    /// Create from an existing reqwest Client (for testing or custom configuration).
    #[must_use]
    #[allow(dead_code)]
    pub fn with_client(
        client: Client,
        base_url: &str,
        api_key: Arc<SecretBox<String>>,
        api_secret: Arc<SecretBox<String>>,
        provider: &'static str,
    ) -> Self {
        Self {
            client,
            base_url: base_url.to_string(),
            api_key,
            api_secret,
            provider,
            recv_window: DEFAULT_RECV_WINDOW,
            retry_config: RetryConfig::default(),
        }
    }

    /// Override the retry configuration (useful for tests).
    #[must_use]
    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Returns a reference to the underlying reqwest Client.
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Returns the base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns a reference to the API key secret.
    #[must_use]
    pub fn api_key(&self) -> &SecretBox<String> {
        &self.api_key
    }

    /// Returns a reference to the API secret.
    #[must_use]
    #[allow(dead_code)]
    pub fn api_secret(&self) -> &SecretBox<String> {
        &self.api_secret
    }

    /// Returns a cloneable handle to the API key.
    #[must_use]
    pub fn api_key_arc(&self) -> Arc<SecretBox<String>> {
        Arc::clone(&self.api_key)
    }

    /// Returns the provider name.
    #[must_use]
    #[allow(dead_code)]
    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// Send an authenticated GET request with retry on transient failures.
    pub async fn signed_get<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        base_params: &str,
    ) -> GatewayResult<T> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let endpoint = endpoint.to_string();
        let base_params = base_params.to_string();
        let recv_window = self.recv_window;
        let provider = self.provider;

        let response = execute_with_retry(
            || {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = Arc::clone(&api_key);
                let api_secret = Arc::clone(&api_secret);
                let endpoint = endpoint.clone();
                let base_params = base_params.clone();

                async move {
                    let timestamp = current_timestamp_ms();
                    let query = if base_params.is_empty() {
                        format!("timestamp={timestamp}&recvWindow={recv_window}")
                    } else {
                        format!("{base_params}&timestamp={timestamp}&recvWindow={recv_window}")
                    };
                    let signature = sign_query(api_secret.expose_secret(), &query);
                    let signed_query = format!("{query}&signature={signature}");
                    let url = format!("{base_url}/{endpoint}?{signed_query}");

                    client
                        .get(&url)
                        .header("X-MBX-APIKEY", api_key.expose_secret().as_str())
                }
            },
            provider,
            Some(&self.retry_config),
            Some(binance_error_mapper),
        )
        .await?;

        response.json().await.map_err(|e| {
            GatewayError::internal(format!("Failed to parse {provider} response: {e}"))
        })
    }

    /// Send an authenticated POST request with retry on transient failures.
    pub async fn signed_post<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        base_params: &str,
    ) -> GatewayResult<T> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let endpoint = endpoint.to_string();
        let base_params = base_params.to_string();
        let recv_window = self.recv_window;
        let provider = self.provider;

        let response = execute_with_retry(
            || {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = Arc::clone(&api_key);
                let api_secret = Arc::clone(&api_secret);
                let endpoint = endpoint.clone();
                let base_params = base_params.clone();

                async move {
                    let timestamp = current_timestamp_ms();
                    let query = if base_params.is_empty() {
                        format!("timestamp={timestamp}&recvWindow={recv_window}")
                    } else {
                        format!("{base_params}&timestamp={timestamp}&recvWindow={recv_window}")
                    };
                    let signature = sign_query(api_secret.expose_secret(), &query);
                    let signed_query = format!("{query}&signature={signature}");
                    let url = format!("{base_url}/{endpoint}?{signed_query}");

                    client
                        .post(&url)
                        .header("X-MBX-APIKEY", api_key.expose_secret().as_str())
                }
            },
            provider,
            Some(&self.retry_config),
            Some(binance_error_mapper),
        )
        .await?;

        let body_text = response.text().await.map_err(|e| {
            GatewayError::internal(format!("Failed to read {provider} response body: {e}"))
        })?;
        tracing::debug!(provider = %provider, body = %body_text, "Raw signed_post response");
        serde_json::from_str(&body_text).map_err(|e| {
            tracing::error!(provider = %provider, body = %body_text, error = %e, "Failed to parse response JSON");
            GatewayError::internal(format!("Failed to parse {provider} response: {e}"))
        })
    }

    /// Send an authenticated DELETE request with retry (no response body).
    pub async fn signed_delete(&self, endpoint: &str, base_params: &str) -> GatewayResult<()> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let endpoint = endpoint.to_string();
        let base_params = base_params.to_string();
        let recv_window = self.recv_window;
        let provider = self.provider;

        execute_with_retry(
            || {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = Arc::clone(&api_key);
                let api_secret = Arc::clone(&api_secret);
                let endpoint = endpoint.clone();
                let base_params = base_params.clone();

                async move {
                    let timestamp = current_timestamp_ms();
                    let query = if base_params.is_empty() {
                        format!("timestamp={timestamp}&recvWindow={recv_window}")
                    } else {
                        format!("{base_params}&timestamp={timestamp}&recvWindow={recv_window}")
                    };
                    let signature = sign_query(api_secret.expose_secret(), &query);
                    let signed_query = format!("{query}&signature={signature}");
                    let url = format!("{base_url}/{endpoint}?{signed_query}");

                    client
                        .delete(&url)
                        .header("X-MBX-APIKEY", api_key.expose_secret().as_str())
                }
            },
            provider,
            Some(&self.retry_config),
            Some(binance_error_mapper),
        )
        .await?;

        Ok(())
    }

    /// Send an authenticated DELETE request with response parsing.
    pub async fn signed_delete_with_response<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        base_params: &str,
    ) -> GatewayResult<T> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let endpoint = endpoint.to_string();
        let base_params = base_params.to_string();
        let recv_window = self.recv_window;
        let provider = self.provider;

        let response = execute_with_retry(
            || {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = Arc::clone(&api_key);
                let api_secret = Arc::clone(&api_secret);
                let endpoint = endpoint.clone();
                let base_params = base_params.clone();

                async move {
                    let timestamp = current_timestamp_ms();
                    let query = if base_params.is_empty() {
                        format!("timestamp={timestamp}&recvWindow={recv_window}")
                    } else {
                        format!("{base_params}&timestamp={timestamp}&recvWindow={recv_window}")
                    };
                    let signature = sign_query(api_secret.expose_secret(), &query);
                    let signed_query = format!("{query}&signature={signature}");
                    let url = format!("{base_url}/{endpoint}?{signed_query}");

                    client
                        .delete(&url)
                        .header("X-MBX-APIKEY", api_key.expose_secret().as_str())
                }
            },
            provider,
            Some(&self.retry_config),
            Some(binance_error_mapper),
        )
        .await?;

        response.json().await.map_err(|e| {
            GatewayError::internal(format!("Failed to parse {provider} response: {e}"))
        })
    }
}
