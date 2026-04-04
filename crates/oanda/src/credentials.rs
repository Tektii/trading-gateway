//! Oanda API credentials with secret redaction.

use secrecy::{ExposeSecret, SecretBox};

/// Oanda API credentials for v20 REST and streaming endpoints.
pub struct OandaCredentials {
    /// Bearer API token (secret).
    pub api_token: SecretBox<String>,
    /// Oanda account ID (not secret - appears in URLs).
    pub account_id: String,
    /// Optional REST API base URL override (for testing with mock servers).
    pub rest_url: Option<String>,
    /// Optional streaming base URL override (for testing with mock servers).
    pub stream_url: Option<String>,
}

impl OandaCredentials {
    /// Create new credentials with required fields.
    pub fn new(api_token: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            api_token: SecretBox::new(Box::new(api_token.into())),
            account_id: account_id.into(),
            rest_url: None,
            stream_url: None,
        }
    }

    /// Set a custom REST API base URL (e.g., for testing).
    #[must_use]
    pub fn with_rest_url(mut self, url: impl Into<String>) -> Self {
        self.rest_url = Some(url.into());
        self
    }

    /// Set a custom streaming base URL (e.g., for testing).
    #[must_use]
    pub fn with_stream_url(mut self, url: impl Into<String>) -> Self {
        self.stream_url = Some(url.into());
        self
    }

    /// Expose the API token for use in HTTP headers.
    pub fn expose_token(&self) -> &str {
        self.api_token.expose_secret()
    }
}

impl std::fmt::Debug for OandaCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OandaCredentials")
            .field("api_token", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .field("rest_url", &self.rest_url)
            .field("stream_url", &self.stream_url)
            .finish()
    }
}
