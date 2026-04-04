//! Credentials for connecting to the Tektii Engine.

use secrecy::{ExposeSecret, SecretBox};

/// Connection credentials for the Tektii Engine.
pub struct TektiiCredentials {
    /// Base URL for engine REST API (e.g., "<http://localhost:8080>").
    pub rest_url: String,
    /// WebSocket URL for engine events (e.g., "<ws://localhost:8081>").
    pub ws_url: String,
    /// Optional API key for authentication.
    pub api_key: Option<SecretBox<String>>,
}

impl TektiiCredentials {
    /// Create new credentials with REST and WebSocket URLs.
    #[must_use]
    pub fn new(rest_url: impl Into<String>, ws_url: impl Into<String>) -> Self {
        Self {
            rest_url: rest_url.into(),
            ws_url: ws_url.into(),
            api_key: None,
        }
    }

    /// Add an API key for authentication.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(SecretBox::new(Box::new(api_key.into())));
        self
    }

    /// Get the API key value, if set.
    pub fn api_key_value(&self) -> Option<&str> {
        self.api_key.as_ref().map(|k| k.expose_secret().as_str())
    }
}

impl std::fmt::Debug for TektiiCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TektiiCredentials")
            .field("rest_url", &self.rest_url)
            .field("ws_url", &self.ws_url)
            .field(
                "api_key",
                if self.api_key.is_some() {
                    &"***REDACTED***"
                } else {
                    &"None"
                },
            )
            .finish()
    }
}
