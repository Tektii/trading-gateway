//! Price source abstraction for trailing stop tracking.
//!
//! This module defines the `PriceSource` trait for receiving real-time price
//! updates, along with implementations for different scenarios.
//!
//! # Implementations
//!
//! - [`WebSocketPriceSource`](super::ws_price_source::WebSocketPriceSource): Production implementation backed by WebSocket quote streams
//! - `MockPriceSource` in `tektii-gateway-test-support`: For testing with controlled price sequences

use async_trait::async_trait;
use rust_decimal::Decimal;
use tokio::sync::mpsc;

use crate::error::GatewayError;

/// A price update from a market data source.
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    /// Trading symbol
    pub symbol: String,
    /// Current price (typically last trade or mid-price)
    pub price: Decimal,
    /// Timestamp of the update (milliseconds since epoch)
    pub timestamp_ms: u64,
}

/// Trait for price sources used by trailing stop tracking.
///
/// Implementations provide real-time price updates for symbols that have
/// active trailing stop orders.
///
/// # Example
///
/// ```ignore
/// let price_source = WebSocketPriceSource::new(event_router);
/// let mut rx = price_source.subscribe("C:BTCUSD").await?;
///
/// while let Some(update) = rx.recv().await {
///     // Process price update
///     if update.price > peak_price {
///         // Adjust trailing stop
///     }
/// }
/// ```
#[async_trait]
pub trait PriceSource: Send + Sync {
    /// Subscribe to price updates for a symbol.
    ///
    /// Returns a receiver that yields price updates for the symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if subscription fails or is not supported.
    async fn subscribe(&self, symbol: &str) -> Result<mpsc::Receiver<PriceUpdate>, GatewayError>;

    /// Unsubscribe from a symbol.
    ///
    /// Called when no more trailing stops are active for the symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if unsubscription fails.
    async fn unsubscribe(&self, symbol: &str) -> Result<(), GatewayError>;

    /// Get the current price for a symbol (one-shot).
    ///
    /// Used to establish initial peak price when trailing stop is registered.
    ///
    /// # Errors
    ///
    /// Returns an error if the price is unavailable.
    async fn current_price(&self, symbol: &str) -> Result<Decimal, GatewayError>;
}

/// Ensures quote data is flowing for a given symbol.
///
/// Used by the trailing stop handler to guarantee that WebSocket price
/// updates arrive for symbols with active trailing stops.
#[async_trait]
pub trait QuoteSubscriber: Send + Sync {
    /// Ensure the provider is subscribed to quote events for this symbol.
    /// No-op if already subscribed.
    async fn ensure_subscribed(&self, symbol: &str) -> Result<(), GatewayError>;
}

/// No-op quote subscriber for testing or when auto-subscription is not needed.
#[derive(Debug, Default)]
pub struct NoOpQuoteSubscriber;

#[async_trait]
impl QuoteSubscriber for NoOpQuoteSubscriber {
    async fn ensure_subscribed(&self, _symbol: &str) -> Result<(), GatewayError> {
        Ok(())
    }
}

/// Placeholder price source for Phase 1 implementation.
///
/// This implementation returns `UnsupportedOperation` for all methods,
/// indicating that real-time price tracking is not yet available.
///
/// Trailing stops registered while using this price source will enter
/// the `PriceSourceUnavailable` state.
#[derive(Debug, Default)]
pub struct UnimplementedPriceSource {
    platform: String,
}

impl UnimplementedPriceSource {
    /// Creates a new unimplemented price source.
    #[must_use]
    pub fn new(platform: &str) -> Self {
        Self {
            platform: platform.to_string(),
        }
    }
}

#[async_trait]
impl PriceSource for UnimplementedPriceSource {
    async fn subscribe(&self, symbol: &str) -> Result<mpsc::Receiver<PriceUpdate>, GatewayError> {
        tracing::warn!(
            symbol,
            platform = %self.platform,
            "Trailing stop price subscription not yet implemented"
        );
        Err(GatewayError::UnsupportedOperation {
            operation: format!("Trailing stop price tracking for {symbol}"),
            provider: self.platform.clone(),
        })
    }

    async fn unsubscribe(&self, _symbol: &str) -> Result<(), GatewayError> {
        // No-op since we never actually subscribed
        Ok(())
    }

    async fn current_price(&self, symbol: &str) -> Result<Decimal, GatewayError> {
        Err(GatewayError::UnsupportedOperation {
            operation: format!("Current price lookup for {symbol}"),
            provider: self.platform.clone(),
        })
    }
}

/// Mock price source for core's own unit tests.
///
/// External consumers should use `tektii_gateway_test_support::mock_price_source::MockPriceSource`.
#[cfg(test)]
pub struct MockPriceSource {
    senders: std::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<PriceUpdate>>>,
    prices: std::sync::Mutex<std::collections::HashMap<String, Decimal>>,
}

#[cfg(test)]
impl MockPriceSource {
    #[must_use]
    pub fn new() -> Self {
        Self {
            senders: std::sync::Mutex::new(std::collections::HashMap::new()),
            prices: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn set_price(&self, symbol: &str, price: Decimal) {
        self.prices
            .lock()
            .unwrap()
            .insert(symbol.to_string(), price);
    }

    pub async fn emit_price(&self, symbol: &str, price: Decimal) {
        self.set_price(symbol, price);
        let sender = {
            let senders = self.senders.lock().unwrap();
            senders.get(symbol).cloned()
        };
        if let Some(sender) = sender {
            let update = PriceUpdate {
                symbol: symbol.to_string(),
                price,
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            };
            let _ = sender.send(update).await;
        }
    }
}

#[cfg(test)]
impl Default for MockPriceSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[async_trait]
impl PriceSource for MockPriceSource {
    async fn subscribe(&self, symbol: &str) -> Result<mpsc::Receiver<PriceUpdate>, GatewayError> {
        let (tx, rx) = mpsc::channel(100);
        self.senders.lock().unwrap().insert(symbol.to_string(), tx);
        Ok(rx)
    }

    async fn unsubscribe(&self, symbol: &str) -> Result<(), GatewayError> {
        self.senders.lock().unwrap().remove(symbol);
        Ok(())
    }

    async fn current_price(&self, symbol: &str) -> Result<Decimal, GatewayError> {
        self.prices
            .lock()
            .unwrap()
            .get(symbol)
            .copied()
            .ok_or_else(|| GatewayError::SymbolNotFound {
                symbol: symbol.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unimplemented_subscribe_returns_error() {
        let source = UnimplementedPriceSource::new("binance");
        let result = source.subscribe("C:BTCUSD").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            GatewayError::UnsupportedOperation { provider, .. } => {
                assert_eq!(provider, "binance");
            }
            _ => panic!("Expected UnsupportedOperation error"),
        }
    }

    #[tokio::test]
    async fn unimplemented_current_price_returns_error() {
        let source = UnimplementedPriceSource::new("binance");
        let result = source.current_price("C:BTCUSD").await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GatewayError::UnsupportedOperation { .. }
        ));
    }
}
