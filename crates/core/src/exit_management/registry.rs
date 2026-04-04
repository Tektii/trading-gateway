//! Registry for per-adapter `ExitHandler` instances.
//!
//! Provides centralized access to `ExitHandler` instances from all registered adapters,
//! used by `ReconciliationService` for periodic order status checks.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::ExitHandler;
use crate::models::TradingPlatform;

/// Registry for per-adapter `ExitHandler` instances.
///
/// This allows services like `ReconciliationService` to iterate over all
/// registered `ExitHandler` instances without needing direct adapter references.
#[derive(Debug, Default)]
pub struct ExitHandlerRegistry {
    handlers: DashMap<TradingPlatform, Arc<ExitHandler>>,
}

impl ExitHandlerRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handlers: DashMap::new(),
        }
    }

    /// Register an `ExitHandler` for a platform.
    ///
    /// If a handler was already registered for this platform, it is replaced.
    pub fn register(&self, platform: TradingPlatform, handler: Arc<ExitHandler>) {
        self.handlers.insert(platform, handler);
    }

    /// Get the `ExitHandler` for a platform.
    #[must_use]
    pub fn get(&self, platform: &TradingPlatform) -> Option<Arc<ExitHandler>> {
        self.handlers.get(platform).map(|r| Arc::clone(r.value()))
    }

    /// Get all registered handlers.
    #[must_use]
    pub fn all(&self) -> Vec<(TradingPlatform, Arc<ExitHandler>)> {
        self.handlers
            .iter()
            .map(|r| (*r.key(), Arc::clone(r.value())))
            .collect()
    }

    /// Get the number of registered handlers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Check if the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Get the total count of pending entries across all handlers.
    #[must_use]
    pub fn total_pending_count(&self) -> usize {
        self.handlers
            .iter()
            .map(|r| r.value().pending_count())
            .sum()
    }

    /// Get the total count of primary orders with pending entries across all handlers.
    #[must_use]
    pub fn total_primary_orders_count(&self) -> usize {
        self.handlers
            .iter()
            .map(|r| r.value().primary_orders_count())
            .sum()
    }

    /// Spawn cleanup tasks for all registered handlers.
    ///
    /// Returns a vector of join handles for the spawned tasks.
    pub fn spawn_cleanup_tasks(
        &self,
        cancel_token: &CancellationToken,
        interval: Duration,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        self.handlers
            .iter()
            .map(|r| {
                let handler = Arc::clone(r.value());
                let platform = *r.key();
                let token = cancel_token.clone();

                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            () = token.cancelled() => {
                                info!(?platform, "ExitHandler cleanup task cancelled");
                                break;
                            }
                            () = tokio::time::sleep(interval) => {
                                let expired = handler.cleanup_expired_entries();
                                if expired > 0 {
                                    info!(?platform, expired, "Cleaned up expired exit entries");
                                }
                            }
                        }
                    }
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateManager;

    fn create_test_handler(platform: TradingPlatform) -> Arc<ExitHandler> {
        let state_manager = Arc::new(StateManager::new());
        Arc::new(ExitHandler::with_defaults(state_manager, platform))
    }

    #[test]
    fn test_register_and_get() {
        let registry = ExitHandlerRegistry::new();
        let handler = create_test_handler(TradingPlatform::AlpacaLive);

        registry.register(TradingPlatform::AlpacaLive, handler);

        assert!(registry.get(&TradingPlatform::AlpacaLive).is_some());
        assert!(registry.get(&TradingPlatform::BinanceSpotLive).is_none());
    }

    #[test]
    fn test_all_handlers() {
        let registry = ExitHandlerRegistry::new();
        registry.register(
            TradingPlatform::AlpacaLive,
            create_test_handler(TradingPlatform::AlpacaLive),
        );
        registry.register(
            TradingPlatform::BinanceSpotLive,
            create_test_handler(TradingPlatform::BinanceSpotLive),
        );

        let all = registry.all();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_len_and_is_empty() {
        let registry = ExitHandlerRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        registry.register(
            TradingPlatform::AlpacaLive,
            create_test_handler(TradingPlatform::AlpacaLive),
        );
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }
}
