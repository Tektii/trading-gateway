//! Saxo Bank OpenAPI credentials for SIM or LIVE environments.

use secrecy::{ExposeSecret, SecretBox};

/// Saxo Bank OpenAPI credentials.
///
/// Contains OAuth2 client credentials and pre-seeded tokens for
/// authenticating with the Saxo Bank OpenAPI (SIM or LIVE).
pub struct SaxoCredentials {
    /// OAuth2 client ID (public, not secret).
    pub client_id: String,
    /// OAuth2 client secret.
    pub client_secret: SecretBox<String>,
    /// Saxo account key.
    pub account_key: String,
    /// Pre-seeded access token (from dev portal or external OAuth flow).
    pub access_token: Option<SecretBox<String>>,
    /// Pre-seeded refresh token.
    pub refresh_token: Option<SecretBox<String>>,
    /// REST API base URL override (for mock servers).
    pub rest_url: Option<String>,
    /// Auth/token URL override (for mock servers).
    pub auth_url: Option<String>,
    /// WebSocket URL override (for mock servers).
    pub ws_url: Option<String>,
    /// Whether to run pre-trade margin precheck before placing orders (default: true).
    /// Set to `false` to skip the `POST /trade/v2/orders/precheck` call.
    pub precheck_orders: Option<bool>,
}

impl SaxoCredentials {
    /// Returns whether pre-trade margin precheck is enabled (default: true).
    #[must_use]
    pub fn precheck_enabled(&self) -> bool {
        self.precheck_orders.unwrap_or(true)
    }

    /// Expose the access token string (if present).
    pub fn access_token_str(&self) -> Option<&str> {
        self.access_token
            .as_ref()
            .map(|s| s.expose_secret().as_str())
    }

    /// Expose the refresh token string (if present).
    pub fn refresh_token_str(&self) -> Option<&str> {
        self.refresh_token
            .as_ref()
            .map(|s| s.expose_secret().as_str())
    }

    /// Expose the client secret string.
    pub fn client_secret_str(&self) -> &str {
        self.client_secret.expose_secret()
    }
}

impl std::fmt::Debug for SaxoCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaxoCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("account_key", &self.account_key)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("rest_url", &self.rest_url)
            .field("auth_url", &self.auth_url)
            .field("ws_url", &self.ws_url)
            .field("precheck_orders", &self.precheck_orders)
            .finish()
    }
}
