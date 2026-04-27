//! WebSocket provider trait for streaming market events
//!
//! This trait defines the interface for WebSocket-based event streaming,
//! following the same pattern as `TradingAdapter` for REST APIs.
//!
//! Allow dead code - these are public API traits for future implementations

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretBox};
use tokio::sync::mpsc;

use super::error::WebSocketError;
use super::messages::{EventAckMessage, WsMessage};
use crate::models::TradingPlatform;

/// Configuration for WebSocket provider connection
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Trading platform for this connection
    pub platform: TradingPlatform,

    /// Symbols to subscribe to
    pub symbols: Vec<String>,

    /// Event types to receive (e.g., [`candle`, `quote`, `trade`])
    pub event_types: Vec<String>,

    /// Authentication credentials (optional, used for live providers)
    pub credentials: Option<Credentials>,

    /// Tektii-specific parameters (optional, used for Tektii engine)
    pub tektii_params: Option<TektiiParams>,
}

/// Authentication credentials for live trading platforms.
///
/// Secrets are stored as `Arc<SecretBox<String>>` to prevent long-lived
/// plaintext in memory. Call `.expose_secret()` only at the point of use
/// (e.g., building HTTP headers or signing requests).
pub struct Credentials {
    /// API key (wrapped to prevent memory exposure)
    pub api_key: Arc<SecretBox<String>>,

    /// API secret (wrapped to prevent memory exposure)
    pub api_secret: Arc<SecretBox<String>>,

    /// Optional additional parameters
    pub extra: Option<serde_json::Value>,
}

impl Credentials {
    /// Create new credentials from raw strings.
    pub fn new(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            api_key: arc_secret_from_string(api_key.into()),
            api_secret: arc_secret_from_string(api_secret.into()),
            extra: None,
        }
    }
}

impl Clone for Credentials {
    fn clone(&self) -> Self {
        Self {
            api_key: Arc::clone(&self.api_key),
            api_secret: Arc::clone(&self.api_secret),
            extra: self.extra.clone(),
        }
    }
}

// Custom Debug implementation that redacts secrets
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"***REDACTED***")
            .field("api_secret", &"***REDACTED***")
            .field("extra", &self.extra)
            .finish()
    }
}

/// Tektii-specific configuration parameters (e.g., for Tektii engine simulation)
#[derive(Debug, Clone)]
pub struct TektiiParams {
    /// Data provider (e.g., "massive")
    pub provider: String,

    /// Start time (Unix milliseconds)
    pub start_time: u64,

    /// End time (Unix milliseconds)
    pub end_time: u64,

    /// Initial capital
    pub initial_capital: f64,
}

/// Create an `Arc<SecretBox<String>>` from an existing `SecretBox<String>`.
///
/// This briefly exposes the secret on the stack during the copy — unavoidable
/// since `SecretBox` is not `Clone`. The plaintext is transient and not stored.
pub fn arc_secret(value: &SecretBox<String>) -> Arc<SecretBox<String>> {
    Arc::new(SecretBox::new(Box::new(value.expose_secret().clone())))
}

/// Create an `Arc<SecretBox<String>>` from an owned `String`.
pub fn arc_secret_from_string(value: String) -> Arc<SecretBox<String>> {
    Arc::new(SecretBox::new(Box::new(value)))
}

/// Event stream type alias
pub type EventStream = mpsc::UnboundedReceiver<WsMessage>;

/// WebSocket provider trait for streaming market events
///
/// This trait is implemented by:
/// - `TektiiWebSocketProvider` in the tektii crate (for Tektii engine simulation)
/// - `AlpacaWebSocketProvider` in the alpaca crate (for live Alpaca trading)
/// - `BinanceWebSocketProvider` in the binance crate (for live Binance trading)
///
/// # Example
///
/// ```ignore
/// use tektii_gateway_core::websocket::{WebSocketProvider, ProviderConfig};
/// use tektii_gateway_core::models::TradingPlatform;
///
/// async fn connect_to_provider(provider: impl WebSocketProvider) {
///     let config = ProviderConfig {
///         platform: TradingPlatform::AlpacaLive,
///         symbols: vec!["BTCUSDT".to_string()],
///         event_types: vec!["candle".to_string()],
///         credentials: Some(credentials),
///         tektii_params: None,
///     };
///
///     let mut stream = provider.connect(config).await.unwrap();
///
///     // Process events
///     while let Some(event) = stream.recv().await {
///         println!("Received: {:?}", event);
///     }
/// }
/// ```
#[async_trait]
pub trait WebSocketProvider: Send + Sync {
    /// Connect to the WebSocket feed
    ///
    /// # Arguments
    ///
    /// * `config` - Provider-specific configuration (auth, symbols, etc.)
    ///
    /// # Returns
    ///
    /// * Event stream receiver
    ///
    /// # Errors
    ///
    /// * `WebSocketError::ConnectionFailed` - If unable to establish connection
    /// * `WebSocketError::ConfigError` - If configuration is invalid
    async fn connect(&self, config: ProviderConfig) -> Result<EventStream, WebSocketError>;

    /// Subscribe to symbols/events
    ///
    /// # Arguments
    ///
    /// * `symbols` - Symbols to subscribe to
    /// * `event_types` - Event types to receive
    ///
    /// # Errors
    ///
    /// * `WebSocketError::ProviderError` - If subscription fails
    async fn subscribe(
        &self,
        symbols: Vec<String>,
        event_types: Vec<String>,
    ) -> Result<(), WebSocketError>;

    /// Unsubscribe from symbols/events
    ///
    /// # Arguments
    ///
    /// * `symbols` - Symbols to unsubscribe from
    ///
    /// # Errors
    ///
    /// * `WebSocketError::ProviderError` - If unsubscription fails
    async fn unsubscribe(&self, symbols: Vec<String>) -> Result<(), WebSocketError>;

    /// Handle incoming acknowledgment from strategy
    ///
    /// The engine implementation MUST use this to control simulation time progression.
    /// Live provider implementations MAY ignore this (no time control needed).
    ///
    /// # Arguments
    ///
    /// * `ack` - Acknowledgment message from strategy
    ///
    /// # Errors
    ///
    /// * `WebSocketError::InvalidAck` - If acknowledgment is malformed
    async fn handle_ack(&self, ack: EventAckMessage) -> Result<(), WebSocketError>;

    /// Disconnect and cleanup
    ///
    /// # Errors
    ///
    /// * `WebSocketError::ConnectionClosed` - If disconnect fails
    async fn disconnect(&self) -> Result<(), WebSocketError>;

    /// Reconnect after a disconnect.
    ///
    /// Re-establishes the connection using stored configuration from the
    /// initial `connect()` call. The provider should re-authenticate and
    /// re-subscribe to previously active subscriptions.
    ///
    /// # Errors
    ///
    /// * `WebSocketError::ConnectionFailed` - If unable to establish connection
    /// * `WebSocketError::PermanentAuthError` - If credentials are invalid (don't retry)
    /// * `WebSocketError::ConnectionClosed` - If never connected (no stored config)
    async fn reconnect(&self) -> Result<EventStream, WebSocketError>;

    /// Whether this provider supports automatic reconnection.
    ///
    /// Returns `false` for the Tektii engine provider (engine disconnect
    /// during simulation is a failure, not a recoverable event).
    fn supports_reconnection(&self) -> bool {
        true
    }

    /// Whether the upstream is the source of truth for which events to emit.
    ///
    /// Returns `true` for providers that already filter events at the source
    /// (e.g., the Tektii backtest engine, which only emits events for the
    /// resolved subscriptions validated at startup). When `true`, the gateway's
    /// `ProviderRegistry` skips its own subscription filter and rebroadcasts
    /// every event the provider yields — re-filtering at the gateway with
    /// weaker pattern semantics is exactly where TEK-268 silent drops creep in.
    ///
    /// Default `false`: live brokers may push unsolicited events (account
    /// updates, exchange status, etc.); the gateway filter gates which of
    /// those reach strategies. Opt-in unsafe — any new provider that doesn't
    /// override this gets the safe (filtered) path.
    fn filters_events_upstream(&self) -> bool {
        false
    }
}
