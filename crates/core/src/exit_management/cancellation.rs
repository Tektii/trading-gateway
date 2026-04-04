//! Cancellation and cleanup of exit orders.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::Utc;
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use crate::adapter::TradingAdapter;
use crate::error::GatewayError;

use super::{
    ActualOrder, CancelExitResultInternal, CancellationReason, CancelledExitInfo, ExitEntryStatus,
    SiblingCancellation,
};

use super::ExitHandler;

impl ExitHandler {
    /// Cancel a pending SL/TP entry by placeholder ID.
    ///
    /// Returns information about what action the caller needs to take:
    /// - `CancelledPending`: Entry was pending, now cancelled (no further action)
    /// - `NeedsCancelAtProvider`: Entry was placed, caller must cancel at exchange
    /// - `NeedsCancelPartiallyTriggered`: Some orders placed, cancel at exchange
    /// - `NeedsCancelFailed`: Failed entry has live orders that need cancellation
    /// - `AlreadyTerminal`: Entry already in terminal state
    pub fn cancel_entry(
        &self,
        placeholder_id: &str,
    ) -> Result<CancelExitResultInternal, GatewayError> {
        // Read entry data and status into locals, releasing the DashMap guard quickly.
        let entry_ref = self
            .pending_by_placeholder
            .get(placeholder_id)
            .ok_or_else(|| GatewayError::OrderNotFound {
                id: placeholder_id.to_string(),
            })?;

        let info = CancelledExitInfo {
            placeholder_id: placeholder_id.to_string(),
            primary_order_id: entry_ref.primary_order_id.clone(),
            leg_type: entry_ref.order_type,
            platform: entry_ref.platform.to_string(),
            symbol: entry_ref.symbol.clone(),
            side: entry_ref.side,
            trigger_price: entry_ref.trigger_price,
            quantity: entry_ref.quantity,
        };
        let status_snapshot = entry_ref.status.clone();
        drop(entry_ref);

        match &status_snapshot {
            ExitEntryStatus::Pending => {
                if let Some(mut entry_ref) = self.pending_by_placeholder.get_mut(placeholder_id) {
                    entry_ref.set_status(ExitEntryStatus::Cancelled {
                        cancelled_at: Utc::now(),
                        reason: CancellationReason::UserRequested,
                    });
                }
                debug!(placeholder_id, "Pending SL/TP entry cancelled");
                Ok(CancelExitResultInternal::CancelledPending(info))
            }

            ExitEntryStatus::PartiallyTriggered { actual_orders, .. } => {
                if actual_orders.is_empty() {
                    if let Some(mut entry_ref) = self.pending_by_placeholder.get_mut(placeholder_id)
                    {
                        entry_ref.set_status(ExitEntryStatus::Cancelled {
                            cancelled_at: Utc::now(),
                            reason: CancellationReason::UserRequested,
                        });
                    }
                    debug!(
                        placeholder_id,
                        "Partially triggered SL/TP cancelled (no orders yet)"
                    );
                    Ok(CancelExitResultInternal::CancelledPending(info))
                } else {
                    let result = CancelExitResultInternal::NeedsCancelPartiallyTriggered {
                        actual_orders: actual_orders.clone(),
                        info,
                    };
                    debug!(
                        placeholder_id,
                        num_orders = actual_orders.len(),
                        "Needs exchange cancellation"
                    );
                    Ok(result)
                }
            }

            ExitEntryStatus::Placed { actual_orders } => {
                let result = CancelExitResultInternal::NeedsCancelAtProvider {
                    actual_orders: actual_orders.clone(),
                    info,
                };
                debug!(
                    placeholder_id,
                    num_orders = actual_orders.len(),
                    "Needs exchange cancellation"
                );
                Ok(result)
            }

            ExitEntryStatus::Failed { actual_orders, .. } if !actual_orders.is_empty() => {
                let result = CancelExitResultInternal::NeedsCancelFailed {
                    actual_orders: actual_orders.clone(),
                    info,
                };
                debug!(
                    placeholder_id,
                    num_orders = actual_orders.len(),
                    "Failed entry has live orders that need exchange cancellation"
                );
                Ok(result)
            }

            status @ (ExitEntryStatus::Failed { .. }
            | ExitEntryStatus::Cancelled { .. }
            | ExitEntryStatus::Expired { .. }) => {
                debug!(placeholder_id, ?status, "Already in terminal state");
                Ok(CancelExitResultInternal::AlreadyTerminal {
                    status: status.clone(),
                })
            }
        }
    }

    /// Handle cancellation of a primary order.
    ///
    /// Cancels all pending SL/TP entries associated with the primary order.
    /// Returns a list of `CancelledExitInfo` for entries that were successfully cancelled.
    pub fn handle_cancellation_internal(
        &self,
        primary_order_id: &str,
    ) -> Result<Vec<CancelledExitInfo>, GatewayError> {
        let placeholder_ids = self
            .pending_by_primary
            .remove(primary_order_id)
            .map(|(_, ids)| ids)
            .unwrap_or_default();

        if placeholder_ids.is_empty() {
            debug!(primary_order_id, "No pending SL/TP entries to cancel");
            return Ok(Vec::new());
        }

        info!(
            primary_order_id,
            entry_count = placeholder_ids.len(),
            "Cancelling pending SL/TP entries"
        );

        let mut cancelled_entries = Vec::new();

        for placeholder_id in placeholder_ids {
            if let Some(mut entry) = self.pending_by_placeholder.get_mut(&placeholder_id)
                && entry.is_pending()
            {
                cancelled_entries.push(CancelledExitInfo {
                    placeholder_id,
                    primary_order_id: entry.primary_order_id.clone(),
                    leg_type: entry.order_type,
                    platform: entry.platform.to_string(),
                    symbol: entry.symbol.clone(),
                    side: entry.side,
                    trigger_price: entry.trigger_price,
                    quantity: entry.quantity,
                });

                entry.set_status(ExitEntryStatus::Cancelled {
                    cancelled_at: Utc::now(),
                    reason: CancellationReason::PrimaryCancelled,
                });
            }
        }

        info!(
            primary_order_id,
            cancelled_count = cancelled_entries.len(),
            "Cancellation complete"
        );
        Ok(cancelled_entries)
    }

    /// Cancel all pending SL/TP orders for a symbol before closing position.
    ///
    /// This prevents "insufficient balance" errors when SL/TP orders reserve balance.
    pub async fn cancel_for_position_close_internal(
        &self,
        symbol: &str,
        adapter: &dyn TradingAdapter,
    ) -> Vec<CancelledExitInfo> {
        // Find all pending entries for this symbol
        let entries_to_cancel: Vec<(String, String, Vec<ActualOrder>)> = self
            .pending_by_placeholder
            .iter()
            .filter_map(|entry| {
                let e = entry.value();
                if e.symbol != symbol {
                    return None;
                }

                match &e.status {
                    ExitEntryStatus::Pending => {
                        Some((entry.key().clone(), e.primary_order_id.clone(), Vec::new()))
                    }
                    ExitEntryStatus::Placed { actual_orders }
                    | ExitEntryStatus::PartiallyTriggered { actual_orders, .. } => Some((
                        entry.key().clone(),
                        e.primary_order_id.clone(),
                        actual_orders.iter().cloned().collect(),
                    )),
                    ExitEntryStatus::Failed { actual_orders, .. } if !actual_orders.is_empty() => {
                        Some((
                            entry.key().clone(),
                            e.primary_order_id.clone(),
                            actual_orders.iter().cloned().collect(),
                        ))
                    }
                    _ => None,
                }
            })
            .collect();

        if entries_to_cancel.is_empty() {
            info!(
                symbol,
                "No pending SL/TP orders to cancel for position close"
            );
            return Vec::new();
        }

        info!(
            symbol,
            count = entries_to_cancel.len(),
            "Cancelling SL/TP orders before position close"
        );

        let mut cancelled_entries = Vec::new();

        for (placeholder_id, _primary_id, actual_orders) in entries_to_cancel {
            // Cancel any placed orders at the exchange
            for actual_order in &actual_orders {
                match adapter.cancel_order(&actual_order.order_id).await {
                    Ok(_result) => {
                        debug!(
                            placeholder_id,
                            order_id = %actual_order.order_id,
                            "Successfully cancelled SL/TP at exchange"
                        );
                    }
                    Err(e) => {
                        warn!(
                            placeholder_id,
                            order_id = %actual_order.order_id,
                            error = %e,
                            "Failed to cancel SL/TP at exchange (may be already filled)"
                        );
                    }
                }
            }

            // Mark entry as cancelled
            if let Some(mut entry) = self.pending_by_placeholder.get_mut(&placeholder_id) {
                cancelled_entries.push(CancelledExitInfo {
                    placeholder_id: placeholder_id.clone(),
                    primary_order_id: entry.primary_order_id.clone(),
                    leg_type: entry.order_type,
                    platform: entry.platform.to_string(),
                    symbol: entry.symbol.clone(),
                    side: entry.side,
                    trigger_price: entry.trigger_price,
                    quantity: entry.quantity,
                });

                entry.set_status(ExitEntryStatus::Cancelled {
                    cancelled_at: Utc::now(),
                    reason: CancellationReason::PositionClosed,
                });
            }
        }

        info!(
            symbol,
            cancelled_count = cancelled_entries.len(),
            "Position close SL/TP cancellation complete"
        );

        cancelled_entries
    }

    /// Remove all entries in `Failed` status from both indexes.
    ///
    /// Called after re-broadcasting `PositionUnprotected` on reconnect so that
    /// stale failed entries do not accumulate indefinitely.
    pub fn clear_failed_entries_internal(&self) -> usize {
        let failed_placeholders: Vec<String> = self
            .pending_by_placeholder
            .iter()
            .filter(|entry| matches!(entry.value().status, ExitEntryStatus::Failed { .. }))
            .map(|entry| entry.key().clone())
            .collect();

        if failed_placeholders.is_empty() {
            return 0;
        }

        let mut primary_order_removals: HashMap<String, Vec<String>> = HashMap::new();

        for placeholder_id in &failed_placeholders {
            if let Some((_, entry)) = self.pending_by_placeholder.remove(placeholder_id) {
                primary_order_removals
                    .entry(entry.primary_order_id.clone())
                    .or_default()
                    .push(placeholder_id.clone());
            }
        }

        for (primary_order_id, removed_ids) in &primary_order_removals {
            let removed_set: HashSet<String> = removed_ids.iter().cloned().collect();
            self.remove_batch_from_secondary_index(primary_order_id, &removed_set);
        }

        let cleared_count = failed_placeholders.len();
        info!(
            cleared_count,
            "Cleared failed exit entries after rebroadcast"
        );
        cleared_count
    }

    /// Clean up entries that have exceeded the TTL.
    pub fn cleanup_expired_entries(&self) -> usize {
        let now = Instant::now();
        let mut expired_placeholders = Vec::new();

        for entry in &self.pending_by_placeholder {
            if now.duration_since(entry.created_at) > self.config.entry_ttl {
                expired_placeholders.push(entry.placeholder_id.clone());
            }
        }

        if expired_placeholders.is_empty() {
            return 0;
        }

        info!(count = expired_placeholders.len(), "Found expired entries");

        let mut primary_order_expirations: HashMap<String, Vec<String>> = HashMap::new();

        for placeholder_id in &expired_placeholders {
            if let Some((_, entry)) = self.pending_by_placeholder.remove(placeholder_id) {
                primary_order_expirations
                    .entry(entry.primary_order_id.clone())
                    .or_default()
                    .push(placeholder_id.clone());
            }
        }

        for (primary_order_id, expired_ids) in &primary_order_expirations {
            let expired_set: HashSet<String> = expired_ids.iter().cloned().collect();
            self.remove_batch_from_secondary_index(primary_order_id, &expired_set);
        }

        let cleaned_count = expired_placeholders.len();
        info!(cleaned_count, "TTL cleanup complete");
        cleaned_count
    }

    /// Cancel sibling order proportionally (OCO behavior).
    ///
    /// When one leg is placed, the sibling should be reduced by the same amount.
    /// Uses optimistic concurrency control via version checking to prevent TOCTOU races.
    pub(super) async fn cancel_sibling_proportionally(
        &self,
        sibling_id: &str,
        qty_to_cancel: Decimal,
        adapter: &dyn TradingAdapter,
    ) -> Option<SiblingCancellation> {
        // Capture entry and its version atomically
        let (sibling_entry, expected_version) = match self.pending_by_placeholder.get(sibling_id) {
            Some(entry) => (entry.value().clone(), entry.version()),
            None => return None,
        };

        // Get sibling's actual orders
        let actual_orders = Self::get_actual_orders_from_entry(&sibling_entry);

        if actual_orders.is_empty() {
            // Sibling not yet placed - just reduce pending quantity
            if let Some(mut entry) = self.pending_by_placeholder.get_mut(sibling_id) {
                // Verify version hasn't changed (optimistic concurrency control)
                if entry.version() != expected_version {
                    tracing::warn!(
                        sibling_id = %sibling_id,
                        expected_version = %expected_version,
                        actual_version = %entry.version(),
                        "Sibling entry was modified concurrently, skipping update"
                    );
                    return None;
                }

                let new_qty = (entry.quantity - qty_to_cancel).max(Decimal::ZERO);
                if new_qty < self.config.min_exit_order_qty {
                    // Cancel entirely if below threshold
                    entry.set_status(ExitEntryStatus::Cancelled {
                        cancelled_at: Utc::now(),
                        reason: CancellationReason::SiblingFilled,
                    });
                } else {
                    entry.quantity = new_qty;
                    entry.increment_version();
                }
            }
            return Some(SiblingCancellation::pending(sibling_id, qty_to_cancel));
        }

        // Sibling has placed orders - cancel them at provider
        let mut cancelled_order_ids = Vec::new();
        let mut errors = Vec::new();

        for actual_order in &actual_orders {
            match adapter.cancel_order(&actual_order.order_id).await {
                Ok(_) => {
                    cancelled_order_ids.push(actual_order.order_id.clone());
                }
                Err(e) => {
                    errors.push(format!("{}: {}", actual_order.order_id, e));
                }
            }
        }

        // Update sibling status
        if let Some(mut entry) = self.pending_by_placeholder.get_mut(sibling_id) {
            entry.set_status(ExitEntryStatus::Cancelled {
                cancelled_at: Utc::now(),
                reason: CancellationReason::SiblingFilled,
            });
        }

        Some(SiblingCancellation::placed_multiple(
            sibling_id,
            cancelled_order_ids,
            qty_to_cancel,
            errors,
        ))
    }
}
