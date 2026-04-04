//! WebSocket-backed price source for trailing stop tracking.
//!
//! Bridges real-time market data from WebSocket provider streams into the
//! trailing stop tracking system. Uses per-symbol `broadcast` channels for
//! fan-out to multiple trailing stop entries tracking the same symbol.

use async_trait::async_trait;
use dashmap::DashMap;
use rust_decimal::Decimal;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::error::GatewayError;

use super::price_source::{PriceSource, PriceUpdate};

/// Default broadcast channel capacity per symbol.
const DEFAULT_CHANNEL_CAPACITY: usize = 128;

/// Capacity of the mpsc bridge channel per subscriber.
const BRIDGE_CHANNEL_CAPACITY: usize = 64;

/// Real-time price source backed by WebSocket market data streams.
///
/// Receives price updates via [`handle_quote`](Self::handle_quote) (called from
/// the provider event loop) and fans them out to trailing stop tracking tasks
/// via per-symbol broadcast channels.
///
/// # Design
///
/// - **Per-symbol broadcast**: Each symbol gets a `broadcast::Sender<PriceUpdate>`.
///   Multiple trailing stop entries on the same symbol share the broadcast.
/// - **Bridge to mpsc**: The `PriceSource` trait returns `mpsc::Receiver`, so each
///   `subscribe()` call spawns a small bridging task that converts broadcast → mpsc.
/// - **Lazy cleanup**: When all subscribers for a symbol drop, the next `handle_quote`
///   detects zero receivers and removes the broadcast entry.
/// - **Price cache**: Latest price per symbol is cached for `current_price()` one-shot queries.
pub struct WebSocketPriceSource {
    /// Per-symbol broadcast senders for fan-out to subscribers.
    channels: DashMap<String, broadcast::Sender<PriceUpdate>>,

    /// Latest price cache for `current_price()` queries.
    /// Value: `(price, timestamp_ms)`
    latest_prices: DashMap<String, (Decimal, u64)>,

    /// Broadcast channel capacity per symbol.
    channel_capacity: usize,
}

impl WebSocketPriceSource {
    /// Creates a new WebSocket price source with default channel capacity.
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: DashMap::new(),
            latest_prices: DashMap::new(),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }

    /// Creates a new WebSocket price source with custom channel capacity.
    #[must_use]
    pub fn with_capacity(channel_capacity: usize) -> Self {
        Self {
            channels: DashMap::new(),
            latest_prices: DashMap::new(),
            channel_capacity,
        }
    }

    /// Process an incoming price quote.
    ///
    /// Called synchronously from the provider event loop when a `QuoteData`
    /// message arrives. Updates the price cache and fans out to all subscribers
    /// for the symbol.
    ///
    /// # Arguments
    ///
    /// * `symbol` - Trading symbol (e.g., "BTCUSDT")
    /// * `price` - Mid price (bid+ask)/2
    /// * `timestamp_ms` - Timestamp in milliseconds since epoch
    pub fn handle_quote(&self, symbol: &str, price: Decimal, timestamp_ms: u64) {
        // Update cache
        self.latest_prices
            .insert(symbol.to_string(), (price, timestamp_ms));

        // Fan out to subscribers (if any)
        // Clone the sender outside the DashMap ref to avoid holding ref across send
        let sender = self.channels.get(symbol).map(|r| r.clone());
        if let Some(sender) = sender {
            let update = PriceUpdate {
                symbol: symbol.to_string(),
                price,
                timestamp_ms,
            };
            if sender.send(update).is_err() {
                // Zero receivers — lazy cleanup
                debug!(symbol, "No trailing stop subscribers, removing channel");
                self.channels.remove(symbol);
            }
        }
    }

    /// Returns the number of symbols with active subscribers.
    #[must_use]
    pub fn active_symbol_count(&self) -> usize {
        self.channels.len()
    }

    /// Returns the number of symbols with cached prices.
    #[must_use]
    pub fn cached_price_count(&self) -> usize {
        self.latest_prices.len()
    }
}

impl Default for WebSocketPriceSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PriceSource for WebSocketPriceSource {
    async fn subscribe(&self, symbol: &str) -> Result<mpsc::Receiver<PriceUpdate>, GatewayError> {
        // Get or create broadcast channel for this symbol
        let broadcast_rx = {
            let sender = self
                .channels
                .entry(symbol.to_string())
                .or_insert_with(|| broadcast::channel(self.channel_capacity).0);
            sender.subscribe()
        };

        // Bridge broadcast -> mpsc to match trait signature
        let (tx, rx) = mpsc::channel(BRIDGE_CHANNEL_CAPACITY);
        let symbol_owned = symbol.to_string();
        tokio::spawn(bridge_broadcast_to_mpsc(broadcast_rx, tx, symbol_owned));

        debug!(symbol, "Trailing stop price subscription created");
        Ok(rx)
    }

    async fn unsubscribe(&self, _symbol: &str) -> Result<(), GatewayError> {
        // Cleanup happens automatically via drop semantics:
        // 1. Tracking task exits → drops mpsc::Receiver
        // 2. Bridge task detects closed channel → exits, drops broadcast::Receiver
        // 3. Next handle_quote() detects zero receivers → removes broadcast entry
        Ok(())
    }

    async fn current_price(&self, symbol: &str) -> Result<Decimal, GatewayError> {
        self.latest_prices
            .get(symbol)
            .map(|entry| entry.0)
            .ok_or_else(|| GatewayError::SymbolNotFound {
                symbol: symbol.to_string(),
            })
    }
}

/// Bridge task: forwards price updates from broadcast to mpsc channel.
///
/// Exits when either the broadcast sender is dropped or the mpsc receiver
/// is dropped (subscriber gone). Handles lagged receivers gracefully by
/// skipping old ticks.
async fn bridge_broadcast_to_mpsc(
    mut broadcast_rx: broadcast::Receiver<PriceUpdate>,
    mpsc_tx: mpsc::Sender<PriceUpdate>,
    symbol: String,
) {
    loop {
        match broadcast_rx.recv().await {
            Ok(update) => {
                if mpsc_tx.is_closed() {
                    break;
                }
                // try_send: drop tick if subscriber is slow (next tick arrives in ~100ms)
                if let Err(mpsc::error::TrySendError::Closed(_)) = mpsc_tx.try_send(update) {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    symbol,
                    lagged = n,
                    "Price subscriber lagged, skipping old ticks"
                );
                // Continue — next recv() will get a fresh tick
            }
            Err(broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }
    debug!(symbol, "Price bridge task exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn subscribe_receives_price_updates() {
        let source = WebSocketPriceSource::new();
        let mut rx = source.subscribe("BTCUSDT").await.unwrap();

        source.handle_quote("BTCUSDT", dec!(50000), 1000);

        let update = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update.symbol, "BTCUSDT");
        assert_eq!(update.price, dec!(50000));
        assert_eq!(update.timestamp_ms, 1000);
    }

    #[tokio::test]
    async fn multiple_subscribers_same_symbol() {
        let source = WebSocketPriceSource::new();
        let mut rx1 = source.subscribe("BTCUSDT").await.unwrap();
        let mut rx2 = source.subscribe("BTCUSDT").await.unwrap();

        source.handle_quote("BTCUSDT", dec!(50000), 1000);

        let update1 = timeout(Duration::from_secs(1), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let update2 = timeout(Duration::from_secs(1), rx2.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(update1.price, dec!(50000));
        assert_eq!(update2.price, dec!(50000));
    }

    #[tokio::test]
    async fn current_price_returns_cached_value() {
        let source = WebSocketPriceSource::new();
        source.handle_quote("BTCUSDT", dec!(50000), 1000);

        let price = source.current_price("BTCUSDT").await.unwrap();
        assert_eq!(price, dec!(50000));
    }

    #[tokio::test]
    async fn current_price_returns_latest_value() {
        let source = WebSocketPriceSource::new();
        source.handle_quote("BTCUSDT", dec!(50000), 1000);
        source.handle_quote("BTCUSDT", dec!(51000), 2000);

        let price = source.current_price("BTCUSDT").await.unwrap();
        assert_eq!(price, dec!(51000));
    }

    #[tokio::test]
    async fn current_price_unknown_symbol_returns_error() {
        let source = WebSocketPriceSource::new();
        let result = source.current_price("UNKNOWN").await;

        assert!(matches!(
            result.unwrap_err(),
            GatewayError::SymbolNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn different_symbols_are_independent() {
        let source = WebSocketPriceSource::new();
        let mut rx_btc = source.subscribe("BTCUSDT").await.unwrap();
        let mut rx_eth = source.subscribe("ETHUSDT").await.unwrap();

        source.handle_quote("BTCUSDT", dec!(50000), 1000);

        let update = timeout(Duration::from_secs(1), rx_btc.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update.symbol, "BTCUSDT");

        // ETH should not receive BTC update
        let eth_result = timeout(Duration::from_millis(50), rx_eth.recv()).await;
        assert!(eth_result.is_err(), "ETH should not receive BTC updates");
    }

    #[tokio::test]
    async fn lazy_cleanup_on_zero_subscribers() {
        let source = WebSocketPriceSource::new();

        {
            let _rx = source.subscribe("BTCUSDT").await.unwrap();
            assert_eq!(source.active_symbol_count(), 1);
            // _rx drops here
        }

        // First handle_quote: wakes bridge task, which detects closed mpsc and exits
        source.handle_quote("BTCUSDT", dec!(50000), 1000);
        // Allow bridge task to exit and drop its broadcast::Receiver
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Second handle_quote: broadcast send() detects zero receivers → removes entry
        source.handle_quote("BTCUSDT", dec!(50001), 2000);

        assert_eq!(source.active_symbol_count(), 0);
        // Price cache should still be populated
        assert_eq!(source.cached_price_count(), 1);
    }

    #[tokio::test]
    async fn handle_quote_without_subscribers_is_noop() {
        let source = WebSocketPriceSource::new();

        // Should not panic or error
        source.handle_quote("BTCUSDT", dec!(50000), 1000);

        // Price should still be cached
        let price = source.current_price("BTCUSDT").await.unwrap();
        assert_eq!(price, dec!(50000));
    }

    #[tokio::test]
    async fn unsubscribe_is_noop() {
        let source = WebSocketPriceSource::new();
        let result = source.unsubscribe("BTCUSDT").await;
        assert!(result.is_ok());
    }
}
