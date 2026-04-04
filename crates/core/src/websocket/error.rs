//! WebSocket-specific error types
//!
//! Allow dead code - these are public API error types for completeness

#![allow(dead_code)]

use thiserror::Error;

/// WebSocket operation errors
#[derive(Error, Debug)]
pub enum WebSocketError {
    /// Connection failed
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    /// Invalid acknowledgment received
    #[error("Invalid acknowledgment: {0}")]
    InvalidAck(String),

    /// Acknowledgment timeout
    #[error("Acknowledgment timeout: {0}")]
    AckTimeout(String),

    /// Connection closed unexpectedly
    #[error("Connection closed: {0}")]
    ConnectionClosed(String),

    /// Connection not found
    #[error("Connection not found: {0}")]
    ConnectionNotFound(String),

    /// Message serialization/deserialization error
    #[error("Message error: {0}")]
    MessageError(#[from] serde_json::Error),

    /// Provider-specific error
    #[error("Provider error: {0}")]
    ProviderError(String),

    /// Permanent authentication/authorization error (401, 403)
    /// These errors will not recover with retries - API key invalid, account suspended, etc.
    #[error("Permanent auth error: {0}")]
    PermanentAuthError(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// Send error
    #[error("Failed to send message: {0}")]
    SendError(String),

    /// Receive error
    #[error("Failed to receive message: {0}")]
    ReceiveError(String),

    /// Invalid input
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// Serialization failed
    #[error("Serialization failed: {0}")]
    SerializationFailed(String),

    /// Generic error
    #[error("{0}")]
    Other(String),
}
