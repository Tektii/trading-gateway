//! Mock `WebSocketProvider` for integration tests.
//!
//! Provides a controllable WebSocket provider that can simulate disconnects,
//! successful reconnects, auth failures, and other scenarios. Use with
//! `ProviderRegistry` to test the full reconnection orchestration.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;

use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{EventAckMessage, WsMessage};
use tektii_gateway_core::websocket::provider::{EventStream, ProviderConfig, WebSocketProvider};

/// Shared state that survives after the provider is moved into `ProviderRegistry`.
///
/// Clone the `MockProviderHandle` before passing the provider to `register_provider()`,
/// then use the handle for assertions (e.g., `reconnect_call_count()`).
#[derive(Clone)]
pub struct MockProviderHandle {
    reconnect_call_count: Arc<AtomicU32>,
}

impl MockProviderHandle {
    /// How many times `reconnect()` has been called.
    #[must_use]
    pub fn reconnect_call_count(&self) -> u32 {
        self.reconnect_call_count.load(Ordering::SeqCst)
    }
}

/// A mock WebSocket provider for integration tests.
///
/// Enqueue reconnect results with `push_reconnect_result()`, then register
/// with `ProviderRegistry`. Use `handle()` to get a cloneable handle for
/// post-registration assertions.
pub struct MockWebSocketProvider {
    supports_reconnection: bool,
    reconnect_results: Mutex<VecDeque<Result<EventStream, WebSocketError>>>,
    reconnect_call_count: Arc<AtomicU32>,
}

impl MockWebSocketProvider {
    /// Create a new mock provider.
    #[must_use]
    pub fn new(supports_reconnection: bool) -> Self {
        Self {
            supports_reconnection,
            reconnect_results: Mutex::new(VecDeque::new()),
            reconnect_call_count: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Get a handle for post-registration assertions.
    #[must_use]
    pub fn handle(&self) -> MockProviderHandle {
        MockProviderHandle {
            reconnect_call_count: Arc::clone(&self.reconnect_call_count),
        }
    }

    /// Enqueue a result that `reconnect()` will return on its next call.
    pub fn push_reconnect_result(&self, result: Result<EventStream, WebSocketError>) {
        self.reconnect_results
            .lock()
            .expect("lock")
            .push_back(result);
    }

    /// Create a controllable event stream pair.
    ///
    /// Drop the sender to simulate a broker disconnect.
    #[must_use]
    pub fn make_event_stream() -> (mpsc::UnboundedSender<WsMessage>, EventStream) {
        mpsc::unbounded_channel()
    }
}

#[async_trait]
impl WebSocketProvider for MockWebSocketProvider {
    async fn connect(&self, _config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        unimplemented!(
            "MockWebSocketProvider::connect — use make_event_stream() and register_provider() directly"
        )
    }

    async fn subscribe(
        &self,
        _symbols: Vec<String>,
        _event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        Ok(())
    }

    async fn unsubscribe(&self, _symbols: Vec<String>) -> Result<(), WebSocketError> {
        Ok(())
    }

    async fn handle_ack(&self, _ack: EventAckMessage) -> Result<(), WebSocketError> {
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), WebSocketError> {
        Ok(())
    }

    async fn reconnect(&self) -> Result<EventStream, WebSocketError> {
        self.reconnect_call_count.fetch_add(1, Ordering::SeqCst);
        self.reconnect_results
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| {
                Err(WebSocketError::ConnectionFailed(
                    "MockWebSocketProvider: no more queued results".into(),
                ))
            })
    }

    fn supports_reconnection(&self) -> bool {
        self.supports_reconnection
    }
}
