//! Mock price source for testing trailing stop behavior.
//!
//! Allows tests to inject controlled price sequences and verify
//! trailing stop tracking logic.

use async_trait::async_trait;
use rust_decimal::Decimal;
use tokio::sync::mpsc;

use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::trailing_stop::{PriceSource, PriceUpdate};

/// Mock price source for testing.
///
/// Allows tests to inject controlled price sequences and verify
/// trailing stop behavior.
pub struct MockPriceSource {
    /// Senders for each subscribed symbol
    senders: std::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<PriceUpdate>>>,
    /// Current prices for each symbol
    prices: std::sync::Mutex<std::collections::HashMap<String, Decimal>>,
}

impl MockPriceSource {
    /// Creates a new mock price source.
    #[must_use]
    pub fn new() -> Self {
        Self {
            senders: std::sync::Mutex::new(std::collections::HashMap::new()),
            prices: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Sets the current price for a symbol.
    pub fn set_price(&self, symbol: &str, price: Decimal) {
        let mut prices = self.prices.lock().unwrap();
        prices.insert(symbol.to_string(), price);
    }

    /// Sends a price update to all subscribers of a symbol.
    pub async fn emit_price(&self, symbol: &str, price: Decimal) {
        self.set_price(symbol, price);

        let sender = {
            let senders = self.senders.lock().unwrap();
            senders.get(symbol).cloned()
        };

        if let Some(sender) = sender {
            let epoch_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis();
            let update = PriceUpdate {
                symbol: symbol.to_string(),
                price,
                timestamp_ms: u64::try_from(epoch_ms).unwrap_or(u64::MAX),
            };
            let _ = sender.send(update).await;
        }
    }
}

impl Default for MockPriceSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PriceSource for MockPriceSource {
    async fn subscribe(&self, symbol: &str) -> Result<mpsc::Receiver<PriceUpdate>, GatewayError> {
        let (tx, rx) = mpsc::channel(100);
        {
            let mut senders = self.senders.lock().unwrap();
            senders.insert(symbol.to_string(), tx);
        }
        Ok(rx)
    }

    async fn unsubscribe(&self, symbol: &str) -> Result<(), GatewayError> {
        let mut senders = self.senders.lock().unwrap();
        senders.remove(symbol);
        Ok(())
    }

    async fn current_price(&self, symbol: &str) -> Result<Decimal, GatewayError> {
        let prices = self.prices.lock().unwrap();
        prices
            .get(symbol)
            .copied()
            .ok_or_else(|| GatewayError::SymbolNotFound {
                symbol: symbol.to_string(),
            })
    }
}
