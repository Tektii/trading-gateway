//! Binance User Data Stream for private trading events.
//!
//! This module provides authenticated WebSocket streaming for order execution
//! events from Binance. Unlike market data streams, User Data Streams require
//! authentication via a listen key and periodic keepalive requests.

use std::sync::Arc;

use secrecy::{ExposeSecret, SecretBox};
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::websocket::error::WebSocketError;
use tracing::{debug, warn};

/// Binance User Data Stream for authenticated trading events.
///
/// Manages listen key lifecycle and WebSocket connection for receiving
/// order execution events from Binance.
pub struct BinanceUserDataStream {
    /// REST API base URL for listen key management
    rest_base_url: String,
    /// WebSocket base URL for stream connection
    ws_base_url: String,
    /// API key for authentication
    api_key: Arc<SecretBox<String>>,
    /// Platform for event tagging
    platform: TradingPlatform,
    /// Listen key REST endpoint path.
    listen_key_path: String,
}

impl BinanceUserDataStream {
    /// Create a new User Data Stream client.
    #[must_use]
    pub fn new(api_key: Arc<SecretBox<String>>, platform: TradingPlatform) -> Self {
        let (rest_base_url, ws_base_url) = match platform {
            TradingPlatform::BinanceSpotTestnet | TradingPlatform::BinanceMarginTestnet => (
                "https://testnet.binance.vision".to_string(),
                "wss://stream.testnet.binance.vision".to_string(),
            ),
            TradingPlatform::BinanceFuturesLive => (
                "https://fapi.binance.com".to_string(),
                "wss://fstream.binance.com".to_string(),
            ),
            TradingPlatform::BinanceFuturesTestnet => (
                "https://testnet.binancefuture.com".to_string(),
                "wss://fstream.binancefuture.com".to_string(),
            ),
            TradingPlatform::BinanceCoinFuturesLive => (
                "https://dapi.binance.com".to_string(),
                "wss://dstream.binance.com".to_string(),
            ),
            TradingPlatform::BinanceCoinFuturesTestnet => (
                "https://testnet.binancefuture.com".to_string(),
                "wss://dstream.binancefuture.com".to_string(),
            ),
            // BinanceSpotLive, BinanceMarginLive, and anything unrecognised
            _ => (
                "https://api.binance.com".to_string(),
                "wss://stream.binance.com:9443".to_string(),
            ),
        };

        let listen_key_path = listen_key_path_for_platform(platform);

        Self {
            rest_base_url,
            ws_base_url,
            api_key,
            platform,
            listen_key_path,
        }
    }

    /// Returns the REST base URL.
    #[must_use]
    pub fn rest_base_url(&self) -> &str {
        &self.rest_base_url
    }

    /// Returns the WebSocket base URL.
    #[must_use]
    pub fn ws_base_url(&self) -> &str {
        &self.ws_base_url
    }

    /// Returns the API key.
    #[must_use]
    pub fn api_key(&self) -> &SecretBox<String> {
        &self.api_key
    }

    /// Returns the platform.
    #[must_use]
    pub const fn platform(&self) -> TradingPlatform {
        self.platform
    }

    /// Returns the listen key path.
    #[must_use]
    pub fn listen_key_path(&self) -> &str {
        &self.listen_key_path
    }

    /// Build the WebSocket URL for a given listen key.
    #[must_use]
    pub fn ws_url(&self, listen_key: &str) -> String {
        format!("{}/ws/{listen_key}", self.ws_base_url)
    }

    /// Create a new listen key via REST API.
    ///
    /// Only requires `X-MBX-APIKEY` header — no HMAC signature.
    pub async fn create_listen_key(
        &self,
        client: &reqwest::Client,
    ) -> Result<String, WebSocketError> {
        #[derive(serde::Deserialize)]
        struct ListenKeyResponse {
            #[serde(rename = "listenKey")]
            listen_key: String,
        }

        let url = format!("{}{}", self.rest_base_url, self.listen_key_path);

        let resp = client
            .post(&url)
            .header("X-MBX-APIKEY", self.api_key.expose_secret().as_str())
            .send()
            .await
            .map_err(|e| {
                WebSocketError::ConnectionFailed(format!("Listen key create failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(WebSocketError::ConnectionFailed(format!(
                "Listen key create returned {status}: {body}"
            )));
        }

        let parsed: ListenKeyResponse = resp.json().await.map_err(|e| {
            WebSocketError::ConnectionFailed(format!("Listen key parse failed: {e}"))
        })?;

        debug!(platform = ?self.platform, "Created listen key");
        Ok(parsed.listen_key)
    }

    /// Send keepalive for an existing listen key.
    ///
    /// Should be called every ~20 minutes. Keys expire after 60 minutes.
    pub async fn keepalive_listen_key(
        &self,
        client: &reqwest::Client,
        listen_key: &str,
    ) -> Result<(), WebSocketError> {
        let url = format!("{}{}", self.rest_base_url, self.listen_key_path);

        let resp = client
            .put(&url)
            .header("X-MBX-APIKEY", self.api_key.expose_secret().as_str())
            .query(&[("listenKey", listen_key)])
            .send()
            .await
            .map_err(|e| {
                WebSocketError::ProviderError(format!("Listen key keepalive failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(platform = ?self.platform, %status, "Listen key keepalive failed: {body}");
            return Err(WebSocketError::ProviderError(format!(
                "Listen key keepalive returned {status}: {body}"
            )));
        }

        debug!(platform = ?self.platform, "Listen key keepalive sent");
        Ok(())
    }

    /// Delete a listen key, closing the user data stream.
    pub async fn delete_listen_key(
        &self,
        client: &reqwest::Client,
        listen_key: &str,
    ) -> Result<(), WebSocketError> {
        let url = format!("{}{}", self.rest_base_url, self.listen_key_path);

        let resp = client
            .delete(&url)
            .header("X-MBX-APIKEY", self.api_key.expose_secret().as_str())
            .query(&[("listenKey", listen_key)])
            .send()
            .await
            .map_err(|e| WebSocketError::ProviderError(format!("Listen key delete failed: {e}")))?;

        if resp.status().is_success() {
            debug!(platform = ?self.platform, "Listen key deleted");
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(platform = ?self.platform, %status, "Listen key delete failed: {body}");
        }

        Ok(())
    }
}

/// Determine the listen key REST endpoint path for a given platform.
fn listen_key_path_for_platform(platform: TradingPlatform) -> String {
    match platform {
        TradingPlatform::BinanceFuturesLive | TradingPlatform::BinanceFuturesTestnet => {
            "/fapi/v1/listenKey".to_string()
        }
        TradingPlatform::BinanceCoinFuturesLive | TradingPlatform::BinanceCoinFuturesTestnet => {
            "/dapi/v1/listenKey".to_string()
        }
        _ => "/api/v3/userDataStream".to_string(),
    }
}
