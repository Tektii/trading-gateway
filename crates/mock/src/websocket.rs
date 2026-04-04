//! Mock WebSocket provider — streams synthetic price events for subscribed symbols.
//! Also exposes an event sink that the adapter uses to push order events into
//! the same stream, so they reach strategy clients via the ProviderRegistry.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{EventAckMessage, WsMessage};
use tektii_gateway_core::websocket::provider::{EventStream, ProviderConfig, WebSocketProvider};

use crate::price::PriceGenerator;

/// Shared event sink that the adapter and WS provider both use to push events
/// into the provider's event stream (which the ProviderRegistry consumes).
/// Set when the WS provider connects; cleared on disconnect.
pub type EventSink = Arc<RwLock<Option<mpsc::UnboundedSender<WsMessage>>>>;

/// Create a new shared event sink. Pass one clone to the adapter and one to the WS provider.
pub fn new_event_sink() -> EventSink {
    Arc::new(RwLock::new(None))
}

pub struct MockWebSocketProvider {
    price_generator: Arc<PriceGenerator>,
    subscribed_symbols: Arc<RwLock<HashSet<String>>>,
    cancel_token: RwLock<CancellationToken>,
    /// Shared with the adapter so order events flow through the same pipeline as quotes.
    event_sink: EventSink,
}

impl MockWebSocketProvider {
    pub fn new(price_generator: Arc<PriceGenerator>, event_sink: EventSink) -> Self {
        Self {
            price_generator,
            subscribed_symbols: Arc::new(RwLock::new(HashSet::new())),
            cancel_token: RwLock::new(CancellationToken::new()),
            event_sink,
        }
    }

    fn start_streaming(&self, tx: mpsc::UnboundedSender<WsMessage>) {
        let price_gen = Arc::clone(&self.price_generator);
        let symbols = Arc::clone(&self.subscribed_symbols);
        let token = self.cancel_token.read().clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));

            loop {
                tokio::select! {
                    () = token.cancelled() => {
                        debug!("Mock WebSocket streaming task cancelled");
                        break;
                    }
                    _ = interval.tick() => {
                        let current_symbols: Vec<String> = symbols.read().iter().cloned().collect();
                        for symbol in &current_symbols {
                            let bar = price_gen.get_bar(symbol);
                            if tx.send(WsMessage::candle(bar)).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
    }
}

#[async_trait]
impl WebSocketProvider for MockWebSocketProvider {
    async fn connect(&self, config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut subs = self.subscribed_symbols.write();
            for sym in &config.symbols {
                subs.insert(sym.clone());
            }
        }

        // Store sender in the shared event sink so the adapter can push order events
        *self.event_sink.write() = Some(tx.clone());

        self.start_streaming(tx);

        info!(
            symbols = ?config.symbols,
            "Mock WebSocket connected — streaming synthetic candles every 2s"
        );

        Ok(rx)
    }

    async fn subscribe(
        &self,
        symbols: Vec<String>,
        _event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        let mut subs = self.subscribed_symbols.write();
        for sym in symbols {
            subs.insert(sym);
        }
        Ok(())
    }

    async fn unsubscribe(&self, symbols: Vec<String>) -> Result<(), WebSocketError> {
        let mut subs = self.subscribed_symbols.write();
        for sym in &symbols {
            subs.remove(sym);
        }
        Ok(())
    }

    async fn handle_ack(&self, _ack: EventAckMessage) -> Result<(), WebSocketError> {
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), WebSocketError> {
        let old_token = {
            let mut token = self.cancel_token.write();
            let old = token.clone();
            *token = CancellationToken::new();
            old
        };
        old_token.cancel();
        *self.event_sink.write() = None;
        Ok(())
    }

    async fn reconnect(&self) -> Result<EventStream, WebSocketError> {
        let old_token = {
            let mut token = self.cancel_token.write();
            let old = token.clone();
            *token = CancellationToken::new();
            old
        };
        old_token.cancel();

        let (tx, rx) = mpsc::unbounded_channel();
        *self.event_sink.write() = Some(tx.clone());

        self.start_streaming(tx);

        Ok(rx)
    }

    fn supports_reconnection(&self) -> bool {
        true
    }
}
