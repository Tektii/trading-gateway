//! Correlation store for mapping provider order IDs to gateway-assigned correlation IDs.
//!
//! The gateway assigns a UUID correlation ID to every order at submission time.
//! This store maps provider order IDs to correlation IDs so they can be included
//! in API responses, WebSocket events, and structured logs.

use dashmap::DashMap;

/// Maps provider order IDs to gateway-assigned correlation IDs.
#[derive(Debug, Default)]
pub struct CorrelationStore {
    /// provider_order_id → correlation_id
    mappings: DashMap<String, String>,
}

impl CorrelationStore {
    /// Create a new empty correlation store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mappings: DashMap::new(),
        }
    }

    /// Store a correlation mapping for an order.
    pub fn store(&self, order_id: &str, correlation_id: &str) {
        self.mappings
            .insert(order_id.to_string(), correlation_id.to_string());
    }

    /// Look up the correlation ID for an order.
    #[must_use]
    pub fn get(&self, order_id: &str) -> Option<String> {
        self.mappings.get(order_id).map(|v| v.value().clone())
    }

    /// Remove the correlation mapping for an order (e.g., on terminal state).
    pub fn remove(&self, order_id: &str) -> Option<String> {
        self.mappings.remove(order_id).map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_get_round_trip() {
        let store = CorrelationStore::new();
        store.store("order-1", "corr-abc");
        assert_eq!(store.get("order-1"), Some("corr-abc".to_string()));
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let store = CorrelationStore::new();
        assert_eq!(store.get("nonexistent"), None);
    }

    #[test]
    fn remove_clears_entry() {
        let store = CorrelationStore::new();
        store.store("order-1", "corr-abc");
        let removed = store.remove("order-1");
        assert_eq!(removed, Some("corr-abc".to_string()));
        assert_eq!(store.get("order-1"), None);
    }

    #[test]
    fn remove_returns_none_for_unknown_id() {
        let store = CorrelationStore::new();
        assert_eq!(store.remove("nonexistent"), None);
    }
}
