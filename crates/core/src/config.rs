use std::collections::HashSet;
use std::num::ParseIntError;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretBox;

use crate::models::{TradingPlatform, TradingPlatformKind};
use crate::websocket::messages::InstrumentSubscription;

/// Configuration for broker WebSocket reconnection with exponential backoff.
#[derive(Debug, Clone)]
pub struct ReconnectionConfig {
    /// Initial backoff delay (env: `RECONNECT_INITIAL_BACKOFF_MS`, default `1000`).
    pub initial_backoff: Duration,
    /// Maximum backoff delay (env: `RECONNECT_MAX_BACKOFF_MS`, default `60000`).
    pub max_backoff: Duration,
    /// Maximum total retry duration before giving up (env: `RECONNECT_MAX_DURATION_SECS`, default `300`).
    pub max_retry_duration: Duration,
}

impl Default for ReconnectionConfig {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(1000),
            max_backoff: Duration::from_secs(60),
            max_retry_duration: Duration::from_secs(300),
        }
    }
}

impl ReconnectionConfig {
    /// Loads reconnection configuration from environment variables, falling back to defaults.
    ///
    /// # Errors
    ///
    /// Returns `GatewayConfigError::InvalidReconnect` if env values are not valid integers.
    pub fn from_env() -> Result<Self, GatewayConfigError> {
        let initial_backoff = match std::env::var("RECONNECT_INITIAL_BACKOFF_MS") {
            Ok(v) => Duration::from_millis(
                v.parse::<u64>()
                    .map_err(GatewayConfigError::InvalidReconnect)?,
            ),
            Err(_) => Duration::from_millis(1000),
        };

        let max_backoff = match std::env::var("RECONNECT_MAX_BACKOFF_MS") {
            Ok(v) => Duration::from_millis(
                v.parse::<u64>()
                    .map_err(GatewayConfigError::InvalidReconnect)?,
            ),
            Err(_) => Duration::from_secs(60),
        };

        let max_retry_duration = match std::env::var("RECONNECT_MAX_DURATION_SECS") {
            Ok(v) => Duration::from_secs(
                v.parse::<u64>()
                    .map_err(GatewayConfigError::InvalidReconnect)?,
            ),
            Err(_) => Duration::from_secs(300),
        };

        Ok(Self {
            initial_backoff,
            max_backoff,
            max_retry_duration,
        })
    }
}

/// Gateway runtime configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Bind address (env: `GATEWAY_HOST`, default `127.0.0.1`).
    pub host: String,
    /// Listen port (env: `GATEWAY_PORT`, default `8080`).
    pub port: u16,
    /// TTL in hours for SL/TP exit orders (env: `SL_TP_TTL_HOURS`, default `24`).
    pub sl_tp_ttl_hours: u64,
    /// Static subscriptions for WebSocket provider streams (env: `SUBSCRIPTIONS`, default `[]`).
    pub subscriptions: Vec<InstrumentSubscription>,
    /// Path for exit state snapshot file (env: `EXIT_STATE_FILE`, default `./gateway-exit-state.json`).
    pub exit_state_file: PathBuf,
    /// Broker WebSocket reconnection configuration.
    pub reconnection: ReconnectionConfig,
    /// Optional API key for authenticating REST and WebSocket requests.
    /// When `Some`, all non-health endpoints require `Authorization: Bearer <key>`
    /// or `X-API-Key: <key>`. When `None`, no authentication is enforced.
    pub api_key: Option<Arc<SecretBox<String>>>,
    /// Whether to serve Swagger UI at `/swagger-ui` and the OpenAPI spec at `/api-docs`.
    /// (env: `ENABLE_SWAGGER`, default `false`).
    pub enable_swagger: bool,
}

/// Errors that can occur when loading [`GatewayConfig`] from the environment.
#[derive(Debug, thiserror::Error)]
pub enum GatewayConfigError {
    #[error("invalid GATEWAY_PORT: {0}")]
    InvalidPort(#[from] ParseIntError),

    #[error("invalid SL_TP_TTL_HOURS: {0}")]
    InvalidTtl(ParseIntError),

    #[error("invalid SUBSCRIPTIONS JSON: {0}")]
    InvalidSubscriptions(#[from] serde_json::Error),

    #[error("invalid reconnection config: {0}")]
    InvalidReconnect(ParseIntError),

    #[error("GATEWAY_API_KEY is too short ({0} chars). Minimum length is 32 characters.")]
    ApiKeyTooShort(usize),

    #[error("{0}")]
    MissingEnvVar(String),

    #[error("{0}")]
    InvalidEnvVar(String),
}

/// Load and validate required environment variables for a provider.
///
/// Returns the values in the same order as [`TradingPlatformKind::required_env_vars`].
/// On failure, lists which vars are set and which are missing.
pub fn load_required_env_vars(
    provider: TradingPlatformKind,
) -> Result<Vec<String>, GatewayConfigError> {
    let required = provider.required_env_vars();
    let mut values = Vec::with_capacity(required.len());
    let mut missing = Vec::new();

    for &var in required {
        match std::env::var(var) {
            Ok(v) if !v.is_empty() => values.push(v),
            _ => missing.push(var),
        }
    }

    if missing.is_empty() {
        return Ok(values);
    }

    let mut msg = format!("Provider '{provider}' requires");
    for (i, var) in required.iter().enumerate() {
        if i > 0 {
            msg.push_str(if i == required.len() - 1 {
                " and "
            } else {
                ", "
            });
        }
        msg.push_str(&format!(" {var}"));
    }
    msg.push_str(".\n");
    for &var in required {
        let status = if missing.contains(&var) {
            "NOT SET"
        } else {
            "set"
        };
        msg.push_str(&format!("  {var}: {status}\n"));
    }

    Err(GatewayConfigError::MissingEnvVar(msg))
}

impl GatewayConfig {
    /// Loads configuration from environment variables, falling back to defaults.
    pub fn from_env() -> Result<Self, GatewayConfigError> {
        let host = std::env::var("GATEWAY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

        let port = match std::env::var("GATEWAY_PORT") {
            Ok(v) => v.parse()?,
            Err(_) => 8080,
        };

        let sl_tp_ttl_hours = match std::env::var("SL_TP_TTL_HOURS") {
            Ok(v) => v.parse().map_err(GatewayConfigError::InvalidTtl)?,
            Err(_) => 24,
        };

        let subscriptions = match std::env::var("SUBSCRIPTIONS") {
            Ok(v) if !v.is_empty() => serde_json::from_str(&v)?,
            _ => vec![],
        };

        let exit_state_file = std::env::var("EXIT_STATE_FILE").map_or_else(
            |_| PathBuf::from("./gateway-exit-state.json"),
            PathBuf::from,
        );

        let reconnection = ReconnectionConfig::from_env()?;

        let api_key = match std::env::var("GATEWAY_API_KEY") {
            Ok(v) if !v.is_empty() => {
                if v.len() < 32 {
                    return Err(GatewayConfigError::ApiKeyTooShort(v.len()));
                }
                Some(Arc::new(SecretBox::new(Box::new(v))))
            }
            _ => None,
        };

        let enable_swagger = matches!(std::env::var("ENABLE_SWAGGER").as_deref(), Ok("true" | "1"));

        Ok(Self {
            host,
            port,
            sl_tp_ttl_hours,
            subscriptions,
            exit_state_file,
            reconnection,
            api_key,
            enable_swagger,
        })
    }

    /// Returns the set of unique platforms referenced by subscriptions.
    #[must_use]
    pub fn platforms_used(&self) -> HashSet<TradingPlatform> {
        self.subscriptions.iter().map(|s| s.platform).collect()
    }

    /// Returns the symbols subscribed for a given platform.
    #[must_use]
    pub fn symbols_for_platform(&self, platform: TradingPlatform) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter(|s| s.platform == platform)
            .map(|s| s.instrument.clone())
            .collect()
    }

    /// Returns the event types subscribed for a given platform.
    #[must_use]
    pub fn events_for_platform(&self, platform: TradingPlatform) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter(|s| s.platform == platform)
            .flat_map(|s| s.events.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Returns subscriptions for a given platform.
    #[must_use]
    pub fn subscriptions_for_platform(
        &self,
        platform: TradingPlatform,
    ) -> Vec<&InstrumentSubscription> {
        self.subscriptions
            .iter()
            .filter(|s| s.platform == platform)
            .collect()
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    /// Helper to clear all gateway env vars before each sub-test.
    unsafe fn clear_env() {
        unsafe {
            std::env::remove_var("GATEWAY_HOST");
            std::env::remove_var("GATEWAY_PORT");
            std::env::remove_var("SL_TP_TTL_HOURS");
            std::env::remove_var("GATEWAY_API_KEY");
        }
    }

    /// Helper to clear subscriptions env var.
    unsafe fn clear_subscriptions() {
        unsafe {
            std::env::remove_var("SUBSCRIPTIONS");
        }
    }

    /// Helper to clear reconnection env vars.
    unsafe fn clear_reconnection() {
        unsafe {
            std::env::remove_var("RECONNECT_INITIAL_BACKOFF_MS");
            std::env::remove_var("RECONNECT_MAX_BACKOFF_MS");
            std::env::remove_var("RECONNECT_MAX_DURATION_SECS");
        }
    }

    /// All config tests share process env vars, so they must run sequentially
    /// within a single test to avoid races from parallel test threads.
    #[test]
    fn from_env_config_parsing() {
        // --- defaults when no env vars ---
        unsafe {
            clear_env();
            clear_subscriptions();
        }

        let config = GatewayConfig::from_env().expect("defaults should work");
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.sl_tp_ttl_hours, 24);
        assert!(config.subscriptions.is_empty());

        // --- reads custom values ---
        unsafe {
            std::env::set_var("GATEWAY_HOST", "0.0.0.0");
            std::env::set_var("GATEWAY_PORT", "9090");
            std::env::set_var("SL_TP_TTL_HOURS", "48");
        }

        let config = GatewayConfig::from_env().expect("valid env vars");
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 9090);
        assert_eq!(config.sl_tp_ttl_hours, 48);

        // --- rejects invalid port ---
        unsafe {
            clear_env();
            clear_subscriptions();
            std::env::set_var("GATEWAY_PORT", "not-a-number");
        }
        let err = GatewayConfig::from_env().unwrap_err();
        assert!(matches!(err, GatewayConfigError::InvalidPort(_)));

        // --- rejects invalid ttl ---
        unsafe {
            clear_env();
            clear_subscriptions();
            std::env::set_var("SL_TP_TTL_HOURS", "abc");
        }
        let err = GatewayConfig::from_env().unwrap_err();
        assert!(matches!(err, GatewayConfigError::InvalidTtl(_)));

        // --- parses subscriptions JSON ---
        unsafe {
            clear_env();
            clear_subscriptions();
            std::env::set_var(
                "SUBSCRIPTIONS",
                r#"[{"platform":"alpaca-paper","instrument":"AAPL","events":["quote"]}]"#,
            );
        }
        let config = GatewayConfig::from_env().expect("valid subscriptions");
        assert_eq!(config.subscriptions.len(), 1);
        assert_eq!(config.subscriptions[0].instrument, "AAPL");

        // --- rejects invalid subscriptions ---
        unsafe {
            clear_env();
            clear_subscriptions();
            std::env::set_var("SUBSCRIPTIONS", "not-json");
        }
        let err = GatewayConfig::from_env().unwrap_err();
        assert!(matches!(err, GatewayConfigError::InvalidSubscriptions(_)));

        // cleanup
        unsafe {
            clear_env();
            clear_subscriptions();
        }
    }

    /// Reconnection config tests — sequential to avoid env var races.
    #[test]
    fn reconnection_config_parsing() {
        // --- defaults when no env vars ---
        unsafe {
            clear_reconnection();
        }

        let config = ReconnectionConfig::from_env().expect("defaults should work");
        assert_eq!(config.initial_backoff, Duration::from_millis(1000));
        assert_eq!(config.max_backoff, Duration::from_secs(60));
        assert_eq!(config.max_retry_duration, Duration::from_secs(300));

        // --- reads custom values ---
        unsafe {
            clear_reconnection();
            std::env::set_var("RECONNECT_INITIAL_BACKOFF_MS", "500");
            std::env::set_var("RECONNECT_MAX_BACKOFF_MS", "30000");
            std::env::set_var("RECONNECT_MAX_DURATION_SECS", "120");
        }

        let config = ReconnectionConfig::from_env().expect("valid env vars");
        assert_eq!(config.initial_backoff, Duration::from_millis(500));
        assert_eq!(config.max_backoff, Duration::from_millis(30000));
        assert_eq!(config.max_retry_duration, Duration::from_secs(120));

        // --- rejects invalid values ---
        unsafe {
            clear_reconnection();
            std::env::set_var("RECONNECT_INITIAL_BACKOFF_MS", "not-a-number");
        }

        let err = ReconnectionConfig::from_env().unwrap_err();
        assert!(matches!(err, GatewayConfigError::InvalidReconnect(_)));

        // cleanup
        unsafe {
            clear_reconnection();
        }
    }

    /// API key config tests — sequential to avoid env var races.
    #[test]
    fn api_key_config_parsing() {
        // --- no key set → None ---
        unsafe {
            clear_env();
            clear_subscriptions();
        }
        let config = GatewayConfig::from_env().expect("defaults should work");
        assert!(config.api_key.is_none());

        // --- empty string → None ---
        unsafe {
            clear_env();
            clear_subscriptions();
            std::env::set_var("GATEWAY_API_KEY", "");
        }
        let config = GatewayConfig::from_env().expect("empty key is ok");
        assert!(config.api_key.is_none());

        // --- too short → error ---
        unsafe {
            clear_env();
            clear_subscriptions();
            std::env::set_var("GATEWAY_API_KEY", "short-key");
        }
        let err = GatewayConfig::from_env().unwrap_err();
        assert!(matches!(err, GatewayConfigError::ApiKeyTooShort(9)));

        // --- valid key (32+ chars) → Some ---
        unsafe {
            clear_env();
            clear_subscriptions();
            std::env::set_var("GATEWAY_API_KEY", "abcdefghijklmnopqrstuvwxyz123456");
        }
        let config = GatewayConfig::from_env().expect("valid key should work");
        assert!(config.api_key.is_some());

        // cleanup
        unsafe {
            clear_env();
            clear_subscriptions();
        }
    }
}
