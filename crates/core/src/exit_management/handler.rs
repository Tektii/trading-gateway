//! Per-adapter Exit Handler for stop-loss and take-profit orders.
//!
//! The Exit Handler manages pending exit orders (SL/TP) that are placed progressively
//! as the primary order fills. This component is instantiated per-adapter, allowing
//! each provider to have its own exit order state.
//!
//! Contains both the `ExitHandling` trait (for `EventRouter` to depend on) and the
//! concrete `ExitHandler` struct that implements it.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::RwLock;

use chrono::Utc;
use rust_decimal::Decimal;
use tracing::{debug, info};

use crate::adapter::TradingAdapter;
use crate::error::{GatewayError, GatewayResult};
use crate::models::{OrderRequest, TradingPlatform};
use crate::state::StateManager;

use super::{
    ActualOrder, CancelExitResult, CancellationReason, CircuitState, ExitBackupRegistration,
    ExitEntry, ExitEntryInfo, ExitEntryParams, ExitEntryStatus, ExitEntryStatusInfo,
    ExitHandlerConfig, ExitLegType, ExitOrderCircuitBreaker, FailedExitInfo, PlacementResult,
    RegisteredExitLegs, opposite_side,
};

// =============================================================================
// ExitHandling Trait
// =============================================================================

/// Trait for exit order (SL/TP) management.
///
/// Implementations handle the lifecycle of exit orders:
/// - Placing SL/TP on fills
/// - Cancelling exits when primary orders are cancelled
/// - Cancelling exits when positions close
/// - Circuit breaker for repeated failures
#[async_trait]
pub trait ExitHandling: Send + Sync {
    /// Handle a fill event by placing exit orders (SL/TP).
    ///
    /// Returns placement results for each exit order attempted.
    async fn handle_fill(
        &self,
        primary_order_id: &str,
        filled_qty: Decimal,
        total_qty: Decimal,
        position_qty: Option<Decimal>,
        adapter: &dyn TradingAdapter,
    ) -> Vec<PlacementResult>;

    /// Handle cancellation of a primary order's exit orders.
    ///
    /// Returns a simplified `CancelExitResult` with lists of cancelled and failed IDs.
    async fn handle_cancellation(&self, primary_order_id: &str) -> GatewayResult<CancelExitResult>;

    /// Check if there are pending exit orders for a primary order.
    fn has_pending_for_primary(&self, primary_order_id: &str) -> bool;

    /// Cancel exit orders when a position closes.
    ///
    /// Returns placeholder IDs that were cancelled.
    async fn cancel_for_position_close(
        &self,
        symbol: &str,
        adapter: &dyn TradingAdapter,
    ) -> Vec<String>;

    /// Check if the circuit breaker is currently open.
    async fn is_circuit_breaker_open(&self) -> bool;

    /// Get failed exit entries for rebroadcasting.
    fn get_failed_entries(&self) -> Vec<FailedExitInfo>;

    /// Clear failed exit entries after rebroadcasting.
    fn clear_failed_entries(&self) -> usize;
}

// =============================================================================
// ExitHandler Struct
// =============================================================================

/// Each adapter has its own `ExitHandler`, isolating exit order state per provider.
pub struct ExitHandler {
    pub(super) pending_by_placeholder: DashMap<String, ExitEntry>,
    pub(super) pending_by_primary: DashMap<String, Vec<String>>,
    #[allow(dead_code)]
    state_manager: Arc<StateManager>,
    pub(super) config: ExitHandlerConfig,
    pub(super) circuit_breaker: RwLock<ExitOrderCircuitBreaker>,
    platform: TradingPlatform,
}

impl std::fmt::Debug for ExitHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExitHandler")
            .field("pending_count", &self.pending_by_placeholder.len())
            .field("primary_orders_tracked", &self.pending_by_primary.len())
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}

// =============================================================================
// ExitHandler Core Implementation
// =============================================================================

impl ExitHandler {
    #[must_use]
    pub fn new(
        state_manager: Arc<StateManager>,
        platform: TradingPlatform,
        config: ExitHandlerConfig,
    ) -> Self {
        let circuit_breaker = ExitOrderCircuitBreaker::new(
            config.circuit_breaker_threshold,
            config.circuit_breaker_window,
        );

        Self {
            pending_by_placeholder: DashMap::new(),
            pending_by_primary: DashMap::new(),
            state_manager,
            config,
            circuit_breaker: RwLock::new(circuit_breaker),
            platform,
        }
    }

    #[must_use]
    pub fn with_defaults(state_manager: Arc<StateManager>, platform: TradingPlatform) -> Self {
        Self::new(state_manager, platform, ExitHandlerConfig::default())
    }

    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending_by_placeholder.len()
    }

    #[must_use]
    pub fn primary_orders_count(&self) -> usize {
        self.pending_by_primary.len()
    }

    #[must_use]
    pub fn is_placeholder_id(id: &str) -> bool {
        id.starts_with("exit:sl:") || id.starts_with("exit:tp:")
    }

    #[must_use]
    pub fn has_pending_for_primary(&self, primary_order_id: &str) -> bool {
        self.pending_by_primary
            .get(primary_order_id)
            .is_some_and(|placeholder_ids| {
                placeholder_ids
                    .iter()
                    .any(|id| self.pending_by_placeholder.contains_key(id))
            })
    }

    #[must_use]
    pub fn get_placeholders_for_primary(&self, primary_order_id: &str) -> Vec<String> {
        self.pending_by_primary
            .get(primary_order_id)
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn get_entry(&self, placeholder_id: &str) -> Option<ExitEntry> {
        self.pending_by_placeholder
            .get(placeholder_id)
            .map(|entry| entry.value().clone())
    }

    #[must_use]
    pub fn get_status(&self, placeholder_id: &str) -> Option<ExitEntryStatus> {
        self.pending_by_placeholder
            .get(placeholder_id)
            .map(|entry| entry.value().status.clone())
    }

    #[must_use]
    pub fn get_status_info(&self, placeholder_id: &str) -> Option<ExitEntryStatusInfo> {
        self.pending_by_placeholder
            .get(placeholder_id)
            .map(|entry| {
                let e = entry.value();
                ExitEntryStatusInfo {
                    status: e.status.clone(),
                    created_at: e.created_at_utc,
                    updated_at: e.updated_at,
                }
            })
    }

    #[must_use]
    pub const fn platform(&self) -> TradingPlatform {
        self.platform
    }

    #[must_use]
    pub const fn config(&self) -> &ExitHandlerConfig {
        &self.config
    }

    /// Returns clones of all non-terminal exit entries (not Cancelled or Expired).
    #[must_use]
    pub fn get_non_terminal_entries(&self) -> Vec<ExitEntry> {
        self.pending_by_placeholder
            .iter()
            .filter(|entry| {
                !matches!(
                    entry.value().status,
                    ExitEntryStatus::Cancelled { .. } | ExitEntryStatus::Expired { .. }
                )
            })
            .map(|entry| entry.value().clone())
            .collect()
    }

    #[must_use]
    pub fn get_pending_entries(&self) -> Vec<ExitEntryInfo> {
        self.pending_by_placeholder
            .iter()
            .filter_map(|entry| {
                let pending = entry.value();
                if !pending.is_pending() {
                    return None;
                }
                Some(ExitEntryInfo {
                    primary_order_id: pending.primary_order_id.clone(),
                    order_type: pending.order_type,
                    created_at: pending.created_at_utc,
                })
            })
            .collect()
    }

    /// Returns information about all exit entries that failed placement.
    #[must_use]
    pub fn get_failed_entries_internal(&self) -> Vec<FailedExitInfo> {
        self.pending_by_placeholder
            .iter()
            .filter_map(|entry| {
                let e = entry.value();
                if let ExitEntryStatus::Failed {
                    ref error,
                    failed_at,
                    ref actual_orders,
                    ..
                } = e.status
                {
                    Some(FailedExitInfo {
                        primary_order_id: e.primary_order_id.clone(),
                        symbol: e.symbol.clone(),
                        order_type: e.order_type,
                        error: error.clone(),
                        failed_at,
                        actual_order_ids: actual_orders
                            .iter()
                            .map(|o| o.order_id.clone())
                            .collect(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    pub async fn is_circuit_breaker_open_internal(&self) -> bool {
        let breaker = self.circuit_breaker.read().await;
        breaker.is_open()
    }

    pub async fn reset_circuit_breaker(&self) -> Result<(), &'static str> {
        let mut breaker = self.circuit_breaker.write().await;
        breaker.reset()
    }

    pub async fn circuit_breaker_state(&self) -> CircuitState {
        let breaker = self.circuit_breaker.read().await;
        breaker.state()
    }

    pub async fn circuit_breaker_failure_count(&self) -> usize {
        let breaker = self.circuit_breaker.read().await;
        breaker.failure_count()
    }

    /// Insert an exit entry directly.
    ///
    /// In production code, entries are created via `register_exit_orders`.
    /// Made `pub` under `test-utils` for integration test setup.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn insert_entry(&self, entry: ExitEntry) {
        let placeholder_id = entry.placeholder_id.clone();
        let primary_order_id = entry.primary_order_id.clone();

        self.pending_by_placeholder
            .insert(placeholder_id.clone(), entry);
        self.add_to_secondary_index(&primary_order_id, &placeholder_id);
    }

    #[cfg(not(any(test, feature = "test-utils")))]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
    pub(crate) fn insert_entry(&self, _entry: ExitEntry) {}

    #[cfg(any(test, feature = "test-utils"))]
    fn add_to_secondary_index(&self, primary_order_id: &str, placeholder_id: &str) {
        self.pending_by_primary
            .entry(primary_order_id.to_string())
            .or_default()
            .push(placeholder_id.to_string());
    }

    pub(super) fn remove_batch_from_secondary_index(
        &self,
        primary_order_id: &str,
        to_remove: &HashSet<String>,
    ) {
        if let Some(mut ids) = self.pending_by_primary.get_mut(primary_order_id) {
            ids.retain(|id| !to_remove.contains(id));
            if ids.is_empty() {
                drop(ids);
                self.pending_by_primary.remove(primary_order_id);
            }
        }
    }

    pub(super) fn extend_secondary_index(
        &self,
        primary_order_id: &str,
        placeholder_ids: &[String],
    ) {
        self.pending_by_primary
            .entry(primary_order_id.to_string())
            .or_default()
            .extend(placeholder_ids.iter().cloned());
    }

    pub(super) fn get_actual_orders_from_entry(entry: &ExitEntry) -> Vec<ActualOrder> {
        match &entry.status {
            ExitEntryStatus::PartiallyTriggered { actual_orders, .. }
            | ExitEntryStatus::Placed { actual_orders }
            | ExitEntryStatus::Failed { actual_orders, .. } => {
                actual_orders.iter().cloned().collect()
            }
            _ => Vec::new(),
        }
    }

    // =========================================================================
    // Registration Methods
    // =========================================================================

    /// Register pending SL/TP orders for a primary order.
    pub fn register(
        &self,
        primary_order_id: &str,
        order: &OrderRequest,
    ) -> GatewayResult<RegisteredExitLegs> {
        if self.pending_by_placeholder.len() >= self.config.max_pending_entries {
            return Err(GatewayError::Internal {
                message: format!(
                    "Resource exhausted: pending_exit_entries (limit: {})",
                    self.config.max_pending_entries
                ),
                source: None,
            });
        }

        let exit_side = opposite_side(order.side);

        if order.stop_loss.is_none() && order.take_profit.is_none() {
            debug!(primary_order_id, "No SL/TP on fill - nothing to register");
            return Ok(RegisteredExitLegs::empty());
        }

        if let Some(sl_price) = order.stop_loss {
            Self::validate_price(sl_price, "stop_loss")?;
        }
        if let Some(tp_price) = order.take_profit {
            Self::validate_price(tp_price, "take_profit")?;
        }

        let mut result = RegisteredExitLegs::empty();

        if let Some(sl_price) = order.stop_loss {
            let entry = ExitEntry::new(ExitEntryParams {
                primary_order_id: primary_order_id.to_string(),
                order_type: ExitLegType::StopLoss,
                symbol: order.symbol.clone(),
                side: exit_side,
                trigger_price: sl_price,
                limit_price: None,
                quantity: order.quantity,
                platform: self.platform(),
            });

            let placeholder_id = entry.placeholder_id.clone();
            self.insert_entry(entry);
            result.stop_loss_id = Some(placeholder_id.clone());
            debug!(
                primary_order_id,
                placeholder_id, sl_price = %sl_price, "Registered pending stop-loss"
            );
        }

        if let Some(tp_price) = order.take_profit {
            let entry = ExitEntry::new(ExitEntryParams {
                primary_order_id: primary_order_id.to_string(),
                order_type: ExitLegType::TakeProfit,
                symbol: order.symbol.clone(),
                side: exit_side,
                trigger_price: tp_price,
                limit_price: None,
                quantity: order.quantity,
                platform: self.platform(),
            });

            let placeholder_id = entry.placeholder_id.clone();
            self.insert_entry(entry);
            result.take_profit_id = Some(placeholder_id.clone());
            debug!(
                primary_order_id,
                placeholder_id, tp_price = %tp_price, "Registered pending take-profit"
            );
        }

        if result.stop_loss_id.is_some() && result.take_profit_id.is_some() {
            let sl_id = result.stop_loss_id.as_ref().unwrap().clone();
            let tp_id = result.take_profit_id.as_ref().unwrap().clone();

            let (first_id, second_id) = if sl_id < tp_id {
                (&sl_id, &tp_id)
            } else {
                (&tp_id, &sl_id)
            };

            if let Some(mut first) = self.pending_by_placeholder.get_mut(first_id) {
                first.sibling_id = Some(second_id.clone());
            }
            if let Some(mut second) = self.pending_by_placeholder.get_mut(second_id) {
                second.sibling_id = Some(first_id.clone());
            }
            debug!(primary_order_id, %sl_id, %tp_id, "Linked SL/TP siblings");
        }

        Ok(result)
    }

    /// Register a pre-emptive backup before attempting native OCO placement.
    pub fn register_preemptive(
        &self,
        order_request: &OrderRequest,
    ) -> GatewayResult<ExitBackupRegistration> {
        let backup_id = format!("backup:{}", uuid::Uuid::new_v4());

        let registered = self.register(&backup_id, order_request)?;

        let mut placeholder_ids = Vec::with_capacity(2);
        if let Some(sl_id) = registered.stop_loss_id {
            placeholder_ids.push(sl_id);
        }
        if let Some(tp_id) = registered.take_profit_id {
            placeholder_ids.push(tp_id);
        }

        debug!(backup_id = %backup_id, placeholder_count = placeholder_ids.len(), "Registered pre-emptive backup");

        Ok(ExitBackupRegistration {
            id: backup_id,
            placeholder_ids,
        })
    }

    /// Activate backup entries by linking them to the actual primary order.
    pub fn activate_backup(&self, backup_id: &str, actual_primary_id: &str) -> GatewayResult<()> {
        let placeholder_ids = self.get_placeholders_for_primary(backup_id);

        if placeholder_ids.is_empty() {
            return Err(GatewayError::OrderNotFound {
                id: backup_id.to_string(),
            });
        }

        for placeholder_id in &placeholder_ids {
            if let Some(mut entry) = self.pending_by_placeholder.get_mut(placeholder_id) {
                entry.primary_order_id = actual_primary_id.to_string();
            }
        }

        self.pending_by_primary.remove(backup_id);
        self.extend_secondary_index(actual_primary_id, &placeholder_ids);

        info!(
            backup_id,
            actual_primary_id,
            entries_activated = placeholder_ids.len(),
            "Backup entries activated"
        );
        Ok(())
    }

    /// Cancel backup entries after successful OCO placement.
    pub fn cancel_backup(&self, backup_id: &str) -> GatewayResult<()> {
        let placeholder_ids = self.get_placeholders_for_primary(backup_id);

        if placeholder_ids.is_empty() {
            return Err(GatewayError::OrderNotFound {
                id: backup_id.to_string(),
            });
        }

        let cancelled_count = placeholder_ids.len();

        for placeholder_id in &placeholder_ids {
            if let Some(mut entry) = self.pending_by_placeholder.get_mut(placeholder_id) {
                entry.set_status(ExitEntryStatus::Cancelled {
                    cancelled_at: Utc::now(),
                    reason: CancellationReason::OcoSucceeded,
                });
            }
            self.pending_by_placeholder.remove(placeholder_id);
        }

        self.pending_by_primary.remove(backup_id);
        info!(
            backup_id,
            cancelled_count, "Backup entries cancelled - native OCO succeeded"
        );
        Ok(())
    }

    pub(super) fn validate_price(price: Decimal, field_name: &str) -> GatewayResult<()> {
        if price <= Decimal::ZERO {
            return Err(GatewayError::InvalidRequest {
                message: format!("{field_name} must be a positive number, got {price}"),
                field: None,
            });
        }
        Ok(())
    }
}

// =============================================================================
// ExitHandling Trait Implementation
// =============================================================================

#[async_trait]
impl ExitHandling for ExitHandler {
    async fn handle_fill(
        &self,
        primary_order_id: &str,
        filled_qty: Decimal,
        total_qty: Decimal,
        position_qty: Option<Decimal>,
        adapter: &dyn TradingAdapter,
    ) -> Vec<PlacementResult> {
        self.handle_fill_internal(
            primary_order_id,
            filled_qty,
            total_qty,
            position_qty,
            adapter,
        )
        .await
    }

    async fn handle_cancellation(&self, primary_order_id: &str) -> GatewayResult<CancelExitResult> {
        let cancelled_entries = self.handle_cancellation_internal(primary_order_id)?;

        let cancelled: Vec<String> = cancelled_entries
            .iter()
            .map(|e| e.placeholder_id.clone())
            .collect();

        Ok(CancelExitResult {
            cancelled,
            failed: Vec::new(),
        })
    }

    fn has_pending_for_primary(&self, primary_order_id: &str) -> bool {
        Self::has_pending_for_primary(self, primary_order_id)
    }

    async fn cancel_for_position_close(
        &self,
        symbol: &str,
        adapter: &dyn TradingAdapter,
    ) -> Vec<String> {
        let cancelled_entries = self
            .cancel_for_position_close_internal(symbol, adapter)
            .await;
        cancelled_entries
            .iter()
            .map(|e| e.placeholder_id.clone())
            .collect()
    }

    async fn is_circuit_breaker_open(&self) -> bool {
        self.is_circuit_breaker_open_internal().await
    }

    fn get_failed_entries(&self) -> Vec<FailedExitInfo> {
        self.get_failed_entries_internal()
    }

    fn clear_failed_entries(&self) -> usize {
        self.clear_failed_entries_internal()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::capabilities::BracketStrategy;
    use crate::exit_management::{ActualOrder, ActualOrderStatus, CancelExitResultInternal};
    use crate::models::{
        CancelOrderResult, Capabilities, ConnectionStatus, Order, OrderHandle, OrderQueryParams,
        OrderStatus, OrderType, Position, PositionMode, Side, TimeInForce,
    };
    use crate::state::StateManager;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn create_test_handler() -> ExitHandler {
        let state_manager = Arc::new(StateManager::new());
        ExitHandler::with_defaults(state_manager, TradingPlatform::AlpacaLive)
    }

    fn create_test_order_request(
        symbol: &str,
        quantity: Decimal,
        side: Side,
        sl: Option<Decimal>,
        tp: Option<Decimal>,
    ) -> OrderRequest {
        OrderRequest {
            symbol: symbol.to_string(),
            quantity,
            side,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Gtc,
            limit_price: None,
            stop_price: None,
            stop_loss: sl,
            take_profit: tp,
            trailing_distance: None,
            trailing_type: None,
            client_order_id: None,
            position_id: None,
            display_quantity: None,
            margin_mode: None,
            leverage: None,
            oco_group_id: None,
            hidden: false,
            post_only: false,
            reduce_only: false,
        }
    }

    #[tokio::test]
    async fn test_register_stop_loss_only() {
        let handler = create_test_handler();
        let order = create_test_order_request("BTCUSD", dec!(1), Side::Buy, Some(dec!(95)), None);

        let result = handler.register("order-1", &order).unwrap();

        assert!(result.has_any());
        assert!(result.stop_loss_id.is_some());
        assert!(result.take_profit_id.is_none());
        assert_eq!(handler.pending_count(), 1);

        let sl_id = result.stop_loss_id.unwrap();
        assert!(sl_id.starts_with("exit:sl:"));

        let entry = handler.get_entry(&sl_id).unwrap();
        assert_eq!(entry.primary_order_id, "order-1");
        assert_eq!(entry.trigger_price, dec!(95));
        assert_eq!(entry.side, Side::Sell);
    }

    #[tokio::test]
    async fn test_register_both_sl_and_tp_links_siblings() {
        let handler = create_test_handler();
        let order = create_test_order_request(
            "BTCUSD",
            dec!(1),
            Side::Buy,
            Some(dec!(95)),
            Some(dec!(110)),
        );

        let result = handler.register("order-1", &order).unwrap();

        assert!(result.has_any());
        assert_eq!(handler.pending_count(), 2);

        let sl_id = result.stop_loss_id.unwrap();
        let tp_id = result.take_profit_id.unwrap();

        let sl_entry = handler.get_entry(&sl_id).unwrap();
        let tp_entry = handler.get_entry(&tp_id).unwrap();

        assert_eq!(sl_entry.sibling_id, Some(tp_id));
        assert_eq!(tp_entry.sibling_id, Some(sl_id));
    }

    #[tokio::test]
    async fn test_cancel_pending_entry() {
        let handler = create_test_handler();
        let order = create_test_order_request("BTCUSD", dec!(1), Side::Buy, Some(dec!(95)), None);

        let result = handler.register("order-1", &order).unwrap();
        let sl_id = result.stop_loss_id.unwrap();

        let cancel_result = handler.cancel_entry(&sl_id).unwrap();
        assert!(matches!(
            cancel_result,
            CancelExitResultInternal::CancelledPending(_)
        ));

        let status = handler.get_status(&sl_id).unwrap();
        assert!(matches!(status, ExitEntryStatus::Cancelled { .. }));
    }

    #[tokio::test]
    async fn test_circuit_breaker_reset() {
        let handler = create_test_handler();

        {
            let mut breaker = handler.circuit_breaker.write().await;
            breaker.record_failure();
            breaker.record_failure();
            breaker.record_failure();
        }

        assert!(handler.is_circuit_breaker_open_internal().await);
        handler
            .reset_circuit_breaker()
            .await
            .expect("reset should succeed");
        assert!(!handler.is_circuit_breaker_open_internal().await);
    }

    fn create_test_entry(
        primary_order_id: &str,
        order_type: ExitLegType,
        symbol: &str,
        status: ExitEntryStatus,
    ) -> ExitEntry {
        let mut entry = ExitEntry::new(ExitEntryParams {
            primary_order_id: primary_order_id.to_string(),
            order_type,
            symbol: symbol.to_string(),
            side: Side::Sell,
            trigger_price: dec!(45000),
            limit_price: None,
            quantity: dec!(10),
            platform: TradingPlatform::AlpacaLive,
        });
        entry.set_status(status);
        entry
    }

    #[test]
    fn test_get_failed_entries_empty_when_no_entries() {
        let handler = create_test_handler();
        assert!(handler.get_failed_entries_internal().is_empty());
    }

    #[test]
    fn test_get_failed_entries_returns_only_failed_entries() {
        let handler = create_test_handler();

        handler.insert_entry(create_test_entry(
            "order-1",
            ExitLegType::StopLoss,
            "BTCUSD",
            ExitEntryStatus::Pending,
        ));
        handler.insert_entry(create_test_entry(
            "order-2",
            ExitLegType::StopLoss,
            "ETHUSD",
            ExitEntryStatus::Failed {
                error: "Connection refused".to_string(),
                retry_count: 3,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![],
            },
        ));
        handler.insert_entry(create_test_entry(
            "order-3",
            ExitLegType::TakeProfit,
            "SOLUSD",
            ExitEntryStatus::Placed {
                actual_orders: smallvec::smallvec![],
            },
        ));

        let failed_entries = handler.get_failed_entries_internal();
        assert_eq!(failed_entries.len(), 1);
        assert_eq!(failed_entries[0].primary_order_id, "order-2");
    }

    #[test]
    fn test_get_failed_entries_includes_actual_order_ids() {
        let handler = create_test_handler();

        handler.insert_entry(create_test_entry(
            "order-99",
            ExitLegType::StopLoss,
            "BTCUSD",
            ExitEntryStatus::Failed {
                error: "Placement timeout".to_string(),
                retry_count: 1,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![
                    ActualOrder {
                        order_id: "live-order-1".to_string(),
                        quantity: dec!(0.5),
                        placed_at: Utc::now(),
                        status: ActualOrderStatus::Open,
                        leg_type: ExitLegType::StopLoss,
                    },
                    ActualOrder {
                        order_id: "live-order-2".to_string(),
                        quantity: dec!(0.3),
                        placed_at: Utc::now(),
                        status: ActualOrderStatus::Open,
                        leg_type: ExitLegType::StopLoss,
                    },
                ],
            },
        ));

        let entries = handler.get_failed_entries_internal();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].actual_order_ids,
            vec!["live-order-1", "live-order-2"]
        );
    }

    #[test]
    fn test_clear_failed_entries_removes_only_failed() {
        let handler = create_test_handler();

        handler.insert_entry(create_test_entry(
            "order-1",
            ExitLegType::StopLoss,
            "BTCUSD",
            ExitEntryStatus::Pending,
        ));
        handler.insert_entry(create_test_entry(
            "order-2",
            ExitLegType::StopLoss,
            "ETHUSD",
            ExitEntryStatus::Failed {
                error: "Connection refused".to_string(),
                retry_count: 3,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![],
            },
        ));

        let cleared = handler.clear_failed_entries_internal();
        assert_eq!(cleared, 1);
        assert_eq!(handler.pending_count(), 1);
    }

    #[test]
    fn test_clear_failed_entries_cleans_secondary_index() {
        let handler = create_test_handler();

        handler.insert_entry(create_test_entry(
            "order-1",
            ExitLegType::StopLoss,
            "BTCUSD",
            ExitEntryStatus::Failed {
                error: "Timeout".to_string(),
                retry_count: 2,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![],
            },
        ));

        assert!(handler.has_pending_for_primary("order-1"));
        handler.clear_failed_entries_internal();
        assert!(!handler.has_pending_for_primary("order-1"));
        assert_eq!(handler.primary_orders_count(), 0);
    }

    fn create_test_actual_order(order_id: &str, quantity: Decimal) -> ActualOrder {
        ActualOrder {
            order_id: order_id.to_string(),
            quantity,
            placed_at: Utc::now(),
            status: ActualOrderStatus::Open,
            leg_type: ExitLegType::StopLoss,
        }
    }

    #[test]
    fn test_cancel_entry_failed_with_actual_orders_returns_needs_cancel() {
        let handler = create_test_handler();

        let failed = create_test_entry(
            "order-1",
            ExitLegType::StopLoss,
            "BTCUSD",
            ExitEntryStatus::Failed {
                error: "Connection refused".to_string(),
                retry_count: 3,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![
                    create_test_actual_order("actual-1", dec!(0.5)),
                    create_test_actual_order("actual-2", dec!(0.3)),
                ],
            },
        );
        let placeholder_id = failed.placeholder_id.clone();
        handler.insert_entry(failed);

        let result = handler.cancel_entry(&placeholder_id).unwrap();

        match result {
            CancelExitResultInternal::NeedsCancelFailed {
                actual_orders,
                info,
            } => {
                assert_eq!(actual_orders.len(), 2);
                assert_eq!(actual_orders[0].order_id, "actual-1");
                assert_eq!(info.primary_order_id, "order-1");
            }
            other => panic!("Expected NeedsCancelFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_cancel_entry_failed_without_actual_orders_returns_already_terminal() {
        let handler = create_test_handler();

        let failed = create_test_entry(
            "order-2",
            ExitLegType::TakeProfit,
            "ETHUSD",
            ExitEntryStatus::Failed {
                error: "Insufficient margin".to_string(),
                retry_count: 1,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![],
            },
        );
        let placeholder_id = failed.placeholder_id.clone();
        handler.insert_entry(failed);

        let result = handler.cancel_entry(&placeholder_id).unwrap();
        assert!(matches!(
            result,
            CancelExitResultInternal::AlreadyTerminal { .. }
        ));
    }

    // =========================================================================
    // Mock adapter for cancel_for_position_close tests
    // =========================================================================

    struct CancelSuccessMockCapabilities;

    impl crate::adapter::ProviderCapabilities for CancelSuccessMockCapabilities {
        fn supports_bracket_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn supports_oco_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn supports_oto_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn bracket_strategy(&self, _order: &OrderRequest) -> BracketStrategy {
            BracketStrategy::None
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supported_asset_classes: vec![],
                supported_order_types: vec![],
                position_mode: PositionMode::Netting,
                features: vec![],
                max_leverage: None,
                rate_limits: None,
            }
        }
    }

    struct CancelSuccessMockAdapter;

    #[async_trait]
    impl TradingAdapter for CancelSuccessMockAdapter {
        fn capabilities(&self) -> &dyn crate::adapter::ProviderCapabilities {
            &CancelSuccessMockCapabilities
        }
        fn platform(&self) -> TradingPlatform {
            TradingPlatform::AlpacaLive
        }
        fn provider_name(&self) -> &'static str {
            "mock-cancel"
        }
        async fn get_account(&self) -> GatewayResult<crate::models::Account> {
            unimplemented!()
        }
        async fn submit_order(&self, _request: &OrderRequest) -> GatewayResult<OrderHandle> {
            unimplemented!()
        }
        async fn get_order(&self, _order_id: &str) -> GatewayResult<Order> {
            unimplemented!()
        }
        async fn get_orders(&self, _params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
            Ok(vec![])
        }
        async fn get_order_history(&self, _params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
            Ok(vec![])
        }
        async fn modify_order(
            &self,
            _order_id: &str,
            _request: &crate::models::ModifyOrderRequest,
        ) -> GatewayResult<crate::models::ModifyOrderResult> {
            unimplemented!()
        }
        async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
            Ok(CancelOrderResult {
                success: true,
                order: Order {
                    id: order_id.to_string(),
                    client_order_id: None,
                    symbol: "BTCUSD".to_string(),
                    side: Side::Sell,
                    order_type: OrderType::StopLimit,
                    quantity: dec!(1),
                    filled_quantity: dec!(0),
                    remaining_quantity: dec!(1),
                    limit_price: None,
                    stop_price: None,
                    stop_loss: None,
                    take_profit: None,
                    trailing_distance: None,
                    trailing_type: None,
                    average_fill_price: None,
                    status: OrderStatus::Cancelled,
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
                },
            })
        }
        async fn get_trades(
            &self,
            _params: &crate::models::TradeQueryParams,
        ) -> GatewayResult<Vec<crate::models::Trade>> {
            Ok(vec![])
        }
        async fn get_positions(&self, _symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
            Ok(vec![])
        }
        async fn get_position(&self, _position_id: &str) -> GatewayResult<Position> {
            unimplemented!()
        }
        async fn close_position(
            &self,
            _position_id: &str,
            _request: &crate::models::ClosePositionRequest,
        ) -> GatewayResult<OrderHandle> {
            unimplemented!()
        }
        async fn get_quote(&self, _symbol: &str) -> GatewayResult<crate::models::Quote> {
            unimplemented!()
        }
        async fn get_bars(
            &self,
            _symbol: &str,
            _params: &crate::models::BarParams,
        ) -> GatewayResult<Vec<crate::models::Bar>> {
            Ok(vec![])
        }
        async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
            Ok(Capabilities {
                supported_asset_classes: vec![],
                supported_order_types: vec![],
                position_mode: PositionMode::Netting,
                features: vec![],
                max_leverage: None,
                rate_limits: None,
            })
        }
        async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn test_cancel_for_position_close_includes_failed_entries_with_actual_orders() {
        let handler = create_test_handler();
        let adapter = CancelSuccessMockAdapter;

        handler.insert_entry(create_test_entry(
            "order-1",
            ExitLegType::StopLoss,
            "BTCUSD",
            ExitEntryStatus::Failed {
                error: "Connection timeout".to_string(),
                retry_count: 3,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![create_test_actual_order("actual-1", dec!(0.5))],
            },
        ));
        handler.insert_entry(create_test_entry(
            "order-2",
            ExitLegType::TakeProfit,
            "BTCUSD",
            ExitEntryStatus::Pending,
        ));

        let cancelled = handler
            .cancel_for_position_close_internal("BTCUSD", &adapter)
            .await;
        assert_eq!(cancelled.len(), 2);
    }

    #[tokio::test]
    async fn test_cancel_for_position_close_skips_failed_entries_without_actual_orders() {
        let handler = create_test_handler();
        let adapter = CancelSuccessMockAdapter;

        handler.insert_entry(create_test_entry(
            "order-1",
            ExitLegType::StopLoss,
            "BTCUSD",
            ExitEntryStatus::Failed {
                error: "Rejected by exchange".to_string(),
                retry_count: 1,
                failed_at: Utc::now(),
                actual_orders: smallvec::smallvec![],
            },
        ));

        let cancelled = handler
            .cancel_for_position_close_internal("BTCUSD", &adapter)
            .await;
        assert!(cancelled.is_empty());
    }
}
