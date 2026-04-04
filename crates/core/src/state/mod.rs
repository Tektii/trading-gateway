//! State Manager for per-adapter order and position caching.
//!
//! The State Manager maintains an in-memory cache of open orders and positions
//! for internal use by the Exit Handler and adapter logic. It is NOT used for
//! REST responses - those always query the provider directly.
//!
//! # Key Principles
//!
//! - **Provider is source of truth**: State Manager is cache only
//! - **Used for internal operations**: Exit Handler queries, order/position correlation
//! - **Concurrent access**: Uses `DashMap` for lock-free operations
//! - **Sync on connect**: Full state rebuilt from provider on connect/reconnect
//!
//! # Example
//!
//! ```ignore
//! use tektii_gateway_core::state::StateManager;
//! use tektii_gateway_core::models::{Order, Position};
//!
//! let state = StateManager::new();
//!
//! // Sync from provider on connect
//! state.sync_from_provider(orders, positions);
//!
//! // Update on events
//! state.upsert_order(&order);
//! state.update_order_fill("order-1", dec!(50), OrderStatus::PartiallyFilled);
//!
//! // Query for Exit Handler
//! let orders = state.get_orders_for_position("pos-1");
//! ```

use crate::models::{Order, OrderStatus, Position, PositionSide, Side};
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::collections::HashSet;

/// Minimal order data for internal cache operations (exit tracking, position correlation).
#[derive(Debug, Clone)]
pub struct CachedOrder {
    pub order_id: String,
    pub symbol: String,
    pub side: Side,
    pub quantity: Decimal,
    pub filled_quantity: Decimal,
    pub status: OrderStatus,
    pub position_id: Option<String>,
    pub is_exit_order: bool,
    pub parent_order_id: Option<String>,
    /// For OCO pairs (SL ↔ TP)
    pub sibling_order_id: Option<String>,
    pub oco_group_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CachedPosition {
    pub position_id: String,
    pub symbol: String,
    pub side: PositionSide,
    pub quantity: Decimal,
}

/// Record of the first exit fill in an OCO group, used for double-exit detection.
#[derive(Debug, Clone)]
pub struct OcoFillRecord {
    pub first_filled_order_id: String,
    pub first_filled_qty: Decimal,
    pub symbol: String,
    pub side: Side,
}

/// Cache for Exit Handler and adapter operations. Provider is always source of truth.
pub struct StateManager {
    orders: DashMap<String, CachedOrder>,
    positions: DashMap<String, CachedPosition>,
    // Indexes for fast lookups
    orders_by_position: DashMap<String, HashSet<String>>,
    orders_by_symbol: DashMap<String, HashSet<String>>,
    /// For netting mode - one position per symbol
    position_by_symbol: DashMap<String, String>,
    oco_groups: DashMap<String, HashSet<String>>,
    /// Tracks first exit fill per OCO group for double-exit detection.
    oco_fills: DashMap<String, OcoFillRecord>,
}

impl StateManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            orders: DashMap::new(),
            positions: DashMap::new(),
            orders_by_position: DashMap::new(),
            orders_by_symbol: DashMap::new(),
            position_by_symbol: DashMap::new(),
            oco_groups: DashMap::new(),
            oco_fills: DashMap::new(),
        }
    }

    // =========================================================================
    // Lifecycle
    // =========================================================================

    /// Clear and repopulate from provider data (on connect/reconnect).
    ///
    /// This performs a full sync, replacing all cached state with fresh data
    /// from the provider. Exit order tracking is NOT preserved - that's handled
    /// separately by the Exit Handler. OCO groups are also cleared since they
    /// are client-side concepts not stored by providers.
    pub fn sync_from_provider(&self, orders: Vec<Order>, positions: Vec<Position>) {
        // Clear existing state
        self.orders.clear();
        self.positions.clear();
        self.orders_by_position.clear();
        self.orders_by_symbol.clear();
        self.position_by_symbol.clear();
        self.oco_groups.clear();
        self.oco_fills.clear();

        // Repopulate from provider data
        for order in orders {
            self.upsert_order(&order);
        }
        for position in positions {
            self.upsert_position(&position);
        }
    }

    // =========================================================================
    // Order Operations
    // =========================================================================

    /// Insert or update an order.
    ///
    /// If the order's `position_id` has changed from a previous upsert,
    /// the old position index will be cleaned up automatically.
    ///
    /// Uses atomic insert to avoid TOCTOU race conditions.
    pub fn upsert_order(&self, order: &Order) {
        let cached = CachedOrder {
            order_id: order.id.clone(),
            symbol: order.symbol.clone(),
            side: order.side,
            quantity: order.quantity,
            filled_quantity: order.filled_quantity,
            status: order.status,
            position_id: order.position_id.clone(),
            is_exit_order: false,
            parent_order_id: None,
            sibling_order_id: None,
            oco_group_id: order.oco_group_id.clone(),
        };

        // Atomically insert and get old value (eliminates TOCTOU race)
        let old_cached = self.orders.insert(order.id.clone(), cached);

        // Clean up old indexes based on actual old value
        if let Some(ref old) = old_cached {
            if old.position_id != order.position_id
                && let Some(ref old_pos_id) = old.position_id
                && let Some(mut orders) = self.orders_by_position.get_mut(old_pos_id)
            {
                orders.remove(&order.id);
            }
            if old.symbol != order.symbol
                && let Some(mut orders) = self.orders_by_symbol.get_mut(&old.symbol)
            {
                orders.remove(&order.id);
            }
            if old.oco_group_id != order.oco_group_id
                && let Some(ref old_group_id) = old.oco_group_id
                && let Some(mut orders) = self.oco_groups.get_mut(old_group_id)
            {
                orders.remove(&order.id);
                if orders.is_empty() {
                    drop(orders);
                    self.oco_groups.remove(old_group_id);
                }
            }
        }

        // Update new indexes
        if let Some(ref group_id) = order.oco_group_id {
            self.oco_groups
                .entry(group_id.clone())
                .or_default()
                .insert(order.id.clone());
        }

        self.orders_by_symbol
            .entry(order.symbol.clone())
            .or_default()
            .insert(order.id.clone());

        if let Some(ref pos_id) = order.position_id {
            self.orders_by_position
                .entry(pos_id.clone())
                .or_default()
                .insert(order.id.clone());
        }
    }

    /// Update order on fill event.
    ///
    /// Note: Does not validate that `filled_qty` <= quantity.
    /// Provider is source of truth for all quantities.
    #[must_use]
    pub fn update_order_fill(
        &self,
        order_id: &str,
        filled_qty: Decimal,
        status: OrderStatus,
    ) -> bool {
        if let Some(mut entry) = self.orders.get_mut(order_id) {
            entry.filled_quantity = filled_qty;
            entry.status = status;
            true
        } else {
            false
        }
    }

    /// Remove order (cancelled, filled, expired).
    #[must_use]
    pub fn remove_order(&self, order_id: &str) -> bool {
        if let Some((_, order)) = self.orders.remove(order_id) {
            // Remove from symbol index
            if let Some(mut orders) = self.orders_by_symbol.get_mut(&order.symbol) {
                orders.remove(order_id);
            }

            // Remove from position index
            if let Some(ref pos_id) = order.position_id
                && let Some(mut orders) = self.orders_by_position.get_mut(pos_id)
            {
                orders.remove(order_id);
            }

            // Remove from OCO group index
            if let Some(ref group_id) = order.oco_group_id
                && let Some(mut orders) = self.oco_groups.get_mut(group_id)
            {
                orders.remove(order_id);
                // Clean up empty groups
                if orders.is_empty() {
                    drop(orders); // Release lock before removal
                    self.oco_groups.remove(group_id);
                }
            }
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn get_order(&self, order_id: &str) -> Option<CachedOrder> {
        self.orders.get(order_id).map(|r| r.value().clone())
    }

    #[must_use]
    pub fn get_orders_for_position(&self, position_id: &str) -> Vec<CachedOrder> {
        let order_ids = self
            .orders_by_position
            .get(position_id)
            .map(|r| r.value().clone())
            .unwrap_or_default();

        order_ids
            .into_iter()
            .filter_map(|id| self.get_order(&id))
            .collect()
    }

    #[must_use]
    pub fn get_orders_for_symbol(&self, symbol: &str) -> Vec<CachedOrder> {
        let order_ids = self
            .orders_by_symbol
            .get(symbol)
            .map(|r| r.value().clone())
            .unwrap_or_default();

        order_ids
            .into_iter()
            .filter_map(|id| self.get_order(&id))
            .collect()
    }

    #[must_use]
    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    /// Get all tracked order IDs.
    ///
    /// Used by reconnect reconciliation to check each order against the provider.
    #[must_use]
    pub fn get_all_order_ids(&self) -> Vec<String> {
        self.orders
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    // =========================================================================
    // Position Operations
    // =========================================================================

    /// Insert or update a position.
    ///
    /// Uses atomic insert to avoid TOCTOU race conditions.
    pub fn upsert_position(&self, position: &Position) {
        let cached = CachedPosition {
            position_id: position.id.clone(),
            symbol: position.symbol.clone(),
            side: position.side,
            quantity: position.quantity,
        };

        // Atomically insert and get old value
        let old_cached = self.positions.insert(position.id.clone(), cached);

        // Clean up old symbol index if symbol changed (rare but possible)
        if let Some(ref old) = old_cached
            && old.symbol != position.symbol
            && let Some(entry) = self.position_by_symbol.get(&old.symbol)
            && entry.value() == &position.id
        {
            drop(entry);
            self.position_by_symbol.remove(&old.symbol);
        }

        // Update symbol index (for netting mode lookups)
        self.position_by_symbol
            .insert(position.symbol.clone(), position.id.clone());
    }

    /// Remove position (closed).
    #[must_use]
    pub fn remove_position(&self, position_id: &str) -> bool {
        if let Some((_, position)) = self.positions.remove(position_id) {
            // Remove from symbol index only if this position owns it
            // (protects against race conditions)
            if let Some(entry) = self.position_by_symbol.get(&position.symbol)
                && entry.value() == position_id
            {
                drop(entry); // Release read lock before removal
                self.position_by_symbol.remove(&position.symbol);
            }

            // Clean up order index for this position
            self.orders_by_position.remove(position_id);
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn get_position(&self, position_id: &str) -> Option<CachedPosition> {
        self.positions.get(position_id).map(|r| r.value().clone())
    }

    /// For netting mode - one position per symbol
    #[must_use]
    pub fn get_position_by_symbol(&self, symbol: &str) -> Option<CachedPosition> {
        self.position_by_symbol
            .get(symbol)
            .and_then(|pos_id| self.get_position(&pos_id))
    }

    #[must_use]
    pub fn has_position_by_symbol(&self, symbol: &str) -> bool {
        self.position_by_symbol.contains_key(symbol)
    }

    #[must_use]
    pub fn position_count(&self) -> usize {
        self.positions.len()
    }

    // =========================================================================
    // Exit Order Tracking
    // =========================================================================

    /// Mark orders as exit orders with sibling link.
    ///
    /// Called when SL/TP orders are placed for an entry order.
    /// Returns false if either order is not found in the cache.
    #[must_use]
    pub fn link_exit_orders(
        &self,
        sl_order_id: &str,
        tp_order_id: &str,
        parent_order_id: &str,
    ) -> bool {
        let sl_updated = self.mark_as_exit_order(sl_order_id, parent_order_id, Some(tp_order_id));
        let tp_updated = self.mark_as_exit_order(tp_order_id, parent_order_id, Some(sl_order_id));

        sl_updated && tp_updated
    }

    /// Mark a single order as an exit order.
    ///
    /// Links the order to its parent and optionally to a sibling order
    /// (for OCO pairs where SL and TP cancel each other).
    fn mark_as_exit_order(
        &self,
        order_id: &str,
        parent_order_id: &str,
        sibling_order_id: Option<&str>,
    ) -> bool {
        if let Some(mut entry) = self.orders.get_mut(order_id) {
            entry.is_exit_order = true;
            entry.parent_order_id = Some(parent_order_id.to_string());
            entry.sibling_order_id = sibling_order_id.map(String::from);
            true
        } else {
            false
        }
    }

    /// For OCO cancellation
    #[must_use]
    pub fn get_sibling_order(&self, order_id: &str) -> Option<String> {
        self.orders
            .get(order_id)
            .and_then(|r| r.sibling_order_id.clone())
    }

    #[must_use]
    pub fn is_exit_order(&self, order_id: &str) -> bool {
        self.orders.get(order_id).is_some_and(|r| r.is_exit_order)
    }

    #[must_use]
    pub fn get_parent_order(&self, order_id: &str) -> Option<String> {
        self.orders
            .get(order_id)
            .and_then(|r| r.parent_order_id.clone())
    }

    // =========================================================================
    // OCO Group Tracking
    // =========================================================================

    /// Add an order to an OCO group.
    ///
    /// Called when creating orders with `oco_group_id` to register them
    /// in the group index. Orders in the same group will be cancelled
    /// when one fills completely.
    pub fn add_to_oco_group(&self, order_id: &str, group_id: &str) {
        // Add to group index
        self.oco_groups
            .entry(group_id.to_string())
            .or_default()
            .insert(order_id.to_string());

        // Update the cached order's oco_group_id
        if let Some(mut entry) = self.orders.get_mut(order_id) {
            entry.oco_group_id = Some(group_id.to_string());
        }
    }

    /// Get all sibling order IDs in an OCO group, excluding a specific order.
    ///
    /// Returns the IDs of all other orders in the same OCO group.
    /// Used when an order fills to find siblings that need cancellation.
    #[must_use]
    pub fn get_oco_siblings(&self, group_id: &str, exclude_order_id: &str) -> Vec<String> {
        self.oco_groups
            .get(group_id)
            .map(|orders| {
                orders
                    .iter()
                    .filter(|id| *id != exclude_order_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn get_oco_group_id(&self, order_id: &str) -> Option<String> {
        // First check if order is in orders map with oco_group_id
        if let Some(entry) = self.orders.get(order_id)
            && entry.oco_group_id.is_some()
        {
            return entry.oco_group_id.clone();
        }
        // Otherwise search oco_groups index (order may not be in orders map yet)
        for entry in &self.oco_groups {
            if entry.value().contains(order_id) {
                return Some(entry.key().clone());
            }
        }
        None
    }

    #[must_use]
    pub fn is_in_oco_group(&self, order_id: &str) -> bool {
        self.orders
            .get(order_id)
            .is_some_and(|r| r.oco_group_id.is_some())
    }

    // =========================================================================
    // OCO Double-Exit Detection
    // =========================================================================

    /// Record an OCO fill for double-exit detection.
    ///
    /// Returns `None` if this is the first fill for the group (normal case).
    /// Returns `Some(previous_record)` if a fill was already recorded,
    /// indicating a double-exit has occurred.
    pub fn record_oco_fill(
        &self,
        group_id: &str,
        order_id: &str,
        filled_qty: Decimal,
        symbol: &str,
        side: Side,
    ) -> Option<OcoFillRecord> {
        let new_record = OcoFillRecord {
            first_filled_order_id: order_id.to_string(),
            first_filled_qty: filled_qty,
            symbol: symbol.to_string(),
            side,
        };

        // Try to insert; if entry already exists, return the previous record
        match self.oco_fills.entry(group_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(existing) => Some(existing.get().clone()),
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(new_record);
                None
            }
        }
    }

    /// Clear OCO fill tracking for a group (cleanup after resolution).
    pub fn clear_oco_fill(&self, group_id: &str) {
        self.oco_fills.remove(group_id);
    }
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OrderType, TimeInForce};
    use chrono::Utc;
    use rust_decimal_macros::dec;

    // =========================================================================
    // Test Helpers
    // =========================================================================

    fn create_test_order(id: &str, symbol: &str, side: Side, quantity: Decimal) -> Order {
        Order {
            id: id.to_string(),
            client_order_id: None,
            symbol: symbol.to_string(),
            side,
            order_type: OrderType::Market,
            quantity,
            filled_quantity: Decimal::ZERO,
            remaining_quantity: quantity,
            limit_price: None,
            stop_price: None,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            average_fill_price: None,
            status: OrderStatus::Open,
            reject_reason: None,
            position_id: None,
            reduce_only: None,
            post_only: None,
            hidden: None,
            display_quantity: None,
            oco_group_id: None,
            correlation_id: None,
            time_in_force: TimeInForce::Gtc,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn create_test_position(
        id: &str,
        symbol: &str,
        side: PositionSide,
        quantity: Decimal,
    ) -> Position {
        Position {
            id: id.to_string(),
            symbol: symbol.to_string(),
            side,
            quantity,
            average_entry_price: dec!(100),
            current_price: dec!(105),
            unrealized_pnl: dec!(500),
            realized_pnl: Decimal::ZERO,
            margin_mode: None,
            leverage: None,
            liquidation_price: None,
            opened_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    // =========================================================================
    // Index Invariant Tests
    // =========================================================================

    #[test]
    fn test_upsert_order_cleans_old_position_index() {
        let state = StateManager::new();

        // Insert order with position_id = "pos-1"
        let mut order = create_test_order("order-1", "AAPL", Side::Buy, dec!(100));
        order.position_id = Some("pos-1".to_string());
        state.upsert_order(&order);

        // Verify it's in pos-1 index
        let orders = state.get_orders_for_position("pos-1");
        assert_eq!(orders.len(), 1);

        // Update order with position_id = "pos-2"
        order.position_id = Some("pos-2".to_string());
        state.upsert_order(&order);

        // Verify it's removed from pos-1 index
        let old_orders = state.get_orders_for_position("pos-1");
        assert!(old_orders.is_empty());

        // Verify it's in pos-2 index
        let new_orders = state.get_orders_for_position("pos-2");
        assert_eq!(new_orders.len(), 1);
    }

    #[test]
    fn test_remove_order_cleans_up_indexes() {
        let state = StateManager::new();
        let mut order = create_test_order("order-1", "AAPL", Side::Buy, dec!(100));
        order.position_id = Some("pos-1".to_string());
        state.upsert_order(&order);

        // Verify indexes were created
        assert!(!state.get_orders_for_symbol("AAPL").is_empty());
        assert!(!state.get_orders_for_position("pos-1").is_empty());

        let _ = state.remove_order("order-1");

        // Verify indexes were cleaned
        assert!(state.get_orders_for_symbol("AAPL").is_empty());
        assert!(state.get_orders_for_position("pos-1").is_empty());
    }

    #[test]
    fn test_remove_position_only_removes_own_symbol_index() {
        let state = StateManager::new();

        // Add position pos-1 for AAPL
        let position1 = create_test_position("pos-1", "AAPL", PositionSide::Long, dec!(100));
        state.upsert_position(&position1);

        // Replace with position pos-2 for AAPL (simulating position update)
        let position2 = create_test_position("pos-2", "AAPL", PositionSide::Long, dec!(50));
        state.upsert_position(&position2);

        // Now remove pos-1 - should NOT remove AAPL from symbol index
        let removed = state.remove_position("pos-1");
        assert!(removed);

        // AAPL should still resolve to pos-2
        let cached = state
            .get_position_by_symbol("AAPL")
            .expect("should still exist");
        assert_eq!(cached.position_id, "pos-2");
    }

    // =========================================================================
    // Exit Order Tracking Tests
    // =========================================================================

    #[test]
    fn test_link_exit_orders() {
        let state = StateManager::new();

        // Create entry order and exit orders
        state.upsert_order(&create_test_order("entry-1", "AAPL", Side::Buy, dec!(100)));
        state.upsert_order(&create_test_order("sl-1", "AAPL", Side::Sell, dec!(100)));
        state.upsert_order(&create_test_order("tp-1", "AAPL", Side::Sell, dec!(100)));

        // Link SL and TP as exit orders
        let linked = state.link_exit_orders("sl-1", "tp-1", "entry-1");
        assert!(linked);

        // Verify SL
        let sl = state.get_order("sl-1").unwrap();
        assert!(sl.is_exit_order);
        assert_eq!(sl.parent_order_id, Some("entry-1".to_string()));
        assert_eq!(sl.sibling_order_id, Some("tp-1".to_string()));

        // Verify TP
        let tp = state.get_order("tp-1").unwrap();
        assert!(tp.is_exit_order);
        assert_eq!(tp.parent_order_id, Some("entry-1".to_string()));
        assert_eq!(tp.sibling_order_id, Some("sl-1".to_string()));
    }

    #[test]
    fn test_link_exit_orders_returns_false_if_order_missing() {
        let state = StateManager::new();
        state.upsert_order(&create_test_order("sl-1", "AAPL", Side::Sell, dec!(100)));
        // tp-1 does not exist

        let linked = state.link_exit_orders("sl-1", "tp-1", "entry-1");
        assert!(!linked);
    }

    #[test]
    fn test_is_exit_order() {
        let state = StateManager::new();
        state.upsert_order(&create_test_order("entry-1", "AAPL", Side::Buy, dec!(100)));
        state.upsert_order(&create_test_order("sl-1", "AAPL", Side::Sell, dec!(100)));
        state.upsert_order(&create_test_order("tp-1", "AAPL", Side::Sell, dec!(100)));

        // Before linking
        assert!(!state.is_exit_order("entry-1"));
        assert!(!state.is_exit_order("sl-1"));
        assert!(!state.is_exit_order("tp-1"));

        let _ = state.link_exit_orders("sl-1", "tp-1", "entry-1");

        // After linking
        assert!(!state.is_exit_order("entry-1")); // Entry is not exit
        assert!(state.is_exit_order("sl-1"));
        assert!(state.is_exit_order("tp-1"));

        // Unknown order
        assert!(!state.is_exit_order("unknown"));
    }

    #[test]
    fn test_get_sibling_order() {
        let state = StateManager::new();
        state.upsert_order(&create_test_order("sl-1", "AAPL", Side::Sell, dec!(100)));
        state.upsert_order(&create_test_order("tp-1", "AAPL", Side::Sell, dec!(100)));
        let _ = state.link_exit_orders("sl-1", "tp-1", "entry-1");

        assert_eq!(state.get_sibling_order("sl-1"), Some("tp-1".to_string()));
        assert_eq!(state.get_sibling_order("tp-1"), Some("sl-1".to_string()));
        assert!(state.get_sibling_order("unknown").is_none());
    }

    #[test]
    fn test_get_parent_order() {
        let state = StateManager::new();
        state.upsert_order(&create_test_order("entry-1", "AAPL", Side::Buy, dec!(100)));
        state.upsert_order(&create_test_order("sl-1", "AAPL", Side::Sell, dec!(100)));
        let _ = state.link_exit_orders("sl-1", "tp-nonexistent", "entry-1");

        assert_eq!(state.get_parent_order("sl-1"), Some("entry-1".to_string()));
        assert!(state.get_parent_order("entry-1").is_none()); // Entry has no parent
        assert!(state.get_parent_order("unknown").is_none());
    }

    // =========================================================================
    // Concurrent Access Tests
    // =========================================================================

    #[test]
    fn test_concurrent_order_operations() {
        use std::sync::Arc;
        use std::thread;

        let state = Arc::new(StateManager::new());
        let mut handles = vec![];

        // Spawn multiple threads adding orders
        for i in 0..10 {
            let state_clone = Arc::clone(&state);
            let handle = thread::spawn(move || {
                let order = create_test_order(
                    &format!("order-{i}"),
                    "AAPL",
                    Side::Buy,
                    Decimal::from(i + 1),
                );
                state_clone.upsert_order(&order);
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        // Verify all orders were added
        assert_eq!(state.order_count(), 10);
    }

    #[test]
    fn test_concurrent_read_write() {
        use std::sync::Arc;
        use std::thread;

        let state = Arc::new(StateManager::new());

        // Pre-populate
        for i in 0..5 {
            let order = create_test_order(&format!("order-{i}"), "AAPL", Side::Buy, dec!(100));
            state.upsert_order(&order);
        }

        let mut handles = vec![];

        // Readers
        for _ in 0..5 {
            let state_clone = Arc::clone(&state);
            handles.push(thread::spawn(move || {
                for i in 0..5 {
                    let _ = state_clone.get_order(&format!("order-{i}"));
                }
            }));
        }

        // Writers
        for i in 5..10 {
            let state_clone = Arc::clone(&state);
            handles.push(thread::spawn(move || {
                let order = create_test_order(&format!("order-{i}"), "AAPL", Side::Buy, dec!(100));
                state_clone.upsert_order(&order);
            }));
        }

        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        assert_eq!(state.order_count(), 10);
    }

    #[test]
    fn test_concurrent_upsert_remove_same_order() {
        use std::sync::Arc;
        use std::thread;

        let state = Arc::new(StateManager::new());

        // Run multiple iterations to increase chance of hitting race conditions
        for iteration in 0..50 {
            let order_id = format!("order-{iteration}");
            let mut order = create_test_order(&order_id, "AAPL", Side::Buy, dec!(100));
            order.position_id = Some("pos-1".to_string());
            state.upsert_order(&order);

            let state_clone1 = Arc::clone(&state);
            let state_clone2 = Arc::clone(&state);
            let order_id_for_upsert = order_id.clone();
            let order_id_for_remove = order_id.clone();

            // Spawn concurrent upsert (position change) and remove
            let upsert_handle = thread::spawn(move || {
                let mut updated =
                    create_test_order(&order_id_for_upsert, "AAPL", Side::Buy, dec!(100));
                updated.position_id = Some("pos-2".to_string());
                state_clone1.upsert_order(&updated);
            });

            let remove_handle = thread::spawn(move || {
                let _ = state_clone2.remove_order(&order_id_for_remove);
            });

            upsert_handle
                .join()
                .expect("upsert thread should not panic");
            remove_handle
                .join()
                .expect("remove thread should not panic");

            // Verify index consistency: pos-1 index should not contain the order
            let pos1_orders = state.get_orders_for_position("pos-1");
            for o in &pos1_orders {
                assert_ne!(
                    o.order_id, order_id,
                    "Order should not be in pos-1 index after position change"
                );
            }
        }
    }

    // =========================================================================
    // OCO Double-Exit Detection Tests
    // =========================================================================

    #[test]
    fn test_record_oco_fill_first_fill_returns_none() {
        let state = StateManager::new();
        let result =
            state.record_oco_fill("group-1", "sl-order-1", dec!(100), "BTCUSD", Side::Sell);
        assert!(result.is_none(), "First fill should return None");
    }

    #[test]
    fn test_record_oco_fill_second_fill_returns_previous() {
        let state = StateManager::new();
        state.record_oco_fill("group-1", "sl-order-1", dec!(100), "BTCUSD", Side::Sell);

        let result =
            state.record_oco_fill("group-1", "tp-order-1", dec!(100), "BTCUSD", Side::Sell);
        let previous = result.expect("Second fill should return Some");
        assert_eq!(previous.first_filled_order_id, "sl-order-1");
        assert_eq!(previous.first_filled_qty, dec!(100));
        assert_eq!(previous.symbol, "BTCUSD");
    }

    #[test]
    fn test_clear_oco_fill_allows_reuse() {
        let state = StateManager::new();
        state.record_oco_fill("group-1", "sl-order-1", dec!(100), "BTCUSD", Side::Sell);

        state.clear_oco_fill("group-1");

        let result = state.record_oco_fill("group-1", "new-order", dec!(50), "BTCUSD", Side::Sell);
        assert!(
            result.is_none(),
            "After clear, first fill should return None"
        );
    }

    #[test]
    fn test_record_oco_fill_different_groups_independent() {
        let state = StateManager::new();
        let r1 = state.record_oco_fill("group-1", "order-a", dec!(100), "BTCUSD", Side::Sell);
        let r2 = state.record_oco_fill("group-2", "order-b", dec!(50), "ETHUSD", Side::Sell);

        assert!(r1.is_none(), "First fill in group-1 should return None");
        assert!(r2.is_none(), "First fill in group-2 should return None");
    }

    #[test]
    fn test_sync_from_provider_clears_oco_fills() {
        let state = StateManager::new();
        state.record_oco_fill("group-1", "order-1", dec!(100), "BTCUSD", Side::Sell);

        state.sync_from_provider(vec![], vec![]);

        let result = state.record_oco_fill("group-1", "order-2", dec!(100), "BTCUSD", Side::Sell);
        assert!(result.is_none(), "After sync, oco_fills should be cleared");
    }

    // =========================================================================
    // Symbol Index Tests
    // =========================================================================

    #[test]
    fn test_upsert_order_cleans_old_symbol_index() {
        let state = StateManager::new();

        // Insert order with symbol = "AAPL"
        let order = create_test_order("order-1", "AAPL", Side::Buy, dec!(100));
        state.upsert_order(&order);

        // Verify it's in AAPL index
        assert_eq!(state.get_orders_for_symbol("AAPL").len(), 1);

        // Update order with symbol = "MSFT" (unusual but possible)
        let mut updated = create_test_order("order-1", "MSFT", Side::Buy, dec!(100));
        updated.id = "order-1".to_string();
        state.upsert_order(&updated);

        // Verify it's removed from AAPL index
        assert!(state.get_orders_for_symbol("AAPL").is_empty());
        // Verify it's in MSFT index
        assert_eq!(state.get_orders_for_symbol("MSFT").len(), 1);
    }
}
