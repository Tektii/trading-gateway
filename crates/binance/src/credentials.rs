//! Binance API credentials with secret redaction.

use secrecy::SecretBox;

/// Binance API credentials.
///
/// Supports all Binance products (Spot, Margin, Futures, Coin-M).
/// Secrets are wrapped in `SecretBox` to prevent accidental logging.
pub struct BinanceCredentials {
    /// Binance API key
    pub api_key: SecretBox<String>,
    /// Binance API secret
    pub api_secret: SecretBox<String>,
    /// Optional REST API base URL override (for testing with mock servers)
    pub base_url: Option<String>,
    /// Optional WebSocket base URL override (for testing)
    pub ws_base_url: Option<String>,
}

impl BinanceCredentials {
    /// Create new Binance credentials.
    pub fn new(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            api_key: SecretBox::new(Box::new(api_key.into())),
            api_secret: SecretBox::new(Box::new(api_secret.into())),
            base_url: None,
            ws_base_url: None,
        }
    }

    /// Set a custom REST API base URL.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Set a custom WebSocket base URL.
    #[must_use]
    pub fn with_ws_base_url(mut self, url: impl Into<String>) -> Self {
        self.ws_base_url = Some(url.into());
        self
    }
}

impl std::fmt::Debug for BinanceCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BinanceCredentials")
            .field("api_key", &"[REDACTED]")
            .field("api_secret", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("ws_base_url", &self.ws_base_url)
            .finish()
    }
}
