//! Entry↔exit links reported by brokers that manage brackets natively.
//!
//! Alpaca reports its bracket legs as a `legs` array on the entry order; Saxo
//! reports them as `RelatedOpenOrders`. In both cases the link only ever points
//! *down*: a leg fetched or streamed on its own carries no reference back to the
//! entry it protects. An adapter therefore has to remember the link at the one
//! moment it is visible — when it translates an entry that declares legs — and
//! answer from that record afterwards.
//!
//! This is the native-bracket counterpart to the
//! [`ExitHandler`](crate::exit_management::handler::ExitHandler)'s tracking of
//! the legs the gateway synthesizes itself, and it follows the same lifetime
//! rule: the link is dropped once the leg resolves.

use dashmap::DashMap;

/// Records which entry order each broker-native exit leg belongs to.
///
/// Held in memory only. A restart, or a leg first seen before its entry, reads
/// back `None` until the entry is observed again; the gateway then falls back to
/// reporting no link rather than guessing one.
#[derive(Debug, Default)]
pub struct BracketLinks {
    by_exit_order: DashMap<String, String>,
}

impl BracketLinks {
    /// Create an empty set of links.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `exit_order_id` exits `parent_order_id`.
    pub fn record(&self, exit_order_id: impl Into<String>, parent_order_id: impl Into<String>) {
        self.by_exit_order
            .insert(exit_order_id.into(), parent_order_id.into());
    }

    /// The entry order `order_id` exits, if one has been recorded.
    #[must_use]
    pub fn parent_of(&self, order_id: &str) -> Option<String> {
        self.by_exit_order
            .get(order_id)
            .map(|entry| entry.value().clone())
    }

    /// Forget the link for a leg that has reached a terminal state.
    ///
    /// Callers resolve the parent *before* releasing, so the response reporting
    /// the terminal state still carries the link — that event is exactly when a
    /// strategy needs to know which entry the leg closed.
    ///
    /// Both observers release: the adapter's own translation of a REST
    /// response, and
    /// [`TradingAdapter::parent_order_id_for`](crate::adapter::TradingAdapter::parent_order_id_for)
    /// when an order event reports the resolution. The event path is the one
    /// that normally fires, so the map stays bounded by live brackets rather
    /// than growing with every bracket ever seen.
    pub fn release(&self, order_id: &str) {
        self.by_exit_order.remove(order_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_resolves_a_link() {
        let links = BracketLinks::new();
        links.record("sl-1", "entry-1");

        assert_eq!(links.parent_of("sl-1"), Some("entry-1".to_string()));
    }

    #[test]
    fn unknown_order_has_no_parent() {
        let links = BracketLinks::new();
        links.record("sl-1", "entry-1");

        assert_eq!(links.parent_of("entry-1"), None);
        assert_eq!(links.parent_of("other"), None);
    }

    #[test]
    fn release_drops_the_link() {
        let links = BracketLinks::new();
        links.record("sl-1", "entry-1");
        links.release("sl-1");

        assert_eq!(links.parent_of("sl-1"), None);
    }

    #[test]
    fn re_recording_a_leg_replaces_its_parent() {
        let links = BracketLinks::new();
        links.record("sl-1", "entry-1");
        links.record("sl-1", "entry-2");

        assert_eq!(links.parent_of("sl-1"), Some("entry-2".to_string()));
    }
}
