//! WebSocket message types for the Tektii Engine.
//!
//! Split into `ServerMessage` (engine -> strategy) and `ClientMessage` (strategy -> engine)
//! for type safety.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::rest::{Account, AccountEventType, Order, OrderEventType, Position, Trade};

// =============================================================================
// Server -> Client Messages
// =============================================================================

/// Messages sent from the engine to connected strategies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Market data candle event.
    Candle {
        event_id: String,
        symbol: String,
        timeframe: String,
        timestamp: u64,
        open: Decimal,
        high: Decimal,
        low: Decimal,
        close: Decimal,
        volume: f64,
    },

    /// Order state change event.
    Order {
        event_id: String,
        event: OrderEventType,
        order: Order,
    },

    /// Trade execution event.
    Trade { event_id: String, trade: Trade },

    /// Position update event.
    Position {
        event_id: String,
        position: Position,
    },

    /// Account state update event.
    Account {
        event_id: String,
        event: AccountEventType,
        account: Account,
    },

    /// Error event.
    Error {
        event_id: String,
        code: String,
        message: String,
    },

    /// Response to client Ping.
    Pong,
}

impl ServerMessage {
    /// Create a pong response.
    pub const fn pong() -> Self {
        Self::Pong
    }

    /// Create an error message.
    pub fn error(
        event_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Error {
            event_id: event_id.into(),
            code: code.into(),
            message: message.into(),
        }
    }

    /// Create a candle event.
    #[allow(clippy::too_many_arguments)]
    pub fn candle(
        event_id: impl Into<String>,
        symbol: impl Into<String>,
        timeframe: impl Into<String>,
        timestamp: u64,
        open: Decimal,
        high: Decimal,
        low: Decimal,
        close: Decimal,
        volume: f64,
    ) -> Self {
        Self::Candle {
            event_id: event_id.into(),
            symbol: symbol.into(),
            timeframe: timeframe.into(),
            timestamp,
            open,
            high,
            low,
            close,
            volume,
        }
    }

    /// Create an order event.
    pub fn order(event_id: impl Into<String>, event: OrderEventType, order: Order) -> Self {
        Self::Order {
            event_id: event_id.into(),
            event,
            order,
        }
    }

    /// Create a trade event.
    pub fn trade(event_id: impl Into<String>, trade: Trade) -> Self {
        Self::Trade {
            event_id: event_id.into(),
            trade,
        }
    }

    /// Create a position event.
    pub fn position(event_id: impl Into<String>, position: Position) -> Self {
        Self::Position {
            event_id: event_id.into(),
            position,
        }
    }

    /// Create an account event with the specified event type.
    pub fn account(event_id: impl Into<String>, event: AccountEventType, account: Account) -> Self {
        Self::Account {
            event_id: event_id.into(),
            event,
            account,
        }
    }

    /// Create an account balance updated event.
    pub fn account_balance_updated(event_id: impl Into<String>, account: Account) -> Self {
        Self::account(event_id, AccountEventType::BalanceUpdated, account)
    }

    /// Create a margin call event (positions were liquidated).
    pub fn account_margin_call(event_id: impl Into<String>, account: Account) -> Self {
        Self::account(event_id, AccountEventType::MarginCall, account)
    }

    /// Create a margin warning event (approaching margin limit).
    pub fn account_margin_warning(event_id: impl Into<String>, account: Account) -> Self {
        Self::account(event_id, AccountEventType::MarginWarning, account)
    }

    /// Extract the `event_id` from this message, if it has one.
    pub fn event_id(&self) -> Option<&str> {
        match self {
            Self::Candle { event_id, .. }
            | Self::Order { event_id, .. }
            | Self::Trade { event_id, .. }
            | Self::Position { event_id, .. }
            | Self::Account { event_id, .. }
            | Self::Error { event_id, .. } => Some(event_id),
            Self::Pong => None,
        }
    }
}

// =============================================================================
// Client -> Server Messages
// =============================================================================

/// Messages sent from strategies to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Ping message for keepalive.
    Ping { timestamp: u64 },

    /// Pong response to server ping (if used).
    Pong,

    /// Event acknowledgment - critical for simulation time synchronization.
    EventAck {
        /// Correlation ID for the ACK batch.
        correlation_id: String,
        /// List of event IDs that have been processed.
        events_processed: Vec<String>,
    },
}

impl ClientMessage {
    /// Create an `EventAck` message.
    pub fn event_ack(events_processed: Vec<String>) -> Self {
        Self::EventAck {
            correlation_id: uuid::Uuid::new_v4().to_string(),
            events_processed,
        }
    }

    /// Check if this is an ACK message.
    pub const fn is_ack(&self) -> bool {
        matches!(self, Self::EventAck { .. })
    }

    /// Get the correlation ID if this is an ACK message.
    pub fn correlation_id(&self) -> Option<&str> {
        match self {
            Self::EventAck { correlation_id, .. } => Some(correlation_id),
            _ => None,
        }
    }

    /// Get the list of processed event IDs if this is an ACK message.
    pub fn events_processed(&self) -> Option<&[String]> {
        match self {
            Self::EventAck {
                events_processed, ..
            } => Some(events_processed),
            _ => None,
        }
    }
}

// =============================================================================
// Event ID Generation
// =============================================================================

use std::sync::atomic::{AtomicU64, Ordering};

/// Global counter for generating unique event IDs.
static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique event ID.
///
/// Uses a simple incrementing counter prefixed with "evt-".
/// This is faster than UUID and sufficient for simulation purposes.
pub fn generate_event_id() -> String {
    let id = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("evt-{id}")
}

/// Reset the event ID counter (useful for tests).
#[cfg(test)]
pub fn reset_event_counter() {
    EVENT_COUNTER.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_ack_deserialization() {
        let json = r#"{"type":"event_ack","correlation_id":"batch-1","events_processed":["evt-1","evt-2"]}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_ack());
        assert_eq!(msg.correlation_id(), Some("batch-1"));
        assert_eq!(
            msg.events_processed(),
            Some(&["evt-1".to_string(), "evt-2".to_string()][..])
        );
    }

    #[test]
    fn test_generate_event_id() {
        reset_event_counter();
        assert_eq!(generate_event_id(), "evt-0");
        assert_eq!(generate_event_id(), "evt-1");
        assert_eq!(generate_event_id(), "evt-2");
    }
}
