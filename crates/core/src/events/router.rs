//! Event Router for per-adapter event processing.
//!
//! The Event Router receives normalized events from the provider WebSocket,
//! updates the State Manager, routes fills to the Exit Handler, and broadcasts
//! events to connected strategies.
//!
//! # Key Responsibilities
//!
//! 1. **State Manager Updates**: Keep order/position cache in sync with provider events
//! 2. **Exit Handler Routing**: Route fill events for SL/TP placement
//! 3. **Event Broadcasting**: Send normalized events to strategies via WebSocket
//!
//! # Event Ordering Guarantees
//!
//! 1. State Manager updated BEFORE Exit Handler processes
//! 2. Exit orders placed BEFORE primary fill broadcast
//! 3. Bracket events broadcast AFTER primary fill
//!
//! This prevents race conditions where a strategy sees a fill and calls
//! `close_position` before Exit Handler has placed SL/TP.
//!
//! # Example
//!
//! ```ignore
//! use tektii_gateway_core::events::router::{EventRouter, EventRouterConfig};
//! use tektii_gateway_core::state::StateManager;
//!
//! let state_manager = Arc::new(StateManager::new());
//! // exit_handler implements ExitHandling trait
//! let (broadcaster, _) = broadcast::channel(256);
//!
//! let router = EventRouter::new(state_manager, exit_handler, broadcaster, platform);
//!
//! // Process incoming events
//! router.handle_order_event(event, order, parent_order_id).await;
//! router.handle_position_event(event, position).await;
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chrono::Utc;
use futures_util::stream::StreamExt;
use std::sync::OnceLock;

use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use rust_decimal::Decimal;

use crate::adapter::TradingAdapter;
use crate::error::GatewayError;
use crate::exit_management::{ExitHandling, FailedExitInfo, PlacementOutcome, PlacementResult};
use crate::models::{
    Order, OrderQueryParams, OrderStatus, Position, PositionSide, TradingPlatform,
};
use crate::state::StateManager;
use crate::trailing_stop::TrailingStopHandler;
use crate::websocket::messages::{
    ConnectionEventType, OrderEventType, PositionEventType, WsErrorCode, WsMessage,
};

/// Configuration for the Event Router.
#[derive(Debug, Clone)]
pub struct EventRouterConfig {
    /// Whether to broadcast events to strategies.
    /// Useful to disable in tests that only care about state updates.
    pub broadcast_enabled: bool,
}

impl Default for EventRouterConfig {
    fn default() -> Self {
        Self {
            broadcast_enabled: true,
        }
    }
}

/// Event Router for processing provider events.
///
/// Each adapter (Alpaca, Binance) has its own `EventRouter` instance, ensuring
/// isolated event processing and state management per provider.
pub struct EventRouter {
    /// Reference to the State Manager for caching orders/positions.
    state_manager: Arc<StateManager>,

    /// Reference to the Exit Handler for SL/TP management.
    exit_handler: Arc<dyn ExitHandling>,

    /// Broadcast channel for sending events to strategies.
    broadcaster: broadcast::Sender<WsMessage>,

    /// Platform this router is for.
    platform: TradingPlatform,

    /// Configuration.
    config: EventRouterConfig,

    /// Trading adapter for placing exit orders on fills.
    ///
    /// Set exactly once after construction via `set_adapter()` because the adapter
    /// and `EventRouter` have a circular dependency — the adapter creates the `EventRouter`,
    /// but the `EventRouter` needs the adapter reference to place exit orders.
    ///
    /// Uses `OnceLock` to enforce set-once semantics: the adapter is wired during
    /// startup (via `register_event_router`) and never changes. This eliminates
    /// read-lock overhead on every fill/position event.
    adapter: OnceLock<Arc<dyn TradingAdapter>>,

    /// Count of order events processed.
    order_events_processed: AtomicU64,

    /// Count of position events processed.
    position_events_processed: AtomicU64,

    /// Count of fill events routed to exit handler.
    fills_routed: AtomicU64,

    /// Count of critical notifications (e.g., `PositionUnprotected`) that could not
    /// be delivered because no WebSocket receivers were connected.
    critical_notifications_dropped: AtomicU64,

    /// Guard to prevent concurrent reconciliation runs.
    reconciling: AtomicBool,

    /// Trailing stop handler for activating tracking on fills and cancelling on reject/cancel.
    ///
    /// Set once after construction via `set_trailing_stop_handler()`.
    /// Uses `OnceLock` for set-once semantics (same pattern as `adapter`).
    trailing_stop_handler: OnceLock<Arc<TrailingStopHandler>>,
}

impl std::fmt::Debug for EventRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventRouter")
            .field("platform", &self.platform)
            .field(
                "order_events_processed",
                &self.order_events_processed.load(Ordering::Relaxed),
            )
            .field(
                "position_events_processed",
                &self.position_events_processed.load(Ordering::Relaxed),
            )
            .field("fills_routed", &self.fills_routed.load(Ordering::Relaxed))
            .field(
                "critical_notifications_dropped",
                &self.critical_notifications_dropped.load(Ordering::Relaxed),
            )
            .field("reconciling", &self.reconciling.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// RAII guard that resets the reconciliation flag on drop.
struct ReconciliationGuard<'a>(&'a AtomicBool);

impl Drop for ReconciliationGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// These methods are public API for library consumers and tests.
// The binary uses handle_order_event and handle_position_event via ProviderRegistry,
// but other methods are exported for external use.
#[allow(dead_code)]
impl EventRouter {
    /// Create a new Event Router.
    ///
    /// # Arguments
    ///
    /// * `state_manager` - Reference to the shared State Manager
    /// * `exit_handler` - Reference to the Exit Handler for this adapter
    /// * `broadcaster` - Broadcast channel for sending events to strategies
    /// * `platform` - The platform this router is for
    #[must_use]
    pub fn new(
        state_manager: Arc<StateManager>,
        exit_handler: Arc<dyn ExitHandling>,
        broadcaster: broadcast::Sender<WsMessage>,
        platform: TradingPlatform,
    ) -> Self {
        Self {
            state_manager,
            exit_handler,
            broadcaster,
            platform,
            config: EventRouterConfig::default(),
            adapter: OnceLock::new(),
            order_events_processed: AtomicU64::new(0),
            position_events_processed: AtomicU64::new(0),
            fills_routed: AtomicU64::new(0),
            critical_notifications_dropped: AtomicU64::new(0),
            reconciling: AtomicBool::new(false),
            trailing_stop_handler: OnceLock::new(),
        }
    }

    /// Create a new Event Router with custom configuration.
    #[must_use]
    pub fn with_config(
        state_manager: Arc<StateManager>,
        exit_handler: Arc<dyn ExitHandling>,
        broadcaster: broadcast::Sender<WsMessage>,
        platform: TradingPlatform,
        config: EventRouterConfig,
    ) -> Self {
        Self {
            state_manager,
            exit_handler,
            broadcaster,
            platform,
            config,
            adapter: OnceLock::new(),
            order_events_processed: AtomicU64::new(0),
            position_events_processed: AtomicU64::new(0),
            fills_routed: AtomicU64::new(0),
            critical_notifications_dropped: AtomicU64::new(0),
            reconciling: AtomicBool::new(false),
            trailing_stop_handler: OnceLock::new(),
        }
    }

    /// Set the trading adapter for exit order placement.
    ///
    /// Must be called exactly once after construction because adapters and `EventRouters`
    /// have a circular dependency — the adapter creates the `EventRouter`,
    /// but the `EventRouter` needs the adapter reference to place exit orders.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the adapter has already been set (set-once semantics).
    pub fn set_adapter(
        &self,
        adapter: Arc<dyn TradingAdapter>,
    ) -> Result<(), Arc<dyn TradingAdapter>> {
        self.adapter.set(adapter)
    }

    /// Set the trailing stop handler for activating tracking on fills.
    ///
    /// Optional — if not set, trailing stop events are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the handler has already been set.
    pub fn set_trailing_stop_handler(
        &self,
        handler: Arc<TrailingStopHandler>,
    ) -> Result<(), Arc<TrailingStopHandler>> {
        self.trailing_stop_handler.set(handler)
    }

    // =========================================================================
    // Public Query Methods
    // =========================================================================

    /// Get the number of order events processed.
    #[must_use]
    pub fn order_events_processed(&self) -> u64 {
        self.order_events_processed.load(Ordering::Relaxed)
    }

    /// Get the number of position events processed.
    #[must_use]
    pub fn position_events_processed(&self) -> u64 {
        self.position_events_processed.load(Ordering::Relaxed)
    }

    /// Get reference to the `StateManager`.
    ///
    /// Used by `ProviderRegistry` to check position existence for determining
    /// position event types (opened vs modified).
    #[must_use]
    pub fn state_manager(&self) -> &StateManager {
        &self.state_manager
    }

    /// Get the number of fills routed to exit handler.
    #[must_use]
    pub fn fills_routed(&self) -> u64 {
        self.fills_routed.load(Ordering::Relaxed)
    }

    /// Get the number of critical notifications dropped due to no WebSocket receivers.
    #[must_use]
    pub fn critical_notifications_dropped(&self) -> u64 {
        self.critical_notifications_dropped.load(Ordering::Relaxed)
    }

    /// Get the platform this router is for.
    #[must_use]
    pub const fn platform(&self) -> TradingPlatform {
        self.platform
    }

    /// Get a clone of the broadcaster for direct event sending.
    ///
    /// Used by WebSocket providers that need to broadcast events not handled
    /// by the router (e.g., candle/trade/account events in simulation mode).
    #[must_use]
    pub fn broadcaster(&self) -> broadcast::Sender<WsMessage> {
        self.broadcaster.clone()
    }

    // =========================================================================
    // Order Event Handling
    // =========================================================================

    /// Handle an order event from the provider.
    ///
    /// This is the main entry point for order-related events. It:
    /// 1. Updates the State Manager
    /// 2. Routes fill events to Exit Handler (using stored adapter)
    /// 3. Broadcasts the event to strategies
    ///
    /// The ordering guarantees that State Manager is updated before Exit Handler
    /// processes, and Exit Handler completes before the event is broadcast.
    ///
    /// # Arguments
    ///
    /// * `event` - The type of order event
    /// * `order` - The current order state
    /// * `parent_order_id` - Parent order ID for bracket/OCO child orders
    pub async fn handle_order_event(
        &self,
        event: OrderEventType,
        order: &Order,
        parent_order_id: Option<&str>,
    ) {
        Self::log_order_event(event, order);

        // Step 1: Update State Manager FIRST
        self.update_state_for_order_event(event, order);

        // Step 2: Route fills to Exit Handler BEFORE broadcast
        if Self::is_fill_event(event) {
            if let Some(adapter) = self.adapter.get() {
                self.route_fill_to_exit_handler(order, adapter.as_ref())
                    .await;
            } else {
                error!(
                    order_id = %order.id,
                    "Fill event received but no adapter set — exit orders NOT placed. Position unprotected. Call set_adapter() before processing events."
                );
            }
        }

        // Step 2.5: Route trailing stop lifecycle events
        self.route_trailing_stop_event(event, order).await;

        // Step 2.75: Cancel OCO siblings on full fill or rejection
        // Note: Partial fills do NOT trigger sibling cancellation
        // Note: Manual cancellations do NOT cascade (user intent respected)
        if matches!(
            event,
            OrderEventType::OrderFilled | OrderEventType::OrderRejected
        ) && let Some(adapter) = self.adapter.get()
        {
            self.cancel_oco_siblings(order, adapter.as_ref()).await;
        }

        // Step 3: Broadcast to strategies AFTER exit handler completes
        self.broadcast_order_event(event, order, parent_order_id);

        self.order_events_processed.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(
            "gateway_order_events_total",
            "platform" => self.platform.header_value(),
            "event_type" => event.metric_label(),
        )
        .increment(1);
    }

    /// Handle an order cancellation event.
    ///
    /// When a primary order is cancelled, this also cancels any pending
    /// exit orders associated with it.
    pub async fn handle_order_cancelled(&self, order: &Order) {
        info!(
            order_id = %order.id,
            symbol = %order.symbol,
            side = ?order.side,
            "Order cancelled"
        );

        // Update State Manager
        let _ = self.state_manager.remove_order(&order.id);

        // Cancel any pending exit orders for this primary order
        if self.exit_handler.has_pending_for_primary(&order.id) {
            match self.exit_handler.handle_cancellation(&order.id).await {
                Ok(result) => {
                    info!(
                        order_id = %order.id,
                        cancelled_count = result.cancelled.len(),
                        "Cancelled pending exit orders due to primary order cancellation"
                    );
                }
                Err(e) => {
                    warn!(
                        order_id = %order.id,
                        error = %e,
                        "Failed to cancel pending exit orders"
                    );
                }
            }
        }

        // Broadcast cancellation
        self.broadcast_order_event(OrderEventType::OrderCancelled, order, None);

        self.order_events_processed.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(
            "gateway_order_events_total",
            "platform" => self.platform.header_value(),
            "event_type" => "cancelled",
        )
        .increment(1);
    }

    /// Synthesize and broadcast a position event from a fill with `position_qty`.
    ///
    /// This is called when Alpaca (or other providers) sends a fill event with
    /// `position_qty` (net position after fill). We compare against `StateManager`
    /// to determine the event type and build a synthetic position.
    ///
    /// # Arguments
    ///
    /// * `order` - The filled order
    /// * `position_qty` - Net position quantity after the fill (from provider)
    ///
    /// # Event Type Detection
    ///
    /// - `0 -> non-zero` = `PositionOpened`
    /// - `non-zero -> different non-zero` = `PositionModified`
    /// - `non-zero -> 0` = `PositionClosed`
    pub async fn synthesize_position_from_fill(&self, order: &Order, position_qty: Decimal) {
        let symbol = &order.symbol;
        let position_qty_decimal = position_qty.abs();

        // Get previous position state from StateManager
        let previous_position = self.state_manager.get_position_by_symbol(symbol);
        let previous_qty = previous_position
            .as_ref()
            .map_or(Decimal::ZERO, |p| p.quantity);

        // Determine event type based on quantity changes
        let event_type = match (previous_qty.is_zero(), position_qty_decimal.is_zero()) {
            (true, false) => PositionEventType::PositionOpened,
            (false, true) => PositionEventType::PositionClosed,
            (false, false) if previous_qty != position_qty_decimal => {
                PositionEventType::PositionModified
            }
            _ => {
                // No meaningful change (same quantity or both zero)
                debug!(
                    symbol = %symbol,
                    previous_qty = %previous_qty,
                    new_qty = %position_qty_decimal,
                    "No position change detected, skipping position event"
                );
                return;
            }
        };

        // Build synthetic position
        let position_side = match position_qty.cmp(&Decimal::ZERO) {
            std::cmp::Ordering::Greater => PositionSide::Long,
            std::cmp::Ordering::Less => PositionSide::Short,
            std::cmp::Ordering::Equal => {
                // Position closed - use previous side or default to Long
                previous_position
                    .as_ref()
                    .map_or(PositionSide::Long, |p| p.side)
            }
        };

        let now = Utc::now();
        let position = Position {
            id: format!("{symbol}_position"),
            symbol: symbol.clone(),
            side: position_side,
            quantity: position_qty_decimal,
            average_entry_price: order.average_fill_price.unwrap_or(Decimal::ZERO),
            current_price: Decimal::ZERO, // Would need market data to populate
            unrealized_pnl: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            margin_mode: None,
            leverage: None,
            liquidation_price: None,
            opened_at: now, // Would use existing timestamp if we had it
            updated_at: now,
        };

        info!(
            symbol = %symbol,
            event = ?event_type,
            previous_qty = %previous_qty,
            new_qty = %position_qty_decimal,
            side = ?position_side,
            "Synthesized position event from fill"
        );

        // Use existing handle_position_event for state update and broadcast
        self.handle_position_event(event_type, &position).await;
    }

    /// Log a structured message for the given order event type.
    fn log_order_event(event: OrderEventType, order: &Order) {
        match event {
            OrderEventType::OrderCreated | OrderEventType::BracketOrderCreated => {
                info!(
                    order_id = %order.id,
                    symbol = %order.symbol,
                    side = ?order.side,
                    order_type = ?order.order_type,
                    quantity = %order.quantity,
                    status = ?order.status,
                    "Order acknowledged"
                );
            }
            OrderEventType::OrderPartiallyFilled => {
                info!(
                    order_id = %order.id,
                    symbol = %order.symbol,
                    side = ?order.side,
                    filled_quantity = %order.filled_quantity,
                    remaining_quantity = %order.remaining_quantity,
                    average_fill_price = ?order.average_fill_price,
                    "Order partially filled"
                );
            }
            OrderEventType::OrderFilled => {
                info!(
                    order_id = %order.id,
                    symbol = %order.symbol,
                    side = ?order.side,
                    filled_quantity = %order.filled_quantity,
                    average_fill_price = ?order.average_fill_price,
                    "Order filled"
                );
            }
            OrderEventType::OrderRejected => {
                warn!(
                    order_id = %order.id,
                    symbol = %order.symbol,
                    side = ?order.side,
                    reject_reason = ?order.reject_reason,
                    "Order rejected"
                );
            }
            OrderEventType::OrderExpired => {
                info!(
                    order_id = %order.id,
                    symbol = %order.symbol,
                    "Order expired"
                );
            }
            _ => {
                debug!(
                    order_id = %order.id,
                    event = ?event,
                    status = ?order.status,
                    "Processing order event"
                );
            }
        }
    }

    /// Route an order event to the trailing stop handler if one is configured.
    ///
    /// On fill: activates trailing stop tracking at the fill price.
    /// On cancel/reject: removes the trailing stop entry.
    async fn route_trailing_stop_event(&self, event: OrderEventType, order: &Order) {
        let Some(ts_handler) = self.trailing_stop_handler.get() else {
            return;
        };

        match event {
            OrderEventType::OrderFilled => {
                let fill_price = order.average_fill_price.unwrap_or(Decimal::ZERO);
                if let Err(e) = ts_handler.on_primary_filled(&order.id, fill_price).await {
                    warn!(
                        order_id = %order.id,
                        error = %e,
                        "Failed to activate trailing stop on fill"
                    );
                }
            }
            OrderEventType::OrderCancelled => {
                if let Err(e) = ts_handler.on_primary_cancelled(&order.id) {
                    warn!(
                        order_id = %order.id,
                        error = %e,
                        "Failed to cancel trailing stop on primary cancellation"
                    );
                }
            }
            OrderEventType::OrderRejected => {
                if let Err(e) = ts_handler.on_primary_rejected(&order.id) {
                    warn!(
                        order_id = %order.id,
                        error = %e,
                        "Failed to cancel trailing stop on primary rejection"
                    );
                }
            }
            _ => {}
        }
    }

    /// Update State Manager based on order event type.
    fn update_state_for_order_event(&self, event: OrderEventType, order: &Order) {
        match event {
            OrderEventType::OrderCreated
            | OrderEventType::OrderModified
            | OrderEventType::BracketOrderCreated
            | OrderEventType::BracketOrderModified => {
                self.state_manager.upsert_order(order);
            }
            OrderEventType::OrderPartiallyFilled => {
                // Update with partial fill
                let _ = self.state_manager.update_order_fill(
                    &order.id,
                    order.filled_quantity,
                    order.status,
                );
            }
            OrderEventType::OrderFilled => {
                // Remove from active orders (completed)
                let _ = self.state_manager.remove_order(&order.id);
            }
            OrderEventType::OrderCancelled
            | OrderEventType::OrderRejected
            | OrderEventType::OrderExpired => {
                // Remove from active orders (terminal state)
                let _ = self.state_manager.remove_order(&order.id);
            }
        }
    }

    /// Check if an event type represents a fill.
    const fn is_fill_event(event: OrderEventType) -> bool {
        matches!(
            event,
            OrderEventType::OrderFilled | OrderEventType::OrderPartiallyFilled
        )
    }

    /// Route a fill event to the Exit Handler for SL/TP placement.
    async fn route_fill_to_exit_handler(&self, order: &Order, adapter: &dyn TradingAdapter) {
        info!(
            order_id = %order.id,
            filled_qty = %order.filled_quantity,
            total_qty = %order.quantity,
            "Routing fill to exit handler"
        );

        // Note: position_qty is not available from the Order type
        // Exit Handler will use filled_qty for sizing
        let results = self
            .exit_handler
            .handle_fill(
                &order.id,
                order.filled_quantity,
                order.quantity,
                None,
                adapter,
            )
            .await;

        let success_count = results.iter().filter(|r| r.outcome.is_success()).count();
        let failed_count = results.iter().filter(|r| r.outcome.is_failed()).count();

        if success_count > 0 || failed_count > 0 {
            info!(
                order_id = %order.id,
                success_count,
                failed_count,
                "Exit handler fill processing complete"
            );
        }

        // Notify strategy if any exit orders failed or were deferred due to circuit breaker
        self.broadcast_unprotected_position_if_needed(order, &results)
            .await;

        self.fills_routed.fetch_add(1, Ordering::Relaxed);
    }

    /// Broadcast a `PositionUnprotected` error if exit orders failed or circuit breaker blocked them.
    ///
    /// This notifies the strategy that their position may not have SL/TP protection,
    /// allowing them to take manual action (e.g., close the position).
    async fn broadcast_unprotected_position_if_needed(
        &self,
        order: &Order,
        results: &[PlacementResult],
    ) {
        if results.is_empty() {
            return;
        }

        // Collect failed exit types
        let mut failed_exits = Vec::new();
        for result in results {
            match &result.outcome {
                PlacementOutcome::Failed { error, .. } => {
                    failed_exits.push(serde_json::json!({
                        "type": format!("{:?}", result.order_type),
                        "error": error,
                        "actual_order_ids": [],
                    }));
                }
                PlacementOutcome::Deferred { reason } if reason.contains("Circuit breaker") => {
                    failed_exits.push(serde_json::json!({
                        "type": format!("{:?}", result.order_type),
                        "error": reason,
                        "actual_order_ids": [],
                    }));
                }
                _ => {}
            }
        }

        if failed_exits.is_empty() {
            return;
        }

        let circuit_breaker_open = self.exit_handler.is_circuit_breaker_open().await;

        let details = serde_json::json!({
            "order_id": order.id,
            "symbol": order.symbol,
            "failed_exits": failed_exits,
            "circuit_breaker_open": circuit_breaker_open,
        });

        error!(
            order_id = %order.id,
            symbol = %order.symbol,
            failed_count = failed_exits.len(),
            circuit_breaker_open,
            "Position unprotected — exit order placement failed"
        );

        let message = WsMessage::error_with_details(
            WsErrorCode::PositionUnprotected,
            format!(
                "Exit order placement failed for {} — position may be unprotected",
                order.symbol
            ),
            details,
        );

        if self.broadcaster.send(message).is_err() {
            error!(
                order_id = %order.id,
                symbol = %order.symbol,
                "CRITICAL: PositionUnprotected notification dropped — no WebSocket receivers connected. Strategy may not know position is unprotected."
            );
            self.critical_notifications_dropped
                .fetch_add(1, Ordering::Relaxed);
            metrics::counter!(
                "gateway_critical_notifications_dropped_total",
                "platform" => self.platform.header_value()
            )
            .increment(1);
        }
    }

    /// Cancel OCO sibling orders when one order fills or is rejected.
    ///
    /// This implements client-side OCO (One-Cancels-Other) behavior where orders
    /// sharing the same `oco_group_id` are linked. When one fills completely or
    /// is rejected, all siblings are automatically cancelled.
    ///
    /// # Behavior
    ///
    /// - Full fill: Cancel ALL siblings in same OCO group
    /// - Rejection: Cancel ALL siblings (protection failed)
    /// - Partial fill: No action (handled elsewhere)
    /// - Manual cancel: No cascade (user intent respected)
    ///
    /// # Double-Exit Detection
    ///
    /// If two orders fill simultaneously (race condition at the provider), both
    /// fill events arrive and both attempt to cancel the other. The second fill
    /// is detected via `record_oco_fill` and triggers an `OcoDoubleExit` alert
    /// to connected strategies so they can take corrective action.
    async fn cancel_oco_siblings(&self, filled_order: &Order, adapter: &dyn TradingAdapter) {
        // Check if order is part of an OCO group
        // First try the order's oco_group_id field, then fall back to StateManager lookup
        // (WebSocket events from provider may not have oco_group_id populated)
        let group_id = filled_order
            .oco_group_id
            .clone()
            .or_else(|| self.state_manager.get_oco_group_id(&filled_order.id));

        let Some(group_id) = group_id else {
            return; // Not part of any OCO group
        };

        // Double-exit detection: record this fill and check if one was already recorded
        if let Some(previous_fill) = self.state_manager.record_oco_fill(
            &group_id,
            &filled_order.id,
            filled_order.filled_quantity,
            &filled_order.symbol,
            filled_order.side,
        ) {
            let total_exit_qty = previous_fill.first_filled_qty + filled_order.filled_quantity;

            error!(
                oco_group = %group_id,
                first_order = %previous_fill.first_filled_order_id,
                first_qty = %previous_fill.first_filled_qty,
                second_order = %filled_order.id,
                second_qty = %filled_order.filled_quantity,
                total_exit_qty = %total_exit_qty,
                symbol = %filled_order.symbol,
                "OCO DOUBLE EXIT: both legs filled at provider — position exited at excess quantity"
            );

            metrics::counter!(
                "gateway_oco_double_exit_total",
                "platform" => self.platform.header_value(),
            )
            .increment(1);

            self.broadcast_oco_double_exit(&group_id, &previous_fill, filled_order, total_exit_qty);

            return;
        }

        // Get sibling order IDs (excluding the filled order)
        let siblings = self
            .state_manager
            .get_oco_siblings(&group_id, &filled_order.id);

        if siblings.is_empty() {
            return;
        }

        info!(
            oco_group = %group_id,
            filled_order = %filled_order.id,
            sibling_count = siblings.len(),
            "Cancelling OCO siblings"
        );

        for sibling_id in siblings {
            match adapter.cancel_order(&sibling_id).await {
                Ok(_) => {
                    info!(
                        oco_group = %group_id,
                        cancelled_order = %sibling_id,
                        triggered_by = %filled_order.id,
                        "OCO sibling cancelled"
                    );
                }
                Err(e) => {
                    let error_str = e.to_string();
                    if error_str.contains("not found")
                        || error_str.contains("already")
                        || error_str.contains("terminal")
                    {
                        warn!(
                            oco_group = %group_id,
                            sibling_id = %sibling_id,
                            error = %e,
                            "OCO sibling already terminal (possible race condition)"
                        );
                    } else {
                        error!(
                            oco_group = %group_id,
                            sibling_id = %sibling_id,
                            error = %e,
                            "Failed to cancel OCO sibling"
                        );
                    }
                }
            }
        }
    }

    /// Broadcast an OCO double-exit alert to connected strategies.
    fn broadcast_oco_double_exit(
        &self,
        oco_group_id: &str,
        first_fill: &crate::state::OcoFillRecord,
        second_fill: &Order,
        total_exit_qty: Decimal,
    ) {
        let details = serde_json::json!({
            "oco_group_id": oco_group_id,
            "symbol": second_fill.symbol,
            "first_fill": {
                "order_id": first_fill.first_filled_order_id,
                "quantity": first_fill.first_filled_qty.to_string(),
                "side": format!("{:?}", first_fill.side),
            },
            "second_fill": {
                "order_id": second_fill.id,
                "quantity": second_fill.filled_quantity.to_string(),
                "side": format!("{:?}", second_fill.side),
            },
            "total_exit_quantity": total_exit_qty.to_string(),
        });

        let message = WsMessage::error_with_details(
            WsErrorCode::OcoDoubleExit,
            format!(
                "OCO double-exit on {}: both SL and TP filled (total exit qty: {total_exit_qty})",
                second_fill.symbol
            ),
            details,
        );

        if self.broadcaster.send(message).is_err() {
            error!(
                oco_group = %oco_group_id,
                symbol = %second_fill.symbol,
                "CRITICAL: OcoDoubleExit notification dropped — no WebSocket receivers connected"
            );
            self.critical_notifications_dropped
                .fetch_add(1, Ordering::Relaxed);
            metrics::counter!(
                "gateway_critical_notifications_dropped_total",
                "platform" => self.platform.header_value()
            )
            .increment(1);
        }
    }

    /// Broadcast an order event to strategies.
    fn broadcast_order_event(
        &self,
        event: OrderEventType,
        order: &Order,
        parent_order_id: Option<&str>,
    ) {
        if !self.config.broadcast_enabled {
            return;
        }

        // Only clone if there are receivers listening
        if self.broadcaster.receiver_count() == 0 {
            return;
        }

        let message = WsMessage::Order {
            event,
            order: order.clone(),
            parent_order_id: parent_order_id.map(String::from),
            timestamp: Utc::now(),
        };

        // broadcast::send returns Err only if there are no receivers
        // This is expected during startup/shutdown, so we ignore it
        let _ = self.broadcaster.send(message);
    }

    // =========================================================================
    // Position Event Handling
    // =========================================================================

    /// Handle a position event from the provider.
    ///
    /// This updates the State Manager and broadcasts the event to strategies.
    /// Position closes may trigger cleanup of associated exit orders.
    ///
    /// # Arguments
    ///
    /// * `event` - The type of position event
    /// * `position` - The current position state
    pub async fn handle_position_event(&self, event: PositionEventType, position: &Position) {
        debug!(
            position_id = %position.id,
            event = ?event,
            symbol = %position.symbol,
            quantity = %position.quantity,
            "Processing position event"
        );

        // Update State Manager
        match event {
            PositionEventType::PositionOpened | PositionEventType::PositionModified => {
                self.state_manager.upsert_position(position);
            }
            PositionEventType::PositionClosed => {
                let _ = self.state_manager.remove_position(&position.id);

                // Cancel any pending exit orders for this symbol
                if let Some(adapter) = self.adapter.get() {
                    let cancelled = self
                        .exit_handler
                        .cancel_for_position_close(&position.symbol, adapter.as_ref())
                        .await;

                    if !cancelled.is_empty() {
                        info!(
                            position_id = %position.id,
                            symbol = %position.symbol,
                            cancelled_count = cancelled.len(),
                            "Cancelled exit orders due to position close"
                        );
                    }
                }
            }
        }

        // Broadcast to strategies
        self.broadcast_position_event(event, position);

        self.position_events_processed
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Broadcast a position event to strategies.
    fn broadcast_position_event(&self, event: PositionEventType, position: &Position) {
        if !self.config.broadcast_enabled {
            return;
        }

        // Only clone if there are receivers listening
        if self.broadcaster.receiver_count() == 0 {
            return;
        }

        let message = WsMessage::Position {
            event,
            position: position.clone(),
            timestamp: Utc::now(),
        };

        let _ = self.broadcaster.send(message);
    }

    // =========================================================================
    // Connection Event Handling
    // =========================================================================

    /// Handle a connection event (connected, disconnecting, reconnecting).
    ///
    /// On reconnect, the caller should perform a full state sync by
    /// calling `sync_state_from_provider`.
    pub fn handle_connection_event(&self, event: ConnectionEventType, error: Option<&str>) {
        if !self.config.broadcast_enabled {
            return;
        }

        let message = WsMessage::Connection {
            event,
            error: error.map(String::from),
            broker: None,
            gap_duration_ms: None,
            timestamp: Utc::now(),
        };

        let _ = self.broadcaster.send(message);
    }

    // =========================================================================
    // State Synchronization
    // =========================================================================

    /// Reconcile state after a WebSocket reconnection.
    ///
    /// During a disconnect, fills and cancellations may have occurred that the
    /// strategy never received. This method:
    ///
    /// 1. Snapshots all order IDs currently in `StateManager`
    /// 2. Queries the provider for each order's fresh status (with per-query timeout)
    /// 3. Detects status changes (fills, cancellations, partial fills)
    /// 4. Routes detected changes through internal event pipeline (state update,
    ///    exit handler routing, broadcast to strategies)
    /// 5. Syncs state with fresh provider data (open orders + positions)
    /// 6. Broadcasts `ConnectionEventType::Connected` only if state sync succeeded
    ///
    /// Fill events are broadcast BEFORE the Connected event so strategies
    /// can process missed fills before resuming normal operation.
    ///
    /// Only one reconciliation runs at a time — concurrent reconnect signals
    /// are skipped with a warning.
    #[allow(clippy::too_many_lines)]
    pub async fn reconcile_after_reconnect(&self) {
        // Fix #1: Prevent concurrent reconciliation runs
        if self
            .reconciling
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            warn!(
                platform = ?self.platform,
                "Reconciliation already in progress, skipping duplicate signal"
            );
            return;
        }

        // Ensure the flag is cleared on all exit paths
        let _guard = ReconciliationGuard(&self.reconciling);

        let Some(adapter) = self.adapter.get() else {
            error!("Cannot reconcile: no adapter set. Call set_adapter() first.");
            return;
        };

        let order_ids = self.state_manager.get_all_order_ids();
        let order_count = order_ids.len();
        info!(
            tracked_orders = order_count,
            platform = ?self.platform,
            "Starting post-reconnect reconciliation"
        );

        // Step 1: Fetch all tracked orders from provider concurrently
        let mut changes_detected: u32 = 0;
        let mut errors: u32 = 0;

        // Phase 1: Concurrent fetch — query provider for each order in parallel
        // Bounded to 10 concurrent requests to avoid overwhelming the broker API
        let fetch_results: Vec<_> =
            futures_util::stream::iter(order_ids.into_iter().map(|order_id| {
                let cached = self.state_manager.get_order(&order_id);
                let cached_status = cached.as_ref().map(|c| c.status);
                let cached_filled_qty =
                    cached.as_ref().map_or(Decimal::ZERO, |c| c.filled_quantity);
                let adapter = adapter.clone();
                async move {
                    let query_result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        adapter.get_order(&order_id),
                    )
                    .await;
                    (order_id, cached_status, cached_filled_qty, query_result)
                }
            }))
            .buffer_unordered(10)
            .collect()
            .await;

        // Phase 2: Sequential process — maintain ordering invariants
        for (order_id, cached_status, cached_filled_qty, query_result) in fetch_results {
            let Ok(query_result) = query_result else {
                warn!(
                    order_id = %order_id,
                    "Timed out querying order during reconciliation (5s)"
                );
                errors += 1;
                continue;
            };

            match query_result {
                Ok(provider_order) => {
                    let event = Self::detect_status_change(
                        cached_status,
                        cached_filled_qty,
                        &provider_order,
                    );

                    if let Some(event_type) = event {
                        changes_detected += 1;
                        info!(
                            order_id = %order_id,
                            cached_status = ?cached_status,
                            provider_status = ?provider_order.status,
                            event = ?event_type,
                            "Detected order change during disconnect"
                        );
                        self.update_state_for_order_event(event_type, &provider_order);

                        if Self::is_fill_event(event_type) {
                            self.route_fill_to_exit_handler(&provider_order, adapter.as_ref())
                                .await;
                        }

                        self.broadcast_order_event(event_type, &provider_order, None);
                        self.order_events_processed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(GatewayError::OrderNotFound { .. }) => {
                    changes_detected += 1;
                    warn!(
                        order_id = %order_id,
                        "Order not found at provider during reconciliation — removing from cache"
                    );
                    let _ = self.state_manager.remove_order(&order_id);
                }
                Err(e) => {
                    errors += 1;
                    warn!(
                        order_id = %order_id,
                        error = %e,
                        "Failed to query order during reconciliation"
                    );
                }
            }
        }

        // Step 2: Sync full state from provider (open orders + positions)
        // Fix #3: Only broadcast Connected if both queries succeed
        let open_orders = match adapter
            .get_orders(&OrderQueryParams {
                status: Some(vec![
                    OrderStatus::Open,
                    OrderStatus::PartiallyFilled,
                    OrderStatus::Pending,
                ]),
                ..Default::default()
            })
            .await
        {
            Ok(orders) => orders,
            Err(e) => {
                error!(
                    error = %e,
                    platform = ?self.platform,
                    "Failed to fetch open orders during reconciliation — skipping Connected broadcast"
                );
                return;
            }
        };

        let positions = match adapter.get_positions(None).await {
            Ok(pos) => pos,
            Err(e) => {
                error!(
                    error = %e,
                    platform = ?self.platform,
                    "Failed to fetch positions during reconciliation — skipping Connected broadcast"
                );
                return;
            }
        };

        self.sync_state_from_provider(open_orders, positions);

        // Step 3: Re-broadcast PositionUnprotected for any failed exit entries
        // These notifications may have been dropped during the disconnect.
        let failed_entries = self.exit_handler.get_failed_entries();
        if !failed_entries.is_empty() {
            info!(
                failed_count = failed_entries.len(),
                platform = ?self.platform,
                "Re-broadcasting unprotected positions on reconnect"
            );
            self.rebroadcast_unprotected_positions(&failed_entries)
                .await;

            // Clear failed entries after rebroadcast to prevent unbounded accumulation.
            let cleared = self.exit_handler.clear_failed_entries();
            debug!(cleared, "Cleared failed exit entries after rebroadcast");
        }

        // Step 4: Broadcast Connected AFTER all reconciliation events and successful sync
        info!(
            platform = ?self.platform,
            orders_checked = order_count,
            changes_detected,
            errors,
            "Post-reconnect reconciliation complete"
        );
        self.handle_connection_event(ConnectionEventType::Connected, None);
    }

    /// Re-broadcast `PositionUnprotected` errors for failed exit entries.
    ///
    /// Called during reconnect reconciliation to recover notifications that may
    /// have been dropped when the strategy was disconnected. Groups entries by
    /// primary order ID so each unprotected position gets exactly one notification.
    async fn rebroadcast_unprotected_positions(&self, failed_entries: &[FailedExitInfo]) {
        if failed_entries.is_empty() {
            return;
        }

        // Group by primary_order_id — one broadcast per position
        let mut by_primary: std::collections::HashMap<&str, Vec<&FailedExitInfo>> =
            std::collections::HashMap::new();
        for entry in failed_entries {
            by_primary
                .entry(&entry.primary_order_id)
                .or_default()
                .push(entry);
        }

        let circuit_breaker_open = self.exit_handler.is_circuit_breaker_open().await;

        for (primary_order_id, entries) in &by_primary {
            let symbol = &entries[0].symbol;

            let failed_exits: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "type": format!("{:?}", e.order_type),
                        "error": e.error,
                        "actual_order_ids": e.actual_order_ids,
                    })
                })
                .collect();

            let details = serde_json::json!({
                "order_id": primary_order_id,
                "symbol": symbol,
                "failed_exits": failed_exits,
                "circuit_breaker_open": circuit_breaker_open,
                "rebroadcast": true,
            });

            warn!(
                order_id = %primary_order_id,
                symbol = %symbol,
                failed_count = entries.len(),
                "Re-broadcasting PositionUnprotected on reconnect"
            );

            let message = WsMessage::error_with_details(
                WsErrorCode::PositionUnprotected,
                format!(
                    "Exit order placement failed for {symbol} — position may be unprotected (re-broadcast on reconnect)"
                ),
                details,
            );

            if self.broadcaster.send(message).is_err() {
                error!(
                    order_id = %primary_order_id,
                    symbol = %symbol,
                    "CRITICAL: PositionUnprotected re-broadcast dropped — no WebSocket receivers connected."
                );
                self.critical_notifications_dropped
                    .fetch_add(1, Ordering::Relaxed);
                metrics::counter!(
                    "gateway_critical_notifications_dropped_total",
                    "platform" => self.platform.header_value()
                )
                .increment(1);
            }
        }
    }

    /// Detect what event type represents the status change between cached and provider state.
    ///
    /// Returns `None` if no meaningful change is detected.
    fn detect_status_change(
        cached_status: Option<OrderStatus>,
        cached_filled_qty: Decimal,
        provider_order: &Order,
    ) -> Option<OrderEventType> {
        let provider_status = provider_order.status;

        match provider_status {
            OrderStatus::Filled if cached_status != Some(OrderStatus::Filled) => {
                Some(OrderEventType::OrderFilled)
            }
            OrderStatus::Cancelled if cached_status != Some(OrderStatus::Cancelled) => {
                Some(OrderEventType::OrderCancelled)
            }
            OrderStatus::Rejected if cached_status != Some(OrderStatus::Rejected) => {
                Some(OrderEventType::OrderRejected)
            }
            OrderStatus::Expired if cached_status != Some(OrderStatus::Expired) => {
                Some(OrderEventType::OrderExpired)
            }
            OrderStatus::PartiallyFilled if provider_order.filled_quantity > cached_filled_qty => {
                Some(OrderEventType::OrderPartiallyFilled)
            }
            _ => None,
        }
    }

    /// Sync State Manager from provider data.
    ///
    /// Called on initial connect or reconnect to ensure state is accurate.
    /// This replaces all cached state with fresh data from the provider.
    pub fn sync_state_from_provider(&self, orders: Vec<Order>, positions: Vec<Position>) {
        info!(
            orders = orders.len(),
            positions = positions.len(),
            "Syncing state from provider"
        );

        self.state_manager.sync_from_provider(orders, positions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OrderStatus, OrderType, PositionSide, Side, TimeInForce};
    use crate::state::StateManager;
    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    // =========================================================================
    // Test Helpers
    // =========================================================================

    fn create_test_router() -> (EventRouter, broadcast::Receiver<WsMessage>) {
        let state_manager = Arc::new(StateManager::new());
        let exit_handler: Arc<dyn crate::exit_management::ExitHandling> =
            Arc::new(crate::exit_management::ExitHandler::with_defaults(
                Arc::clone(&state_manager),
                TradingPlatform::AlpacaLive,
            ));
        let (broadcaster, receiver) = broadcast::channel(16);

        let router = EventRouter::new(
            state_manager,
            exit_handler,
            broadcaster,
            TradingPlatform::AlpacaLive,
        );

        (router, receiver)
    }

    fn create_test_order(id: &str, status: OrderStatus, filled_qty: Decimal) -> Order {
        Order {
            id: id.to_string(),
            client_order_id: None,
            symbol: "BTCUSD".to_string(),
            side: Side::Buy,
            order_type: OrderType::Market,
            quantity: dec!(100),
            filled_quantity: filled_qty,
            remaining_quantity: dec!(100) - filled_qty,
            limit_price: None,
            stop_price: None,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            average_fill_price: None,
            status,
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

    fn create_test_position(id: &str, symbol: &str, quantity: Decimal) -> Position {
        Position {
            id: id.to_string(),
            symbol: symbol.to_string(),
            side: PositionSide::Long,
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
    // Position Synthesis Tests (Alpaca position_qty)
    // =========================================================================

    fn create_filled_order(id: &str, symbol: &str, side: Side, avg_price: Decimal) -> Order {
        Order {
            id: id.to_string(),
            client_order_id: None,
            symbol: symbol.to_string(),
            side,
            order_type: OrderType::Market,
            quantity: dec!(10),
            filled_quantity: dec!(10),
            remaining_quantity: Decimal::ZERO,
            limit_price: None,
            stop_price: None,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            average_fill_price: Some(avg_price),
            status: OrderStatus::Filled,
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

    #[tokio::test]
    async fn test_position_opened_on_first_fill() {
        let (router, mut rx) = create_test_router();
        let order = create_filled_order("order-1", "BTCUSD", Side::Buy, dec!(50000));

        assert!(
            router
                .state_manager
                .get_position_by_symbol("BTCUSD")
                .is_none()
        );

        router.synthesize_position_from_fill(&order, dec!(10)).await;

        let msg = rx.try_recv().unwrap();
        match msg {
            WsMessage::Position {
                event, position, ..
            } => {
                assert_eq!(event, PositionEventType::PositionOpened);
                assert_eq!(position.symbol, "BTCUSD");
                assert_eq!(position.quantity, dec!(10));
                assert_eq!(position.side, PositionSide::Long);
                assert_eq!(position.average_entry_price, dec!(50000));
            }
            _ => panic!("Expected Position message, got {:?}", msg),
        }

        let cached = router.state_manager.get_position_by_symbol("BTCUSD");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().quantity, dec!(10));
    }

    #[tokio::test]
    async fn test_position_modified_on_scale_in() {
        let (router, mut rx) = create_test_router();

        let existing = create_test_position("BTCUSD_position", "BTCUSD", dec!(10));
        router.state_manager.upsert_position(&existing);

        let order = create_filled_order("order-2", "BTCUSD", Side::Buy, dec!(51000));

        router.synthesize_position_from_fill(&order, dec!(15)).await;

        let msg = rx.try_recv().unwrap();
        match msg {
            WsMessage::Position {
                event, position, ..
            } => {
                assert_eq!(event, PositionEventType::PositionModified);
                assert_eq!(position.symbol, "BTCUSD");
                assert_eq!(position.quantity, dec!(15));
            }
            _ => panic!("Expected Position message, got {:?}", msg),
        }

        let cached = router
            .state_manager
            .get_position_by_symbol("BTCUSD")
            .unwrap();
        assert_eq!(cached.quantity, dec!(15));
    }

    #[tokio::test]
    async fn test_position_closed_on_full_exit() {
        let (router, mut rx) = create_test_router();

        let existing = create_test_position("BTCUSD_position", "BTCUSD", dec!(10));
        router.state_manager.upsert_position(&existing);

        let order = create_filled_order("order-3", "BTCUSD", Side::Sell, dec!(52000));

        router
            .synthesize_position_from_fill(&order, Decimal::ZERO)
            .await;

        let msg = rx.try_recv().unwrap();
        match msg {
            WsMessage::Position {
                event, position, ..
            } => {
                assert_eq!(event, PositionEventType::PositionClosed);
                assert_eq!(position.symbol, "BTCUSD");
                assert_eq!(position.quantity, Decimal::ZERO);
            }
            _ => panic!("Expected Position message, got {:?}", msg),
        }

        assert!(
            router
                .state_manager
                .get_position_by_symbol("BTCUSD")
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_position_flip_long_to_short() {
        let (router, mut rx) = create_test_router();

        let existing = create_test_position("BTCUSD_position", "BTCUSD", dec!(10));
        router.state_manager.upsert_position(&existing);

        let order = create_filled_order("order-4", "BTCUSD", Side::Sell, dec!(50000));

        router.synthesize_position_from_fill(&order, dec!(-5)).await;

        let msg = rx.try_recv().unwrap();
        match msg {
            WsMessage::Position {
                event, position, ..
            } => {
                assert_eq!(event, PositionEventType::PositionModified);
                assert_eq!(position.symbol, "BTCUSD");
                assert_eq!(position.quantity, dec!(5));
                assert_eq!(position.side, PositionSide::Short);
            }
            _ => panic!("Expected Position message, got {:?}", msg),
        }
    }

    #[tokio::test]
    async fn test_no_position_event_when_qty_unchanged() {
        let (router, mut rx) = create_test_router();

        let existing = create_test_position("BTCUSD_position", "BTCUSD", dec!(10));
        router.state_manager.upsert_position(&existing);

        let order = create_filled_order("order-5", "BTCUSD", Side::Buy, dec!(50000));

        router.synthesize_position_from_fill(&order, dec!(10)).await;

        let result = rx.try_recv();
        assert!(
            result.is_err(),
            "Expected no broadcast, but got: {:?}",
            result
        );

        assert_eq!(router.position_events_processed(), 0);
    }

    #[tokio::test]
    async fn test_short_position_opened() {
        let (router, mut rx) = create_test_router();
        let order = create_filled_order("order-6", "BTCUSD", Side::Sell, dec!(50000));

        assert!(
            router
                .state_manager
                .get_position_by_symbol("BTCUSD")
                .is_none()
        );

        router
            .synthesize_position_from_fill(&order, dec!(-10))
            .await;

        let msg = rx.try_recv().unwrap();
        match msg {
            WsMessage::Position {
                event, position, ..
            } => {
                assert_eq!(event, PositionEventType::PositionOpened);
                assert_eq!(position.side, PositionSide::Short);
                assert_eq!(position.quantity, dec!(10));
            }
            _ => panic!("Expected Position message, got {:?}", msg),
        }
    }

    // =========================================================================
    // Position Unprotected Notification Tests
    // =========================================================================

    use crate::adapter::{BracketStrategy, ProviderCapabilities};
    use crate::error::GatewayError;
    use crate::exit_management::ExitHandlerConfig;
    use crate::models::{Capabilities, OrderRequest as ModelOrderRequest, PositionMode};

    /// Mock adapter that always fails on submit_order.
    struct FailingAdapter;

    struct FailingCapabilities;

    impl ProviderCapabilities for FailingCapabilities {
        fn supports_bracket_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn supports_oco_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn supports_oto_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn bracket_strategy(&self, _order: &ModelOrderRequest) -> BracketStrategy {
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

    #[async_trait::async_trait]
    impl TradingAdapter for FailingAdapter {
        fn capabilities(&self) -> &dyn ProviderCapabilities {
            &FailingCapabilities
        }
        fn platform(&self) -> TradingPlatform {
            TradingPlatform::AlpacaLive
        }
        fn provider_name(&self) -> &'static str {
            "failing-mock"
        }
        async fn get_account(&self) -> crate::error::GatewayResult<crate::models::Account> {
            Err(GatewayError::internal("mock"))
        }
        async fn submit_order(
            &self,
            _request: &ModelOrderRequest,
        ) -> crate::error::GatewayResult<crate::models::OrderHandle> {
            Err(GatewayError::ProviderError {
                message: "Connection refused".to_string(),
                provider: Some("mock".to_string()),
                source: None,
            })
        }
        async fn get_order(&self, _order_id: &str) -> crate::error::GatewayResult<Order> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_orders(
            &self,
            _params: &OrderQueryParams,
        ) -> crate::error::GatewayResult<Vec<Order>> {
            Ok(vec![])
        }
        async fn get_order_history(
            &self,
            _params: &OrderQueryParams,
        ) -> crate::error::GatewayResult<Vec<Order>> {
            Ok(vec![])
        }
        async fn modify_order(
            &self,
            _order_id: &str,
            _request: &crate::models::ModifyOrderRequest,
        ) -> crate::error::GatewayResult<crate::models::ModifyOrderResult> {
            Err(GatewayError::internal("mock"))
        }
        async fn cancel_order(
            &self,
            _order_id: &str,
        ) -> crate::error::GatewayResult<crate::models::CancelOrderResult> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_trades(
            &self,
            _params: &crate::models::TradeQueryParams,
        ) -> crate::error::GatewayResult<Vec<crate::models::Trade>> {
            Ok(vec![])
        }
        async fn get_positions(
            &self,
            _symbol: Option<&str>,
        ) -> crate::error::GatewayResult<Vec<Position>> {
            Ok(vec![])
        }
        async fn get_position(&self, _position_id: &str) -> crate::error::GatewayResult<Position> {
            Err(GatewayError::internal("mock"))
        }
        async fn close_position(
            &self,
            _position_id: &str,
            _request: &crate::models::ClosePositionRequest,
        ) -> crate::error::GatewayResult<crate::models::OrderHandle> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_quote(
            &self,
            _symbol: &str,
        ) -> crate::error::GatewayResult<crate::models::Quote> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_bars(
            &self,
            _symbol: &str,
            _params: &crate::models::BarParams,
        ) -> crate::error::GatewayResult<Vec<crate::models::Bar>> {
            Ok(vec![])
        }
        async fn get_capabilities(&self) -> crate::error::GatewayResult<Capabilities> {
            Ok(Capabilities {
                supported_asset_classes: vec![],
                supported_order_types: vec![],
                position_mode: PositionMode::Netting,
                features: vec![],
                max_leverage: None,
                rate_limits: None,
            })
        }
        async fn get_connection_status(
            &self,
        ) -> crate::error::GatewayResult<crate::models::ConnectionStatus> {
            Err(GatewayError::internal("mock"))
        }
    }

    /// Create a router with an exit handler that has no retry delays (fail immediately)
    /// and a low circuit breaker threshold for testing.
    fn create_test_router_with_exit_handler() -> (
        EventRouter,
        Arc<crate::exit_management::ExitHandler>,
        broadcast::Receiver<WsMessage>,
    ) {
        let state_manager = Arc::new(StateManager::new());
        let config = ExitHandlerConfig {
            circuit_breaker_threshold: 1,
            retry_delays: vec![],
            ..ExitHandlerConfig::default()
        };
        let exit_handler = Arc::new(crate::exit_management::ExitHandler::new(
            Arc::clone(&state_manager),
            TradingPlatform::AlpacaLive,
            config,
        ));
        let (broadcaster, receiver) = broadcast::channel(16);

        let router = EventRouter::new(
            state_manager,
            Arc::clone(&exit_handler) as Arc<dyn crate::exit_management::ExitHandling>,
            broadcaster,
            TradingPlatform::AlpacaLive,
        );

        (router, exit_handler, receiver)
    }

    fn create_order_request_with_sl(symbol: &str, sl_price: Decimal) -> ModelOrderRequest {
        ModelOrderRequest {
            symbol: symbol.to_string(),
            side: Side::Buy,
            quantity: dec!(10),
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Gtc,
            limit_price: None,
            stop_price: None,
            stop_loss: Some(sl_price),
            take_profit: None,
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
    async fn test_failed_exit_placement_broadcasts_position_unprotected() {
        let (router, exit_handler, mut rx) = create_test_router_with_exit_handler();

        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        let order_request = create_order_request_with_sl("BTCUSD", dec!(45000));
        exit_handler.register("order-1", &order_request).unwrap();

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(10));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let unprotected = messages.iter().find(|msg| {
            matches!(
                msg,
                WsMessage::Error {
                    code: WsErrorCode::PositionUnprotected,
                    ..
                }
            )
        });

        assert!(
            unprotected.is_some(),
            "Expected PositionUnprotected error broadcast, got messages: {:?}",
            messages
        );

        if let WsMessage::Error {
            code,
            message,
            details,
            ..
        } = unprotected.unwrap()
        {
            assert_eq!(*code, WsErrorCode::PositionUnprotected);
            assert!(
                message.contains("unprotected"),
                "Message should mention 'unprotected': {message}"
            );
            let details = details.as_ref().expect("details should be present");
            assert_eq!(details["order_id"], "order-1");
            assert_eq!(details["symbol"], "BTCUSD");
        }
    }

    #[tokio::test]
    async fn test_circuit_breaker_open_broadcasts_position_unprotected() {
        let (router, exit_handler, mut rx) = create_test_router_with_exit_handler();

        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        // Trip the circuit breaker by processing a fill that fails
        let trip_order_request = create_order_request_with_sl("BTCUSD", dec!(45000));
        exit_handler
            .register("trip-order", &trip_order_request)
            .unwrap();
        let trip_fill = create_test_order("trip-order", OrderStatus::Filled, dec!(10));
        router
            .handle_order_event(OrderEventType::OrderFilled, &trip_fill, None)
            .await;

        while rx.try_recv().is_ok() {}

        assert!(exit_handler.is_circuit_breaker_open().await);

        let order_request = create_order_request_with_sl("ETHUSD", dec!(3000));
        exit_handler.register("order-2", &order_request).unwrap();

        let filled_order = create_test_order("order-2", OrderStatus::Filled, dec!(5));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let unprotected = messages.iter().find(|msg| {
            matches!(
                msg,
                WsMessage::Error {
                    code: WsErrorCode::PositionUnprotected,
                    ..
                }
            )
        });

        assert!(
            unprotected.is_some(),
            "Expected PositionUnprotected error when circuit breaker open, got: {:?}",
            messages
        );

        if let WsMessage::Error { details, .. } = unprotected.unwrap() {
            let details = details.as_ref().expect("details should be present");
            assert_eq!(
                details["circuit_breaker_open"], true,
                "Details should indicate circuit breaker is open"
            );
        }
    }

    #[tokio::test]
    async fn test_successful_exit_no_unprotected_notification() {
        let (router, _exit_handler, mut rx) = create_test_router_with_exit_handler();

        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        let filled_order = create_test_order("order-no-sl", OrderStatus::Filled, dec!(10));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let unprotected = messages.iter().find(|msg| {
            matches!(
                msg,
                WsMessage::Error {
                    code: WsErrorCode::PositionUnprotected,
                    ..
                }
            )
        });

        assert!(
            unprotected.is_none(),
            "Should not broadcast PositionUnprotected when no exit entries exist"
        );

        assert!(
            messages
                .iter()
                .any(|msg| matches!(msg, WsMessage::Order { .. })),
            "Order event should still be broadcast"
        );
    }

    // =========================================================================
    // Reconnect Reconciliation Tests
    // =========================================================================

    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    /// Mock adapter for reconciliation tests.
    struct ReconcileMockAdapter {
        orders: Mutex<HashMap<String, Order>>,
        positions: Mutex<Vec<Position>>,
        error_order_ids: HashSet<String>,
        fail_get_orders: bool,
        fail_get_positions: bool,
    }

    impl ReconcileMockAdapter {
        fn new(orders: Vec<Order>, positions: Vec<Position>) -> Self {
            let order_map = orders.into_iter().map(|o| (o.id.clone(), o)).collect();
            Self {
                orders: Mutex::new(order_map),
                positions: Mutex::new(positions),
                error_order_ids: HashSet::new(),
                fail_get_orders: false,
                fail_get_positions: false,
            }
        }

        fn new_with_errors(
            orders: Vec<Order>,
            positions: Vec<Position>,
            error_order_ids: HashSet<String>,
        ) -> Self {
            let order_map = orders.into_iter().map(|o| (o.id.clone(), o)).collect();
            Self {
                orders: Mutex::new(order_map),
                positions: Mutex::new(positions),
                error_order_ids,
                fail_get_orders: false,
                fail_get_positions: false,
            }
        }

        fn with_get_orders_failure(mut self, fail: bool) -> Self {
            self.fail_get_orders = fail;
            self
        }

        fn with_get_positions_failure(mut self, fail: bool) -> Self {
            self.fail_get_positions = fail;
            self
        }
    }

    struct ReconcileMockCapabilities;

    impl ProviderCapabilities for ReconcileMockCapabilities {
        fn supports_bracket_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn supports_oco_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn supports_oto_orders(&self, _symbol: &str) -> bool {
            false
        }
        fn bracket_strategy(&self, _order: &ModelOrderRequest) -> BracketStrategy {
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

    #[async_trait::async_trait]
    impl TradingAdapter for ReconcileMockAdapter {
        fn capabilities(&self) -> &dyn ProviderCapabilities {
            &ReconcileMockCapabilities
        }
        fn platform(&self) -> TradingPlatform {
            TradingPlatform::AlpacaLive
        }
        fn provider_name(&self) -> &'static str {
            "reconcile-mock"
        }
        async fn get_account(&self) -> crate::error::GatewayResult<crate::models::Account> {
            Err(GatewayError::internal("mock"))
        }
        async fn submit_order(
            &self,
            _request: &ModelOrderRequest,
        ) -> crate::error::GatewayResult<crate::models::OrderHandle> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_order(&self, order_id: &str) -> crate::error::GatewayResult<Order> {
            if self.error_order_ids.contains(order_id) {
                return Err(GatewayError::RateLimited {
                    retry_after_seconds: None,
                    reset_at: None,
                });
            }
            let orders = self.orders.lock().expect("lock poisoned");
            orders
                .get(order_id)
                .cloned()
                .ok_or(GatewayError::OrderNotFound {
                    id: order_id.to_string(),
                })
        }
        async fn get_orders(
            &self,
            _params: &OrderQueryParams,
        ) -> crate::error::GatewayResult<Vec<Order>> {
            if self.fail_get_orders {
                return Err(GatewayError::internal("mock get_orders failure"));
            }
            let orders = self.orders.lock().expect("lock poisoned");
            Ok(orders
                .values()
                .filter(|o| matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled))
                .cloned()
                .collect())
        }
        async fn get_order_history(
            &self,
            _params: &OrderQueryParams,
        ) -> crate::error::GatewayResult<Vec<Order>> {
            Ok(vec![])
        }
        async fn modify_order(
            &self,
            _order_id: &str,
            _request: &crate::models::ModifyOrderRequest,
        ) -> crate::error::GatewayResult<crate::models::ModifyOrderResult> {
            Err(GatewayError::internal("mock"))
        }
        async fn cancel_order(
            &self,
            _order_id: &str,
        ) -> crate::error::GatewayResult<crate::models::CancelOrderResult> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_trades(
            &self,
            _params: &crate::models::TradeQueryParams,
        ) -> crate::error::GatewayResult<Vec<crate::models::Trade>> {
            Ok(vec![])
        }
        async fn get_positions(
            &self,
            _symbol: Option<&str>,
        ) -> crate::error::GatewayResult<Vec<Position>> {
            if self.fail_get_positions {
                return Err(GatewayError::internal("mock get_positions failure"));
            }
            let positions = self.positions.lock().expect("lock poisoned");
            Ok(positions.clone())
        }
        async fn get_position(&self, _position_id: &str) -> crate::error::GatewayResult<Position> {
            Err(GatewayError::internal("mock"))
        }
        async fn close_position(
            &self,
            _position_id: &str,
            _request: &crate::models::ClosePositionRequest,
        ) -> crate::error::GatewayResult<crate::models::OrderHandle> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_quote(
            &self,
            _symbol: &str,
        ) -> crate::error::GatewayResult<crate::models::Quote> {
            Err(GatewayError::internal("mock"))
        }
        async fn get_bars(
            &self,
            _symbol: &str,
            _params: &crate::models::BarParams,
        ) -> crate::error::GatewayResult<Vec<crate::models::Bar>> {
            Ok(vec![])
        }
        async fn get_capabilities(&self) -> crate::error::GatewayResult<Capabilities> {
            Ok(Capabilities {
                supported_asset_classes: vec![],
                supported_order_types: vec![],
                position_mode: PositionMode::Netting,
                features: vec![],
                max_leverage: None,
                rate_limits: None,
            })
        }
        async fn get_connection_status(
            &self,
        ) -> crate::error::GatewayResult<crate::models::ConnectionStatus> {
            Err(GatewayError::internal("mock"))
        }
    }

    #[tokio::test]
    async fn test_reconcile_no_changes_during_disconnect() {
        let (router, mut rx) = create_test_router();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&open_order);

        let provider_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![provider_order], vec![]));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let order_events: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m, WsMessage::Order { .. }))
            .collect();
        assert!(
            order_events.is_empty(),
            "Expected no order events when nothing changed, got {:?}",
            order_events
        );

        let connected = messages.iter().any(|m| {
            matches!(
                m,
                WsMessage::Connection {
                    event: ConnectionEventType::Connected,
                    ..
                }
            )
        });
        assert!(connected, "Expected Connected event after reconciliation");
    }

    #[tokio::test]
    async fn test_reconcile_detects_fill_during_disconnect() {
        let (router, mut rx) = create_test_router();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&open_order);

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(100));
        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![filled_order], vec![]));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let fill_event = messages.iter().find(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    ..
                }
            )
        });
        assert!(
            fill_event.is_some(),
            "Expected OrderFilled event, got: {:?}",
            messages
        );
        if let Some(WsMessage::Order { order, event, .. }) = fill_event {
            assert_eq!(*event, OrderEventType::OrderFilled);
            assert_eq!(order.id, "order-1");
            assert_eq!(order.status, OrderStatus::Filled);
            assert_eq!(order.filled_quantity, dec!(100));
        }

        let fill_idx = messages.iter().position(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    ..
                }
            )
        });
        let connected_idx = messages.iter().position(|m| {
            matches!(
                m,
                WsMessage::Connection {
                    event: ConnectionEventType::Connected,
                    ..
                }
            )
        });
        assert!(
            fill_idx < connected_idx,
            "Fill event should precede Connected event"
        );
    }

    #[tokio::test]
    async fn test_reconcile_detects_cancellation_during_disconnect() {
        let (router, mut rx) = create_test_router();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&open_order);

        let cancelled_order = create_test_order("order-1", OrderStatus::Cancelled, Decimal::ZERO);
        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![cancelled_order], vec![]));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let cancel_event = messages.iter().find(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderCancelled,
                    ..
                }
            )
        });
        assert!(
            cancel_event.is_some(),
            "Expected OrderCancelled event, got: {:?}",
            messages
        );
        if let Some(WsMessage::Order { order, event, .. }) = cancel_event {
            assert_eq!(*event, OrderEventType::OrderCancelled);
            assert_eq!(order.id, "order-1");
            assert_eq!(order.status, OrderStatus::Cancelled);
        }
    }

    #[tokio::test]
    async fn test_reconcile_detects_partial_fill_progress() {
        let (router, mut rx) = create_test_router();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&open_order);

        let partial_order = create_test_order("order-1", OrderStatus::PartiallyFilled, dec!(50));
        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![partial_order], vec![]));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let partial_event = messages.iter().find(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderPartiallyFilled,
                    ..
                }
            )
        });
        assert!(
            partial_event.is_some(),
            "Expected OrderPartiallyFilled event, got: {:?}",
            messages
        );
        if let Some(WsMessage::Order { order, event, .. }) = partial_event {
            assert_eq!(*event, OrderEventType::OrderPartiallyFilled);
            assert_eq!(order.id, "order-1");
            assert_eq!(order.status, OrderStatus::PartiallyFilled);
            assert_eq!(order.filled_quantity, dec!(50));
        }
    }

    #[tokio::test]
    async fn test_reconcile_syncs_state_with_provider() {
        let (router, _rx) = create_test_router();

        let old_order = create_test_order("old-order", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&old_order);

        let new_order = create_test_order("new-order", OrderStatus::Open, Decimal::ZERO);
        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![new_order], vec![]));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        assert!(
            router.state_manager.get_order("new-order").is_some(),
            "New open order from provider should be in StateManager"
        );
        assert!(
            router.state_manager.get_order("old-order").is_none(),
            "Old order should be removed after state sync"
        );
    }

    #[tokio::test]
    async fn test_reconcile_handles_missing_order_at_provider() {
        let (router, mut rx) = create_test_router();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&open_order);

        let adapter: Arc<dyn TradingAdapter> = Arc::new(ReconcileMockAdapter::new(vec![], vec![]));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let connected = messages.iter().any(|m| {
            matches!(
                m,
                WsMessage::Connection {
                    event: ConnectionEventType::Connected,
                    ..
                }
            )
        });
        assert!(
            connected,
            "Connected event should still be broadcast after errors"
        );
    }

    #[tokio::test]
    async fn test_reconcile_multiple_orders_changed() {
        let (router, mut rx) = create_test_router();

        for id in &["order-1", "order-2", "order-3"] {
            let order = create_test_order(id, OrderStatus::Open, Decimal::ZERO);
            router.state_manager.upsert_order(&order);
        }

        let filled = create_test_order("order-1", OrderStatus::Filled, dec!(100));
        let cancelled = create_test_order("order-2", OrderStatus::Cancelled, Decimal::ZERO);
        let still_open = create_test_order("order-3", OrderStatus::Open, Decimal::ZERO);

        let adapter: Arc<dyn TradingAdapter> = Arc::new(ReconcileMockAdapter::new(
            vec![filled, cancelled, still_open],
            vec![],
        ));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let has_fill = messages.iter().any(|m| {
            matches!(m, WsMessage::Order {
                event: OrderEventType::OrderFilled,
                order,
                ..
            } if order.id == "order-1")
        });
        assert!(has_fill, "Expected fill for order-1");

        let has_cancel = messages.iter().any(|m| {
            matches!(m, WsMessage::Order {
                event: OrderEventType::OrderCancelled,
                order,
                ..
            } if order.id == "order-2")
        });
        assert!(has_cancel, "Expected cancel for order-2");

        let has_order3_event = messages.iter().any(|m| {
            matches!(m, WsMessage::Order {
                order,
                ..
            } if order.id == "order-3")
        });
        assert!(
            !has_order3_event,
            "Should not have events for unchanged order-3"
        );

        assert_eq!(
            router.order_events_processed(),
            2,
            "Should have processed 2 order events (fill + cancel)"
        );
    }

    // =========================================================================
    // Adapter Wiring Tests
    // =========================================================================

    #[tokio::test]
    async fn test_fill_without_adapter_set_does_not_panic() {
        let (router, _rx) = create_test_router();

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(100));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        assert!(
            router.state_manager().get_order("order-1").is_none(),
            "Filled orders are removed from state"
        );
        assert_eq!(router.order_events_processed(), 1);
    }

    #[tokio::test]
    async fn test_set_adapter_twice_returns_error() {
        let (router, _rx) = create_test_router();
        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);

        let first = router.set_adapter(Arc::clone(&adapter));
        assert!(first.is_ok(), "First set_adapter should succeed");

        let second = router.set_adapter(adapter);
        assert!(second.is_err(), "Second set_adapter should fail");
    }

    #[tokio::test]
    async fn test_reconcile_without_adapter_set_returns_early() {
        let (router, _rx) = create_test_router();

        let order = create_test_order("order-1", OrderStatus::Pending, dec!(0));
        router
            .handle_order_event(OrderEventType::OrderCreated, &order, None)
            .await;
        assert_eq!(router.order_events_processed(), 1);

        router.reconcile_after_reconnect().await;

        assert_eq!(
            router.order_events_processed(),
            1,
            "No additional events should be processed without adapter"
        );
    }

    #[tokio::test]
    async fn test_reconcile_provider_error_skips_order_and_continues() {
        let (router, mut rx) = create_test_router();

        let order1 = create_test_order("error-order", OrderStatus::Open, Decimal::ZERO);
        let order2 = create_test_order("fill-order", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&order1);
        router.state_manager.upsert_order(&order2);

        let filled_order = create_test_order("fill-order", OrderStatus::Filled, dec!(100));
        let adapter: Arc<dyn TradingAdapter> = Arc::new(ReconcileMockAdapter::new_with_errors(
            vec![filled_order],
            vec![],
            HashSet::from(["error-order".to_string()]),
        ));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let fill_event = messages.iter().find(|m| {
            matches!(m, WsMessage::Order {
                event: OrderEventType::OrderFilled,
                order,
                ..
            } if order.id == "fill-order")
        });
        assert!(
            fill_event.is_some(),
            "Expected OrderFilled for fill-order, got: {:?}",
            messages
        );

        let connected = messages.iter().any(|m| {
            matches!(
                m,
                WsMessage::Connection {
                    event: ConnectionEventType::Connected,
                    ..
                }
            )
        });
        assert!(
            connected,
            "Connected event should still be broadcast after provider errors"
        );

        assert_eq!(
            router.order_events_processed(),
            1,
            "Only the successful order query should generate an event"
        );
    }

    #[tokio::test]
    async fn test_reconcile_fill_routes_to_exit_handler_and_broadcasts_unprotected() {
        let state_manager = Arc::new(StateManager::new());
        let exit_config = ExitHandlerConfig {
            circuit_breaker_threshold: 1,
            retry_delays: vec![],
            ..ExitHandlerConfig::default()
        };
        let exit_handler = Arc::new(crate::exit_management::ExitHandler::new(
            Arc::clone(&state_manager),
            TradingPlatform::AlpacaLive,
            exit_config,
        ));
        let (broadcaster, mut rx) = broadcast::channel(16);

        let router = EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler) as Arc<dyn crate::exit_management::ExitHandling>,
            broadcaster,
            TradingPlatform::AlpacaLive,
        );

        let order_request = create_order_request_with_sl("BTCUSD", dec!(45000));
        exit_handler.register("order-1", &order_request).unwrap();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        state_manager.upsert_order(&open_order);

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(10));
        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![filled_order], vec![]));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let fill_event = messages.iter().find(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    ..
                }
            )
        });
        assert!(
            fill_event.is_some(),
            "Expected OrderFilled event during reconciliation, got: {:?}",
            messages
        );

        let unprotected = messages.iter().find(|m| {
            matches!(
                m,
                WsMessage::Error {
                    code: WsErrorCode::PositionUnprotected,
                    ..
                }
            )
        });
        assert!(
            unprotected.is_some(),
            "Expected PositionUnprotected when exit placement fails during reconciliation, got: {:?}",
            messages
        );

        if let WsMessage::Error { details, .. } = unprotected.unwrap() {
            let details = details.as_ref().expect("details should be present");
            assert_eq!(details["order_id"], "order-1");
            assert_eq!(details["symbol"], "BTCUSD");
            let failed_exits = details["failed_exits"]
                .as_array()
                .expect("failed_exits should be an array");
            assert!(
                !failed_exits.is_empty(),
                "failed_exits should contain at least one failed exit order"
            );
        }

        let unprotected_idx = messages.iter().position(|m| {
            matches!(
                m,
                WsMessage::Error {
                    code: WsErrorCode::PositionUnprotected,
                    ..
                }
            )
        });
        let fill_idx = messages.iter().position(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    ..
                }
            )
        });
        let connected_idx = messages.iter().position(|m| {
            matches!(
                m,
                WsMessage::Connection {
                    event: ConnectionEventType::Connected,
                    ..
                }
            )
        });
        assert!(
            unprotected_idx < fill_idx,
            "PositionUnprotected should precede OrderFilled (exit handler fires before fill broadcast)"
        );
        assert!(
            fill_idx < connected_idx,
            "OrderFilled should precede Connected"
        );

        assert_eq!(
            router.fills_routed(),
            1,
            "Fill should have been routed to exit handler during reconciliation"
        );
    }

    #[tokio::test]
    async fn test_reconcile_get_orders_failure_skips_connected_broadcast() {
        let (router, mut rx) = create_test_router();

        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![], vec![]).with_get_orders_failure(true));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        assert!(
            messages.is_empty(),
            "No events should be broadcast when get_orders() fails, got: {:?}",
            messages
        );
    }

    #[tokio::test]
    async fn test_reconcile_get_positions_failure_skips_connected_broadcast() {
        let (router, mut rx) = create_test_router();

        let adapter: Arc<dyn TradingAdapter> =
            Arc::new(ReconcileMockAdapter::new(vec![], vec![]).with_get_positions_failure(true));
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        assert!(
            messages.is_empty(),
            "No events should be broadcast when get_positions() fails, got: {:?}",
            messages
        );
    }

    #[tokio::test]
    async fn test_reconcile_step1_broadcasts_fill_but_step2_get_orders_failure_skips_connected() {
        let (router, mut rx) = create_test_router();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&open_order);

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(100));
        let adapter: Arc<dyn TradingAdapter> = Arc::new(
            ReconcileMockAdapter::new(vec![filled_order], vec![]).with_get_orders_failure(true),
        );
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let fill_event = messages.iter().find(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    ..
                }
            )
        });
        assert!(
            fill_event.is_some(),
            "Expected OrderFilled from Step 1, got: {:?}",
            messages
        );
        if let Some(WsMessage::Order { order, event, .. }) = fill_event {
            assert_eq!(*event, OrderEventType::OrderFilled);
            assert_eq!(order.id, "order-1");
            assert_eq!(order.status, OrderStatus::Filled);
            assert_eq!(order.filled_quantity, dec!(100));
        }

        assert_eq!(
            messages.len(),
            1,
            "Expected exactly 1 broadcast (OrderFilled), got: {:?}",
            messages
        );
    }

    #[tokio::test]
    async fn test_reconcile_step1_broadcasts_fill_but_step2_get_positions_failure_skips_connected()
    {
        let (router, mut rx) = create_test_router();

        let open_order = create_test_order("order-1", OrderStatus::Open, Decimal::ZERO);
        router.state_manager.upsert_order(&open_order);

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(100));
        let adapter: Arc<dyn TradingAdapter> = Arc::new(
            ReconcileMockAdapter::new(vec![filled_order], vec![]).with_get_positions_failure(true),
        );
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let fill_event = messages.iter().find(|m| {
            matches!(
                m,
                WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    ..
                }
            )
        });
        assert!(
            fill_event.is_some(),
            "Expected OrderFilled from Step 1, got: {:?}",
            messages
        );
        if let Some(WsMessage::Order { order, event, .. }) = fill_event {
            assert_eq!(*event, OrderEventType::OrderFilled);
            assert_eq!(order.id, "order-1");
            assert_eq!(order.status, OrderStatus::Filled);
            assert_eq!(order.filled_quantity, dec!(100));
        }

        assert_eq!(
            messages.len(),
            1,
            "Expected exactly 1 broadcast (OrderFilled), got: {:?}",
            messages
        );
    }

    // =========================================================================
    // Critical Notification Dropped Tests
    // =========================================================================

    #[tokio::test]
    async fn test_position_unprotected_with_no_receivers_increments_dropped_counter() {
        let (router, exit_handler, rx) = create_test_router_with_exit_handler();

        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        let order_request = create_order_request_with_sl("BTCUSD", dec!(45000));
        exit_handler.register("order-1", &order_request).unwrap();

        drop(rx);

        assert_eq!(router.critical_notifications_dropped(), 0);

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(10));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        assert_eq!(
            router.critical_notifications_dropped(),
            1,
            "Should increment dropped counter when no receivers for PositionUnprotected"
        );
    }

    #[tokio::test]
    async fn test_position_unprotected_with_receivers_does_not_increment_dropped_counter() {
        let (router, exit_handler, _rx) = create_test_router_with_exit_handler();

        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        let order_request = create_order_request_with_sl("BTCUSD", dec!(45000));
        exit_handler.register("order-1", &order_request).unwrap();

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(10));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        assert_eq!(
            router.critical_notifications_dropped(),
            0,
            "Should NOT increment dropped counter when receivers exist"
        );
    }

    // =========================================================================
    // Rebroadcast Unprotected Position Tests (Reconnect Recovery)
    // =========================================================================

    use crate::exit_management::ExitLegType;

    #[tokio::test]
    async fn test_rebroadcast_unprotected_positions_broadcasts_for_failed_entries() {
        let (router, mut rx) = create_test_router();

        let failed_entries = vec![FailedExitInfo {
            primary_order_id: "order-1".to_string(),
            symbol: "BTCUSD".to_string(),
            order_type: ExitLegType::StopLoss,
            error: "Connection refused".to_string(),
            failed_at: Utc::now(),
            actual_order_ids: vec![],
        }];

        router
            .rebroadcast_unprotected_positions(&failed_entries)
            .await;

        let msg = rx.try_recv().expect("Expected a broadcast message");
        match msg {
            WsMessage::Error {
                code,
                message,
                details,
                ..
            } => {
                assert_eq!(code, WsErrorCode::PositionUnprotected);
                assert!(
                    message.contains("unprotected"),
                    "Message should mention 'unprotected': {message}"
                );
                let details = details.expect("details should be present");
                assert_eq!(details["order_id"], "order-1");
                assert_eq!(details["symbol"], "BTCUSD");
            }
            _ => panic!("Expected Error message, got {:?}", msg),
        }
    }

    #[tokio::test]
    async fn test_rebroadcast_unprotected_positions_no_broadcast_when_empty() {
        let (router, mut rx) = create_test_router();

        router.rebroadcast_unprotected_positions(&[]).await;

        assert!(
            rx.try_recv().is_err(),
            "Should not broadcast when no failed entries"
        );
    }

    #[tokio::test]
    async fn test_rebroadcast_unprotected_positions_groups_by_primary_order() {
        let (router, mut rx) = create_test_router();

        let failed_entries = vec![
            FailedExitInfo {
                primary_order_id: "order-1".to_string(),
                symbol: "BTCUSD".to_string(),
                order_type: ExitLegType::StopLoss,
                error: "Connection refused".to_string(),
                failed_at: Utc::now(),
                actual_order_ids: vec![],
            },
            FailedExitInfo {
                primary_order_id: "order-1".to_string(),
                symbol: "BTCUSD".to_string(),
                order_type: ExitLegType::TakeProfit,
                error: "Connection refused".to_string(),
                failed_at: Utc::now(),
                actual_order_ids: vec![],
            },
        ];

        router
            .rebroadcast_unprotected_positions(&failed_entries)
            .await;

        let msg = rx.try_recv().expect("Expected one broadcast");
        assert!(
            rx.try_recv().is_err(),
            "Should have only one broadcast for same primary order"
        );

        if let WsMessage::Error { details, .. } = msg {
            let details = details.expect("details should be present");
            let failed_exits = details["failed_exits"]
                .as_array()
                .expect("failed_exits should be an array");
            assert_eq!(
                failed_exits.len(),
                2,
                "Should include both SL and TP failures"
            );
        }
    }

    #[tokio::test]
    async fn test_rebroadcast_unprotected_positions_includes_actual_order_ids() {
        let (router, mut rx) = create_test_router();

        let failed_entries = vec![FailedExitInfo {
            primary_order_id: "order-1".to_string(),
            symbol: "BTCUSD".to_string(),
            order_type: ExitLegType::StopLoss,
            error: "Placement timeout".to_string(),
            failed_at: Utc::now(),
            actual_order_ids: vec!["live-order-abc".to_string(), "live-order-def".to_string()],
        }];

        router
            .rebroadcast_unprotected_positions(&failed_entries)
            .await;

        let msg = rx.try_recv().expect("Expected a broadcast message");
        match msg {
            WsMessage::Error { details, .. } => {
                let details = details.expect("details should be present");
                let failed_exits = details["failed_exits"]
                    .as_array()
                    .expect("failed_exits should be an array");
                assert_eq!(failed_exits.len(), 1);

                let exit = &failed_exits[0];
                let order_ids = exit["actual_order_ids"]
                    .as_array()
                    .expect("actual_order_ids should be an array");
                assert_eq!(order_ids.len(), 2);
                assert_eq!(order_ids[0], "live-order-abc");
                assert_eq!(order_ids[1], "live-order-def");
            }
            _ => panic!("Expected Error message, got {:?}", msg),
        }
    }

    #[tokio::test]
    async fn test_reconcile_rebroadcasts_failed_exits_on_reconnect() {
        let state_manager = Arc::new(StateManager::new());
        let exit_config = ExitHandlerConfig {
            circuit_breaker_threshold: 100,
            retry_delays: vec![],
            ..ExitHandlerConfig::default()
        };
        let exit_handler = Arc::new(crate::exit_management::ExitHandler::new(
            Arc::clone(&state_manager),
            TradingPlatform::AlpacaLive,
            exit_config,
        ));
        let (broadcaster, mut rx) = broadcast::channel(32);

        let router = EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler) as Arc<dyn crate::exit_management::ExitHandling>,
            broadcaster,
            TradingPlatform::AlpacaLive,
        );

        let order_request = create_order_request_with_sl("BTCUSD", dec!(45000));
        exit_handler.register("order-1", &order_request).unwrap();

        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(10));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        while rx.try_recv().is_ok() {}

        router.reconcile_after_reconnect().await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let unprotected = messages.iter().find(|msg| {
            matches!(
                msg,
                WsMessage::Error {
                    code: WsErrorCode::PositionUnprotected,
                    ..
                }
            )
        });

        assert!(
            unprotected.is_some(),
            "Expected PositionUnprotected re-broadcast during reconciliation, got: {:?}",
            messages
        );

        if let Some(WsMessage::Error {
            message, details, ..
        }) = unprotected
        {
            assert!(
                message.contains("unprotected"),
                "Message should mention 'unprotected': {message}"
            );
            let details = details.as_ref().expect("details should be present");
            assert_eq!(details["order_id"], "order-1");
            assert_eq!(details["symbol"], "BTCUSD");
        }

        let connected = messages.iter().any(|m| {
            matches!(
                m,
                WsMessage::Connection {
                    event: ConnectionEventType::Connected,
                    ..
                }
            )
        });
        assert!(
            connected,
            "Connected event should still be broadcast after reconciliation"
        );

        let unprotected_idx = messages.iter().position(|m| {
            matches!(
                m,
                WsMessage::Error {
                    code: WsErrorCode::PositionUnprotected,
                    ..
                }
            )
        });
        let connected_idx = messages.iter().position(|m| {
            matches!(
                m,
                WsMessage::Connection {
                    event: ConnectionEventType::Connected,
                    ..
                }
            )
        });
        assert!(
            unprotected_idx < connected_idx,
            "PositionUnprotected should precede Connected during reconciliation"
        );
    }

    #[tokio::test]
    async fn test_reconcile_clears_failed_entries_after_rebroadcast() {
        let state_manager = Arc::new(StateManager::new());
        let exit_config = ExitHandlerConfig {
            circuit_breaker_threshold: 100,
            retry_delays: vec![],
            ..ExitHandlerConfig::default()
        };
        let exit_handler = Arc::new(crate::exit_management::ExitHandler::new(
            Arc::clone(&state_manager),
            TradingPlatform::AlpacaLive,
            exit_config,
        ));
        let (broadcaster, mut rx) = broadcast::channel(32);

        let router = EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler) as Arc<dyn crate::exit_management::ExitHandling>,
            broadcaster,
            TradingPlatform::AlpacaLive,
        );

        let order_request = create_order_request_with_sl("BTCUSD", dec!(45000));
        exit_handler.register("order-1", &order_request).unwrap();

        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        let filled_order = create_test_order("order-1", OrderStatus::Filled, dec!(10));
        router
            .handle_order_event(OrderEventType::OrderFilled, &filled_order, None)
            .await;

        while rx.try_recv().is_ok() {}

        assert!(
            !exit_handler.get_failed_entries().is_empty(),
            "Failed entry should exist before reconciliation"
        );

        router.reconcile_after_reconnect().await;

        while let Ok(_msg) = rx.try_recv() {}

        assert!(
            exit_handler.get_failed_entries().is_empty(),
            "Failed entries should be cleared after reconciliation rebroadcast"
        );
    }

    // =========================================================================
    // Initial broadcast_unprotected_position_if_needed tests
    // =========================================================================

    #[tokio::test]
    async fn test_broadcast_unprotected_includes_actual_order_ids_empty_for_failed() {
        let (router, mut rx) = create_test_router();
        let order = create_test_order("order-1", OrderStatus::Filled, dec!(100));

        let results = vec![PlacementResult::failed(
            "sl-placeholder-1".to_string(),
            crate::exit_management::ExitLegType::StopLoss,
            "Connection refused".to_string(),
            3,
        )];

        router
            .broadcast_unprotected_position_if_needed(&order, &results)
            .await;

        let msg = rx.try_recv().expect("Expected a broadcast message");
        match msg {
            WsMessage::Error { code, details, .. } => {
                assert_eq!(code, WsErrorCode::PositionUnprotected);
                let details = details.expect("details should be present");
                let failed_exits = details["failed_exits"]
                    .as_array()
                    .expect("failed_exits should be an array");
                assert_eq!(failed_exits.len(), 1);

                let exit = &failed_exits[0];
                let order_ids = exit["actual_order_ids"]
                    .as_array()
                    .expect("actual_order_ids should be an array");
                assert!(
                    order_ids.is_empty(),
                    "Initial broadcast should have empty actual_order_ids"
                );
            }
            _ => panic!("Expected Error message, got {:?}", msg),
        }
    }

    #[tokio::test]
    async fn test_broadcast_unprotected_includes_actual_order_ids_empty_for_deferred() {
        let (router, mut rx) = create_test_router();
        let order = create_test_order("order-2", OrderStatus::Filled, dec!(100));

        let results = vec![PlacementResult {
            placeholder_id: "tp-placeholder-1".to_string(),
            order_type: crate::exit_management::ExitLegType::TakeProfit,
            outcome: PlacementOutcome::Deferred {
                reason: "Circuit breaker open — exit placement suspended".to_string(),
            },
            sibling_cancellation: None,
        }];

        router
            .broadcast_unprotected_position_if_needed(&order, &results)
            .await;

        let msg = rx.try_recv().expect("Expected a broadcast message");
        match msg {
            WsMessage::Error { code, details, .. } => {
                assert_eq!(code, WsErrorCode::PositionUnprotected);
                let details = details.expect("details should be present");
                let failed_exits = details["failed_exits"]
                    .as_array()
                    .expect("failed_exits should be an array");
                assert_eq!(failed_exits.len(), 1);

                let exit = &failed_exits[0];
                let order_ids = exit["actual_order_ids"]
                    .as_array()
                    .expect("actual_order_ids should be an array");
                assert!(
                    order_ids.is_empty(),
                    "Initial broadcast should have empty actual_order_ids for deferred exit"
                );
            }
            _ => panic!("Expected Error message, got {:?}", msg),
        }
    }

    // =========================================================================
    // OCO Double-Exit Detection Tests
    // =========================================================================

    #[tokio::test]
    async fn test_oco_double_exit_detected_and_broadcast() {
        let (router, mut rx) = create_test_router();
        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        // Set up OCO group: SL and TP linked together
        let mut sl_order = create_test_order("sl-1", OrderStatus::Open, Decimal::ZERO);
        sl_order.oco_group_id = Some("oco-group-1".to_string());
        sl_order.side = Side::Sell;
        router.state_manager.upsert_order(&sl_order);
        router.state_manager.add_to_oco_group("sl-1", "oco-group-1");

        let mut tp_order = create_test_order("tp-1", OrderStatus::Open, Decimal::ZERO);
        tp_order.oco_group_id = Some("oco-group-1".to_string());
        tp_order.side = Side::Sell;
        router.state_manager.upsert_order(&tp_order);
        router.state_manager.add_to_oco_group("tp-1", "oco-group-1");

        // First fill: SL fills — should cancel TP (may fail, that's ok)
        let mut sl_filled = create_test_order("sl-1", OrderStatus::Filled, dec!(100));
        sl_filled.oco_group_id = Some("oco-group-1".to_string());
        sl_filled.side = Side::Sell;
        router
            .handle_order_event(OrderEventType::OrderFilled, &sl_filled, None)
            .await;

        // Drain messages from first fill
        while rx.try_recv().is_ok() {}

        // Second fill: TP also fills — should trigger double-exit detection
        let mut tp_filled = create_test_order("tp-1", OrderStatus::Filled, dec!(100));
        tp_filled.oco_group_id = Some("oco-group-1".to_string());
        tp_filled.side = Side::Sell;
        router
            .handle_order_event(OrderEventType::OrderFilled, &tp_filled, None)
            .await;

        // Collect all messages from second fill
        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let double_exit = messages.iter().find(|msg| {
            matches!(
                msg,
                WsMessage::Error {
                    code: WsErrorCode::OcoDoubleExit,
                    ..
                }
            )
        });

        assert!(
            double_exit.is_some(),
            "Expected OcoDoubleExit error broadcast, got messages: {messages:?}"
        );

        if let WsMessage::Error {
            code,
            message,
            details,
            ..
        } = double_exit.unwrap()
        {
            assert_eq!(*code, WsErrorCode::OcoDoubleExit);
            assert!(
                message.contains("double-exit"),
                "Message should mention 'double-exit': {message}"
            );
            let details = details.as_ref().expect("details should be present");
            assert_eq!(details["oco_group_id"], "oco-group-1");
            assert_eq!(details["symbol"], "BTCUSD");
            assert_eq!(details["first_fill"]["order_id"], "sl-1");
            assert_eq!(details["second_fill"]["order_id"], "tp-1");
        }
    }

    #[tokio::test]
    async fn test_oco_single_fill_no_double_exit_alert() {
        let (router, mut rx) = create_test_router();
        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        // Set up OCO group
        let mut sl_order = create_test_order("sl-1", OrderStatus::Open, Decimal::ZERO);
        sl_order.oco_group_id = Some("oco-group-1".to_string());
        router.state_manager.upsert_order(&sl_order);
        router.state_manager.add_to_oco_group("sl-1", "oco-group-1");

        let mut tp_order = create_test_order("tp-1", OrderStatus::Open, Decimal::ZERO);
        tp_order.oco_group_id = Some("oco-group-1".to_string());
        router.state_manager.upsert_order(&tp_order);
        router.state_manager.add_to_oco_group("tp-1", "oco-group-1");

        // Only one fill: SL fills
        let mut sl_filled = create_test_order("sl-1", OrderStatus::Filled, dec!(100));
        sl_filled.oco_group_id = Some("oco-group-1".to_string());
        router
            .handle_order_event(OrderEventType::OrderFilled, &sl_filled, None)
            .await;

        // Verify no OcoDoubleExit message
        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let double_exit = messages.iter().any(|msg| {
            matches!(
                msg,
                WsMessage::Error {
                    code: WsErrorCode::OcoDoubleExit,
                    ..
                }
            )
        });

        assert!(
            !double_exit,
            "Should NOT broadcast OcoDoubleExit for a single fill"
        );
    }

    #[tokio::test]
    async fn test_non_oco_fill_no_tracking() {
        let (router, mut rx) = create_test_router();
        let adapter: Arc<dyn TradingAdapter> = Arc::new(FailingAdapter);
        assert!(router.set_adapter(Arc::clone(&adapter)).is_ok());

        // Order without OCO group
        let order = create_test_order("order-1", OrderStatus::Filled, dec!(100));
        router
            .handle_order_event(OrderEventType::OrderFilled, &order, None)
            .await;

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        let double_exit = messages.iter().any(|msg| {
            matches!(
                msg,
                WsMessage::Error {
                    code: WsErrorCode::OcoDoubleExit,
                    ..
                }
            )
        });

        assert!(
            !double_exit,
            "Should NOT broadcast OcoDoubleExit for non-OCO orders"
        );
    }
}
