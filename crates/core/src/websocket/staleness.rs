//! Per-instrument data staleness tracking for broker WebSocket reconnection.
//!
//! Instruments are marked stale when the broker WebSocket disconnects, and
//! cleared when the first fresh tick arrives for that instrument after reconnect
//! (not just when the socket reconnects).

use std::time::Instant;

use chrono::{DateTime, Utc};
use dashmap::DashMap;

/// Tracks per-instrument data staleness during broker disconnects.
///
/// Thread-safe via `DashMap` — consistent with `StateManager` pattern.
#[derive(Debug)]
pub struct StalenessTracker {
    /// Instruments marked stale: symbol → (monotonic instant, wall-clock time).
    stale_instruments: DashMap<String, (Instant, DateTime<Utc>)>,
}

impl StalenessTracker {
    /// Create a new empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stale_instruments: DashMap::new(),
        }
    }

    /// Mark all given instruments as stale (called on broker disconnect).
    pub fn mark_all_stale(&self, symbols: &[String]) {
        let now = (Instant::now(), Utc::now());
        for symbol in symbols {
            self.stale_instruments.insert(symbol.clone(), now);
        }
    }

    /// Clear staleness for a single instrument (called when first tick arrives).
    ///
    /// Returns `Some(stale_since)` with the wall-clock time the instrument was
    /// marked stale, or `None` if it wasn't stale.
    pub fn mark_fresh(&self, symbol: &str) -> Option<DateTime<Utc>> {
        self.stale_instruments
            .remove(symbol)
            .map(|(_, (_, stale_since))| stale_since)
    }

    /// Check if an instrument's data is currently stale.
    #[must_use]
    pub fn is_stale(&self, symbol: &str) -> bool {
        self.stale_instruments.contains_key(symbol)
    }

    /// Get all currently stale instrument symbols.
    #[must_use]
    pub fn stale_instruments(&self) -> Vec<String> {
        self.stale_instruments
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Get all currently stale instruments with their stale-since timestamps.
    #[must_use]
    pub fn stale_instruments_with_times(&self) -> Vec<(String, DateTime<Utc>)> {
        self.stale_instruments
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().1))
            .collect()
    }

    /// Clear all staleness (e.g., on shutdown).
    pub fn clear(&self) {
        self.stale_instruments.clear();
    }
}

impl Default for StalenessTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_all_stale_marks_instruments() {
        let tracker = StalenessTracker::new();
        let symbols = vec![
            "AAPL".to_string(),
            "BTCUSD".to_string(),
            "EURUSD".to_string(),
        ];

        tracker.mark_all_stale(&symbols);

        assert!(tracker.is_stale("AAPL"));
        assert!(tracker.is_stale("BTCUSD"));
        assert!(tracker.is_stale("EURUSD"));
    }

    #[test]
    fn mark_fresh_clears_single_instrument() {
        let tracker = StalenessTracker::new();
        let symbols = vec!["AAPL".to_string(), "BTCUSD".to_string()];

        tracker.mark_all_stale(&symbols);

        // Clear AAPL — returns stale_since time
        let stale_since = tracker.mark_fresh("AAPL");
        assert!(stale_since.is_some());
        assert!(!tracker.is_stale("AAPL"));
        // BTCUSD still stale
        assert!(tracker.is_stale("BTCUSD"));
    }

    #[test]
    fn mark_fresh_returns_stale_since_time() {
        let tracker = StalenessTracker::new();
        let before = Utc::now();
        tracker.mark_all_stale(&["AAPL".to_string()]);
        let after = Utc::now();

        let stale_since = tracker.mark_fresh("AAPL").expect("should be stale");
        assert!(stale_since >= before);
        assert!(stale_since <= after);
    }

    #[test]
    fn mark_fresh_returns_none_on_second_call() {
        let tracker = StalenessTracker::new();
        tracker.mark_all_stale(&["AAPL".to_string()]);

        assert!(tracker.mark_fresh("AAPL").is_some()); // first tick — was stale
        assert!(tracker.mark_fresh("AAPL").is_none()); // second tick — already fresh
    }

    #[test]
    fn unknown_symbol_is_not_stale() {
        let tracker = StalenessTracker::new();
        assert!(!tracker.is_stale("UNKNOWN"));
        assert!(tracker.mark_fresh("UNKNOWN").is_none());
    }

    #[test]
    fn clear_removes_all_staleness() {
        let tracker = StalenessTracker::new();
        tracker.mark_all_stale(&["AAPL".to_string(), "BTCUSD".to_string()]);

        tracker.clear();

        assert!(!tracker.is_stale("AAPL"));
        assert!(!tracker.is_stale("BTCUSD"));
        assert!(tracker.stale_instruments().is_empty());
    }

    #[test]
    fn stale_instruments_returns_all_stale() {
        let tracker = StalenessTracker::new();
        tracker.mark_all_stale(&["AAPL".to_string(), "BTCUSD".to_string()]);

        let stale = tracker.stale_instruments();
        assert_eq!(stale.len(), 2);
        assert!(stale.contains(&"AAPL".to_string()));
        assert!(stale.contains(&"BTCUSD".to_string()));
    }

    #[test]
    fn stale_instruments_with_times_returns_all() {
        let tracker = StalenessTracker::new();
        let before = Utc::now();
        tracker.mark_all_stale(&["AAPL".to_string(), "BTCUSD".to_string()]);
        let after = Utc::now();

        let stale = tracker.stale_instruments_with_times();
        assert_eq!(stale.len(), 2);

        for (symbol, stale_since) in &stale {
            assert!(
                symbol == "AAPL" || symbol == "BTCUSD",
                "unexpected symbol: {symbol}"
            );
            assert!(*stale_since >= before);
            assert!(*stale_since <= after);
        }
    }
}
