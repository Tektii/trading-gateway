//! Trailing stop handler for registration and lifecycle management.
//!
//! The `TrailingStopHandler` manages trailing stop entries that are tracked
//! by the gateway for providers without native trailing stop support.
//!
//! # Usage
//!
//! For automatic price tracking, use `new_arc()` to create an `Arc`-wrapped handler:
//!
//! ```ignore
//! let handler = TrailingStopHandler::new_arc(price_source, executor, platform, config);
//!
//! // Register a trailing stop - spawns tracking task automatically
//! let result = handler.register("order-123", &request)?;
//!
//! // Price updates are processed automatically via spawned tasks
//! ```
//!
//! # Architecture
//!
//! - **Registration**: Validates order, creates entry, spawns tracking task
//! - **Tracking**: Background task subscribes to price source, forwards updates
//! - **Cancellation**: Stops tracking task, marks entry as cancelled
//!
//! # State Machine
//!
//! ```text
//! Pending -> Tracking -> Active -> Triggered
//!              |           |
//!         Cancelled   Cancelled
//! ```

use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::GatewayError;
use crate::exit_management::circuit_breaker::ExitOrderCircuitBreaker;
use crate::models::{OrderRequest, OrderType, TradingPlatform, TrailingType};

use super::executor::StopOrderExecutor;
use super::price_source::PriceSource;
use super::types::{
    CancellationReason, RegisteredTrailingStop, TrailingEntry, TrailingEntryStatus,
    TrailingStopConfig,
};

/// Handler for trailing stop orders on a specific platform.
///
/// Each adapter that needs trailing stop emulation has its own handler,
/// isolating state per provider.
///
/// # Task Spawning
///
/// The handler can spawn background tasks to process price updates automatically.
/// This requires the handler to be wrapped in `Arc` using [`new_arc()`].
/// The handler stores a weak self-reference to avoid circular references.
///
/// # Order Execution
///
/// When an executor is set via [`set_executor()`], the handler will:
/// - Place stop orders when transitioning from `Tracking` to `Active`
/// - Modify stop orders when price improvements exceed the threshold
/// - Cancel stop orders when entries are cancelled
///
/// [`new_arc()`]: TrailingStopHandler::new_arc
/// [`set_executor()`]: TrailingStopHandler::set_executor
pub struct TrailingStopHandler {
    /// Active trailing stop entries by placeholder ID.
    entries_by_placeholder: DashMap<String, TrailingEntry>,
    /// Lookup from primary order ID to placeholder ID.
    entries_by_primary: DashMap<String, String>,
    /// Per-symbol tracking tasks: (CancellationToken, refcount).
    /// One task per symbol, shared by all entries on that symbol.
    symbol_tasks: DashMap<String, (CancellationToken, usize)>,
    /// Weak reference to self for spawning tasks.
    /// Set by `new_arc()` after construction.
    self_ref: RwLock<Option<Weak<Self>>>,
    /// Price source for real-time updates.
    price_source: Arc<dyn PriceSource>,
    /// Configuration.
    config: TrailingStopConfig,
    /// Platform this handler is for.
    platform: TradingPlatform,
    /// Stop order executor for placing/modifying orders at the provider.
    executor: Arc<dyn StopOrderExecutor>,
    /// Ensures quote data flows for symbols with active trailing stops.
    quote_subscriber: Arc<dyn super::price_source::QuoteSubscriber>,
    /// Circuit breaker for stop order operations.
    /// Trips after repeated failures to prevent accumulating unprotected positions.
    circuit_breaker: RwLock<ExitOrderCircuitBreaker>,
    /// Cancellation token for the recovery background task.
    recovery_token: CancellationToken,
}

impl std::fmt::Debug for TrailingStopHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrailingStopHandler")
            .field("entry_count", &self.entries_by_placeholder.len())
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}

impl TrailingStopHandler {
    /// Creates a new trailing stop handler.
    ///
    /// Note: For automatic price tracking via spawned tasks, use [`new_arc()`] instead.
    /// This constructor creates a handler without task-spawning capability.
    ///
    /// [`new_arc()`]: TrailingStopHandler::new_arc
    #[must_use]
    pub fn new(
        price_source: Arc<dyn PriceSource>,
        executor: Arc<dyn StopOrderExecutor>,
        platform: TradingPlatform,
        config: TrailingStopConfig,
    ) -> Self {
        Self::with_quote_subscriber(
            price_source,
            executor,
            platform,
            config,
            Arc::new(super::price_source::NoOpQuoteSubscriber),
        )
    }

    /// Creates a new trailing stop handler with a quote subscriber for
    /// auto-subscribing to provider quote streams.
    #[must_use]
    pub fn with_quote_subscriber(
        price_source: Arc<dyn PriceSource>,
        executor: Arc<dyn StopOrderExecutor>,
        platform: TradingPlatform,
        config: TrailingStopConfig,
        quote_subscriber: Arc<dyn super::price_source::QuoteSubscriber>,
    ) -> Self {
        // Circuit breaker: 3 failures in 5 minutes trips the breaker
        let circuit_breaker = ExitOrderCircuitBreaker::new(3, Duration::from_secs(300));

        Self {
            entries_by_placeholder: DashMap::new(),
            entries_by_primary: DashMap::new(),
            symbol_tasks: DashMap::new(),
            self_ref: RwLock::new(None),
            price_source,
            config,
            platform,
            executor,
            quote_subscriber,
            circuit_breaker: RwLock::new(circuit_breaker),
            recovery_token: CancellationToken::new(),
        }
    }

    /// Creates a handler with default configuration.
    ///
    /// Note: For automatic price tracking via spawned tasks, use [`new_arc()`] instead.
    ///
    /// [`new_arc()`]: TrailingStopHandler::new_arc
    #[must_use]
    pub fn with_defaults(
        price_source: Arc<dyn PriceSource>,
        executor: Arc<dyn StopOrderExecutor>,
        platform: TradingPlatform,
    ) -> Self {
        Self::new(
            price_source,
            executor,
            platform,
            TrailingStopConfig::default(),
        )
    }

    /// Creates an `Arc`-wrapped handler with task-spawning capability.
    ///
    /// This constructor enables automatic price tracking. When entries are registered
    /// and tracking is activated, the handler spawns background tasks that subscribe
    /// to the price source and process updates automatically.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handler = TrailingStopHandler::new_arc(price_source, executor, platform, config);
    ///
    /// // Register - spawns tracking task if price source available
    /// let result = handler.register("order-123", &request)?;
    ///
    /// // Price updates are processed automatically
    /// ```
    #[must_use]
    pub fn new_arc(
        price_source: Arc<dyn PriceSource>,
        executor: Arc<dyn StopOrderExecutor>,
        platform: TradingPlatform,
        config: TrailingStopConfig,
    ) -> Arc<Self> {
        Self::new_arc_with_quote_subscriber(
            price_source,
            executor,
            platform,
            config,
            Arc::new(super::price_source::NoOpQuoteSubscriber),
        )
    }

    /// Creates an `Arc`-wrapped handler with quote auto-subscription.
    #[must_use]
    pub fn new_arc_with_quote_subscriber(
        price_source: Arc<dyn PriceSource>,
        executor: Arc<dyn StopOrderExecutor>,
        platform: TradingPlatform,
        config: TrailingStopConfig,
        quote_subscriber: Arc<dyn super::price_source::QuoteSubscriber>,
    ) -> Arc<Self> {
        let handler = Arc::new(Self::with_quote_subscriber(
            price_source,
            executor,
            platform,
            config,
            quote_subscriber,
        ));

        // Store weak self-reference for spawning tasks
        if let Ok(mut self_ref) = handler.self_ref.write() {
            *self_ref = Some(Arc::downgrade(&handler));
        }

        handler
    }

    /// Returns the number of active entries.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries_by_placeholder.len()
    }

    /// Returns the platform this handler is for.
    #[must_use]
    pub const fn platform(&self) -> TradingPlatform {
        self.platform
    }

    /// Checks if an ID is a trailing stop placeholder.
    #[must_use]
    pub fn is_placeholder_id(id: &str) -> bool {
        id.starts_with("trailing:")
    }

    /// Gets an entry by placeholder ID.
    #[must_use]
    pub fn get_entry(&self, placeholder_id: &str) -> Option<TrailingEntry> {
        self.entries_by_placeholder
            .get(placeholder_id)
            .map(|e| e.value().clone())
    }

    /// Gets the status of an entry.
    #[must_use]
    pub fn get_status(&self, placeholder_id: &str) -> Option<TrailingEntryStatus> {
        self.entries_by_placeholder
            .get(placeholder_id)
            .map(|e| e.value().status.clone())
    }

    /// Checks if there's a trailing stop for a primary order.
    #[must_use]
    pub fn has_entry_for_primary(&self, primary_order_id: &str) -> bool {
        self.entries_by_primary.contains_key(primary_order_id)
    }

    /// Gets the placeholder ID for a primary order.
    #[must_use]
    pub fn get_placeholder_for_primary(&self, primary_order_id: &str) -> Option<String> {
        self.entries_by_primary
            .get(primary_order_id)
            .map(|e| e.value().clone())
    }

    // =========================================================================
    // Executor Configuration
    // =========================================================================

    /// Sets the stop order executor for order placement and modification.
    ///
    /// This is called after handler creation to inject the executor,
    /// solving the circular reference issue (adapter creates handler,
    /// handler needs adapter for execution).
    ///
    /// Checks if the circuit breaker is open (blocking new stop order operations).
    #[must_use]
    pub fn is_circuit_breaker_open(&self) -> bool {
        self.circuit_breaker
            .read()
            .map(|guard| guard.is_open())
            .unwrap_or(false)
    }

    /// Resets the circuit breaker after manual investigation.
    ///
    /// # Warning
    ///
    /// Only call this after verifying the underlying issue has been resolved.
    pub fn reset_circuit_breaker(&self) {
        if let Ok(mut guard) = self.circuit_breaker.write() {
            let _ = guard.reset();
        }
    }

    /// Records a stop order placement/modification failure.
    ///
    /// This may trip the circuit breaker if the failure threshold is reached.
    fn record_stop_order_failure(&self) {
        if let Ok(mut guard) = self.circuit_breaker.write() {
            guard.record_failure();
        }
    }

    // =========================================================================
    // Stop Order Lifecycle Methods
    // =========================================================================

    /// Places the initial stop order at the provider.
    ///
    /// Called when an entry is in `Tracking` state and we need to place the actual
    /// stop order. On success, transitions the entry to `Active` state.
    ///
    /// # Arguments
    ///
    /// * `placeholder_id` - The entry's placeholder ID
    /// * `symbol` - Trading symbol (for executor)
    /// * `side` - Order side
    /// * `quantity` - Order quantity
    /// * `stop_level` - Stop trigger price
    ///
    /// # Returns
    ///
    /// - `Ok(true)` if order was placed and entry transitioned to Active
    /// - `Ok(false)` if placement was skipped (circuit breaker open, no executor)
    /// - `Err` if placement failed and entry transitioned to Failed
    async fn place_initial_stop(
        &self,
        placeholder_id: &str,
        symbol: &str,
        side: crate::models::Side,
        quantity: Decimal,
        stop_level: Decimal,
    ) -> Result<bool, GatewayError> {
        // Check circuit breaker
        if self.is_circuit_breaker_open() {
            debug!(
                placeholder_id,
                "Skipping stop placement - circuit breaker open"
            );
            return Ok(false);
        }

        // Build request (limit_price unused for stop market orders, but required by struct)
        let request = super::executor::StopOrderRequest {
            symbol: symbol.to_string(),
            side,
            quantity,
            stop_price: stop_level,
            limit_price: stop_level,
        };

        // Place the order
        match self.executor.place_stop_order(&request).await {
            Ok(placed) => {
                // Transition to Active state
                if let Some(mut entry) = self.entries_by_placeholder.get_mut(placeholder_id) {
                    // Get peak price before mutating
                    let peak_price = entry.current_peak().unwrap_or(stop_level);

                    entry.set_status(TrailingEntryStatus::Active {
                        stop_order_id: placed.order_id.clone(),
                        peak_price,
                        stop_level: placed.stop_price,
                        last_adjusted_at: chrono::Utc::now(),
                    });

                    info!(
                        placeholder_id,
                        stop_order_id = %placed.order_id,
                        stop_price = %placed.stop_price,
                        "Initial stop order placed - entry now Active"
                    );
                }

                Ok(true)
            }
            Err(e) => {
                // Record failure for circuit breaker
                self.record_stop_order_failure();

                // Transition to Failed state
                if let Some(mut entry) = self.entries_by_placeholder.get_mut(placeholder_id) {
                    entry.set_status(TrailingEntryStatus::Failed {
                        error: e.to_string(),
                        retry_count: 0,
                        failed_at: chrono::Utc::now(),
                    });

                    error!(
                        placeholder_id,
                        error = %e,
                        "Failed to place initial stop order"
                    );
                }

                Ok(false) // Don't propagate error - we've handled it by entering Failed state
            }
        }
    }

    /// Adjusts an existing stop order at the provider.
    ///
    /// Called when price has improved beyond the threshold and we need to
    /// modify the stop order. Uses atomic cancel-replace.
    ///
    /// # Arguments
    ///
    /// * `placeholder_id` - The entry's placeholder ID
    /// * `stop_order_id` - The current stop order ID
    /// * `symbol` - Trading symbol
    /// * `new_stop_level` - The new stop price
    /// * `side` - Order side
    /// * `quantity` - Order quantity
    ///
    /// # Returns
    ///
    /// - `Ok(Some(new_order_id))` if modification succeeded
    /// - `Ok(None)` if modification was skipped or failed (old order kept)
    async fn adjust_stop_order(
        &self,
        placeholder_id: &str,
        stop_order_id: &str,
        symbol: &str,
        new_stop_level: Decimal,
        side: crate::models::Side,
        quantity: Decimal,
    ) -> Result<Option<String>, GatewayError> {
        // Check circuit breaker
        if self.is_circuit_breaker_open() {
            debug!(
                placeholder_id,
                "Skipping stop adjustment - circuit breaker open"
            );
            return Ok(None);
        }

        // Build request (limit_price unused for stop market orders, but required by struct)
        let request = super::executor::ModifyStopRequest {
            order_id: stop_order_id.to_string(),
            symbol: symbol.to_string(),
            side,
            quantity,
            new_stop_price: new_stop_level,
            new_limit_price: new_stop_level,
        };

        // Modify the order (cancel + replace)
        match self.executor.modify_stop_order(&request).await {
            Ok(placed) => {
                // Update entry with new order details
                if let Some(mut entry) = self.entries_by_placeholder.get_mut(placeholder_id)
                    && let TrailingEntryStatus::Active { peak_price, .. } = entry.status
                {
                    entry.set_status(TrailingEntryStatus::Active {
                        stop_order_id: placed.order_id.clone(),
                        peak_price,
                        stop_level: placed.stop_price,
                        last_adjusted_at: chrono::Utc::now(),
                    });

                    info!(
                        placeholder_id,
                        old_order_id = stop_order_id,
                        new_order_id = %placed.order_id,
                        new_stop_price = %placed.stop_price,
                        "Stop order adjusted"
                    );
                }

                Ok(Some(placed.order_id))
            }
            Err(e) => {
                // Record failure for circuit breaker
                self.record_stop_order_failure();

                warn!(
                    placeholder_id,
                    error = %e,
                    "Failed to adjust stop order - verifying actual order state"
                );

                // Verify actual order state at provider to detect silent successes
                match self
                    .executor
                    .get_stop_order_status(stop_order_id, symbol)
                    .await
                {
                    Ok(status) if status.stop_price == new_stop_level => {
                        // Modify secretly succeeded - update entry to match reality
                        if let Some(mut entry) = self.entries_by_placeholder.get_mut(placeholder_id)
                            && let TrailingEntryStatus::Active { peak_price, .. } = entry.status
                        {
                            entry.set_status(TrailingEntryStatus::Active {
                                stop_order_id: status.order_id.clone(),
                                peak_price,
                                stop_level: status.stop_price,
                                last_adjusted_at: chrono::Utc::now(),
                            });
                        }

                        info!(
                            placeholder_id,
                            verified_stop_price = %status.stop_price,
                            "Modify appeared to fail but verification confirms success"
                        );

                        Ok(Some(status.order_id))
                    }
                    Ok(status) => {
                        // Truly failed - stop price still at old value
                        warn!(
                            placeholder_id,
                            actual_stop_price = %status.stop_price,
                            expected_stop_price = %new_stop_level,
                            "Modify confirmed failed - keeping existing order"
                        );
                        Err(e)
                    }
                    Err(verify_err) => {
                        // Can't verify - be conservative, assume failure
                        warn!(
                            placeholder_id,
                            verify_error = %verify_err,
                            "Failed to verify stop order status - assuming modify failed"
                        );
                        Err(e)
                    }
                }
            }
        }
    }

    /// Registers a trailing stop order.
    ///
    /// This is called when a `TrailingStop` order is submitted to a provider
    /// without native trailing stop support.
    ///
    /// The entry is stored in `Pending` state. Tracking activates when the
    /// primary order fills via [`on_primary_filled`](Self::on_primary_filled).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The order is not a trailing stop
    /// - Required fields are missing
    /// - Capacity limit reached
    pub fn register(
        &self,
        primary_order_id: &str,
        order: &OrderRequest,
    ) -> Result<RegisteredTrailingStop, GatewayError> {
        // Validate order type
        if order.order_type != OrderType::TrailingStop {
            return Err(GatewayError::InvalidRequest {
                message: format!("Expected TrailingStop order, got {:?}", order.order_type),
                field: None,
            });
        }

        // Validate required fields
        let trailing_distance =
            order
                .trailing_distance
                .ok_or_else(|| GatewayError::InvalidRequest {
                    message: "trailing_distance is required for trailing stop orders".to_string(),
                    field: None,
                })?;

        let trailing_type = order.trailing_type.unwrap_or(TrailingType::Percent);

        // Validate trailing distance is positive
        if trailing_distance <= Decimal::ZERO {
            return Err(GatewayError::InvalidRequest {
                message: format!("trailing_distance must be positive, got {trailing_distance}"),
                field: None,
            });
        }

        // Validate percentage trailing distance is less than 100%
        if trailing_type == TrailingType::Percent && trailing_distance >= dec!(100) {
            return Err(GatewayError::InvalidRequest {
                message: format!(
                    "trailing_distance percentage must be less than 100, got {trailing_distance}"
                ),
                field: None,
            });
        }

        // Check capacity
        if self.entries_by_placeholder.len() >= self.config.max_pending_entries {
            return Err(GatewayError::Internal {
                message: format!(
                    "Resource exhausted: trailing_stop_entries (limit: {})",
                    self.config.max_pending_entries
                ),
                source: None,
            });
        }

        // Create entry
        let entry = TrailingEntry::new(
            primary_order_id,
            order.symbol.clone(),
            order.side,
            order.quantity,
            trailing_distance,
            trailing_type,
            self.platform,
        );

        let placeholder_id = entry.placeholder_id.clone();

        // Entry stays in Pending state until primary order fills.
        // Activation happens in on_primary_filled() to avoid placing
        // stop orders before the position exists.

        // Store entry
        self.entries_by_placeholder
            .insert(placeholder_id.clone(), entry);
        self.entries_by_primary
            .insert(primary_order_id.to_string(), placeholder_id.clone());

        info!(
            placeholder_id = %placeholder_id,
            primary_order_id,
            symbol = %order.symbol,
            trailing_distance = %trailing_distance,
            trailing_type = ?trailing_type,
            platform = ?self.platform,
            "Trailing stop registered"
        );

        Ok(RegisteredTrailingStop { placeholder_id })
    }

    /// Attempts to activate price tracking for an entry.
    ///
    /// # Behavior
    ///
    /// 1. Checks cached price first (fast path if quotes already flowing)
    /// 2. If no cache, subscribes and waits for first price update (5s timeout)
    /// 3. Calculates initial stop level based on trailing distance/type
    /// 4. Updates entry status to `Tracking` with peak and stop level
    /// 5. Spawns tracking task for ongoing price updates
    ///
    /// # Errors
    ///
    /// - `UnsupportedOperation` if price source doesn't support the operation
    /// - `SymbolNotFound` if price isn't available within timeout
    async fn try_activate_tracking(&self, entry: &mut TrailingEntry) -> Result<(), GatewayError> {
        // Ensure the provider is streaming quotes for this symbol
        if let Err(e) = self.quote_subscriber.ensure_subscribed(&entry.symbol).await {
            warn!(
                symbol = %entry.symbol,
                error = %e,
                "Failed to auto-subscribe to quotes (continuing with existing subscriptions)"
            );
        }

        // Fast path: check cached price first (zero cost if quotes already flowing)
        let (current_price, rx) =
            if let Ok(price) = self.price_source.current_price(&entry.symbol).await {
                // Price cached — subscribe for ongoing updates
                let rx = self.price_source.subscribe(&entry.symbol).await?;
                (price, rx)
            } else {
                // No cached price — subscribe and wait for first update
                let mut rx = self.price_source.subscribe(&entry.symbol).await?;
                if let Ok(Some(update)) =
                    tokio::time::timeout(self.config.activation_timeout, rx.recv()).await
                {
                    (update.price, rx)
                } else {
                    let _ = self.price_source.unsubscribe(&entry.symbol).await;
                    return Err(GatewayError::SymbolNotFound {
                        symbol: entry.symbol.clone(),
                    });
                }
            };

        // Calculate initial stop level
        let stop_level = entry.calculate_stop(current_price);

        // Transition to Tracking state
        entry.set_status(TrailingEntryStatus::Tracking {
            peak_price: current_price,
            stop_level,
            started_at: chrono::Utc::now(),
        });

        debug!(
            placeholder_id = %entry.placeholder_id,
            symbol = %entry.symbol,
            peak_price = %current_price,
            stop_level = %stop_level,
            "Trailing stop tracking initialized"
        );

        // Spawn tracking task if we have a self-reference (created via new_arc)
        self.spawn_tracking_task(&entry.placeholder_id, &entry.symbol, rx);

        Ok(())
    }

    /// Handles recoverable activation failures by transitioning to `PriceSourceUnavailable`.
    ///
    /// Propagates unrecognised errors to the caller.
    fn handle_activation_failure(
        entry: &mut TrailingEntry,
        error: GatewayError,
        placeholder_id: &str,
        primary_order_id: &str,
        symbol: &str,
    ) -> Result<(), GatewayError> {
        match error {
            GatewayError::UnsupportedOperation { .. } => {
                entry.set_status(TrailingEntryStatus::PriceSourceUnavailable {
                    registered_at: entry.created_at_utc,
                    reason: "Real-time price tracking not yet implemented".to_string(),
                    recovery_failure_count: 0,
                    last_recovery_attempt: None,
                });
                warn!(
                    placeholder_id = %placeholder_id,
                    primary_order_id,
                    symbol = %symbol,
                    "Trailing stop registered but price tracking unavailable (unsupported)"
                );
                Ok(())
            }
            GatewayError::SymbolNotFound { symbol: sym } => {
                entry.set_status(TrailingEntryStatus::PriceSourceUnavailable {
                    registered_at: entry.created_at_utc,
                    reason: format!("Price not available for {sym}"),
                    recovery_failure_count: 0,
                    last_recovery_attempt: None,
                });
                warn!(
                    placeholder_id = %placeholder_id,
                    primary_order_id,
                    symbol = %symbol,
                    "Trailing stop registered but price not available for symbol"
                );
                Ok(())
            }
            GatewayError::ProviderUnavailable { message } => {
                entry.set_status(TrailingEntryStatus::PriceSourceUnavailable {
                    registered_at: entry.created_at_utc,
                    reason: format!("Price source unavailable: {message}"),
                    recovery_failure_count: 0,
                    last_recovery_attempt: None,
                });
                warn!(
                    placeholder_id = %placeholder_id,
                    primary_order_id,
                    symbol = %symbol,
                    reason = %message,
                    "Trailing stop registered but price source unavailable"
                );
                Ok(())
            }
            other => Err(other),
        }
    }

    /// Spawns a background task to process price updates for an entry.
    ///
    /// The task receives updates via the provided receiver and forwards them to `on_price_update()`.
    /// It runs until cancelled via the entry's cancellation token.
    ///
    /// # Arguments
    ///
    /// * `placeholder_id` - The entry's placeholder ID
    /// * `symbol` - The trading symbol
    /// * `rx` - Pre-subscribed receiver for price updates (avoids race conditions)
    ///
    /// # Requirements
    ///
    /// This method only spawns a task if the handler was created via [`new_arc()`],
    /// which sets up the weak self-reference needed for the task to call back.
    ///
    /// [`new_arc()`]: TrailingStopHandler::new_arc
    fn spawn_tracking_task(
        &self,
        placeholder_id: &str,
        symbol: &str,
        mut rx: tokio::sync::mpsc::Receiver<super::price_source::PriceUpdate>,
    ) {
        // Get the Arc<Self> from our weak reference
        let handler = {
            let Ok(self_ref) = self.self_ref.read() else {
                warn!(
                    placeholder_id,
                    "Cannot spawn tracking task: self_ref lock poisoned"
                );
                return;
            };

            let Some(arc) = self_ref.as_ref().and_then(std::sync::Weak::upgrade) else {
                debug!(
                    placeholder_id,
                    "Cannot spawn tracking task: no self reference (use new_arc())"
                );
                return;
            };
            arc
        };

        // Per-symbol task management: if a task already exists for this symbol,
        // just increment refcount. Otherwise, spawn a new task.
        if let Some(mut entry) = self.symbol_tasks.get_mut(symbol) {
            entry.1 += 1;
            debug!(
                placeholder_id,
                symbol,
                refcount = entry.1,
                "Reusing existing tracking task for symbol"
            );
            // Drop the receiver — the existing task already has its own subscription
            drop(rx);
            return;
        }

        let cancel_token = CancellationToken::new();
        self.symbol_tasks
            .insert(symbol.to_string(), (cancel_token.clone(), 1));

        info!(placeholder_id, symbol, "Spawning tracking task for symbol");

        let symbol = symbol.to_string();
        let price_source = Arc::clone(&self.price_source);

        tokio::spawn(async move {
            debug!(
                symbol = %symbol,
                "Tracking task started"
            );

            loop {
                tokio::select! {
                    () = cancel_token.cancelled() => {
                        debug!(
                            symbol = %symbol,
                            "Tracking task cancelled"
                        );
                        break;
                    }
                    update = rx.recv() => {
                        if let Some(price_update) = update {
                            if let Err(e) = handler.on_price_update(&price_update.symbol, price_update.price).await {
                                warn!(
                                    symbol = %symbol,
                                    error = %e,
                                    "Error processing price update"
                                );
                            }
                        } else {
                            // Channel closed — price source disconnected.
                            // Transition active entries to PriceSourceUnavailable so the
                            // recovery task can re-subscribe when the connection is restored.
                            warn!(
                                symbol = %symbol,
                                "Price source channel closed — transitioning entries to recoverable state"
                            );
                            handler.on_price_source_disconnected(&symbol);
                            break;
                        }
                    }
                }
            }

            if let Err(e) = price_source.unsubscribe(&symbol).await {
                debug!(
                    symbol = %symbol,
                    error = %e,
                    "Error unsubscribing from price source (may already be unsubscribed)"
                );
            }
        });
    }

    /// Cancels a trailing stop entry.
    ///
    /// This also stops the tracking task (if any) for this entry.
    ///
    /// # Returns
    ///
    /// - `Ok(true)` if the entry was found and cancelled
    /// - `Ok(false)` if the entry was not found
    /// - `Err` if cancellation failed
    pub fn cancel(
        &self,
        placeholder_id: &str,
        reason: CancellationReason,
    ) -> Result<bool, GatewayError> {
        let entry_info = match self.entries_by_placeholder.get_mut(placeholder_id) {
            Some(mut e) => {
                // Check if already terminal
                if e.status.is_terminal() {
                    return Ok(false);
                }

                let info = (e.primary_order_id.clone(), e.symbol.clone());

                e.set_status(TrailingEntryStatus::Cancelled {
                    cancelled_at: chrono::Utc::now(),
                    reason,
                });

                info!(
                    placeholder_id,
                    reason = %reason,
                    "Trailing stop cancelled"
                );

                Some(info)
            }
            None => None,
        };

        let Some((primary_id, symbol)) = entry_info else {
            return Ok(false);
        };

        self.entries_by_primary.remove(&primary_id);
        self.decrement_symbol_task(&symbol);

        Ok(true)
    }

    /// Async version of cancel that also cancels the stop order at the provider.
    ///
    /// For entries in `Active` state (with a stop order placed), this will:
    /// 1. Call `executor.cancel_stop_order()` to cancel at the provider
    /// 2. Transition the entry to `Cancelled` state
    ///
    /// For entries in other states (e.g., `Tracking`), this just transitions
    /// to `Cancelled` without any provider call.
    ///
    /// # Returns
    ///
    /// - `Ok(true)` if the entry was found and cancelled
    /// - `Ok(false)` if the entry was not found or already terminal
    /// - `Err` if cancellation at provider failed
    pub async fn cancel_async(
        &self,
        placeholder_id: &str,
        reason: CancellationReason,
    ) -> Result<bool, GatewayError> {
        // First, extract the info we need for provider cancellation
        let cancel_info = {
            let Some(entry) = self.entries_by_placeholder.get(placeholder_id) else {
                return Ok(false);
            };

            // Check if already terminal
            if entry.status.is_terminal() {
                return Ok(false);
            }

            // If Active, we need to cancel at provider
            if let TrailingEntryStatus::Active {
                ref stop_order_id, ..
            } = entry.status
            {
                Some((stop_order_id.clone(), entry.symbol.clone()))
            } else {
                None
            }
        }; // Lock released

        // If we have a stop order to cancel, do it now
        if let Some((stop_order_id, symbol)) = cancel_info {
            match self
                .executor
                .cancel_stop_order(&stop_order_id, &symbol)
                .await
            {
                Ok(()) => {
                    debug!(
                        placeholder_id,
                        stop_order_id = %stop_order_id,
                        "Cancelled stop order at provider"
                    );
                }
                Err(e) => {
                    // Log the error but still proceed with local cancellation
                    // The order may have already been filled/cancelled
                    warn!(
                        placeholder_id,
                        stop_order_id = %stop_order_id,
                        error = %e,
                        "Failed to cancel stop order at provider, proceeding with local cancellation"
                    );
                }
            }
        }

        // Now perform the local cancellation (same as sync cancel)
        self.cancel(placeholder_id, reason)
    }

    /// Called when the primary order is cancelled.
    ///
    /// Cancels any associated trailing stop entry.
    pub fn on_primary_cancelled(&self, primary_order_id: &str) -> Result<bool, GatewayError> {
        self.get_placeholder_for_primary(primary_order_id)
            .map_or_else(
                || Ok(false),
                |placeholder_id| self.cancel(&placeholder_id, CancellationReason::PrimaryCancelled),
            )
    }

    /// Called when the primary order is rejected.
    ///
    /// Cancels any associated trailing stop entry.
    pub fn on_primary_rejected(&self, primary_order_id: &str) -> Result<bool, GatewayError> {
        self.get_placeholder_for_primary(primary_order_id)
            .map_or_else(
                || Ok(false),
                |placeholder_id| self.cancel(&placeholder_id, CancellationReason::PrimaryRejected),
            )
    }

    /// Called when the primary order fills.
    ///
    /// If the trailing stop entry is still pending (waiting for price source),
    /// this attempts to activate tracking. The fill price is used as the initial
    /// peak price if the price source doesn't yet have a cached price.
    pub async fn on_primary_filled(
        &self,
        primary_order_id: &str,
        fill_price: Decimal,
    ) -> Result<bool, GatewayError> {
        let Some(placeholder_id) = self.get_placeholder_for_primary(primary_order_id) else {
            return Ok(false); // No trailing stop registered for this order
        };

        // Check if entry needs activation
        let needs_activation = self
            .entries_by_placeholder
            .get(&placeholder_id)
            .is_some_and(|entry| entry.status.is_pending());

        if !needs_activation {
            debug!(
                placeholder_id,
                primary_order_id,
                "Trailing stop already active or terminal, skipping fill activation"
            );
            return Ok(false);
        }

        // Clone entry to avoid holding DashMap guard across .await
        let mut entry = match self.entries_by_placeholder.get(&placeholder_id) {
            Some(guard) => guard.clone(),
            None => return Ok(false),
        };
        // Guard dropped here ^

        // Try to activate tracking (async — no DashMap lock held)
        match self.try_activate_tracking(&mut entry).await {
            Ok(()) => {
                info!(
                    placeholder_id,
                    primary_order_id,
                    fill_price = %fill_price,
                    "Trailing stop tracking activated on primary fill"
                );
                // Reinsert the updated entry
                self.entries_by_placeholder.insert(placeholder_id, entry);
                Ok(true)
            }
            Err(error) => {
                // Handle activation failure — may transition to PriceSourceUnavailable
                let symbol = entry.symbol.clone();
                Self::handle_activation_failure(
                    &mut entry,
                    error,
                    &placeholder_id,
                    primary_order_id,
                    &symbol,
                )?;
                // Reinsert the updated entry
                self.entries_by_placeholder.insert(placeholder_id, entry);
                Ok(false)
            }
        }
    }

    /// Called with a price update from the price source.
    ///
    /// Processes price updates for all active trailing stop entries on the given symbol.
    /// When price improves (higher for SELL, lower for BUY), updates the peak price
    /// and recalculates the stop level.
    ///
    /// # Arguments
    ///
    /// * `symbol` - The trading symbol (e.g., "C:BTCUSD")
    /// * `price` - The current market price
    ///
    /// # Behavior
    ///
    /// For each entry on the symbol:
    /// 1. **Tracking state**: Place initial stop order, transition to Active
    /// 2. **Active state**: If price improves beyond threshold, modify stop order
    ///
    /// Entries in terminal or pending states are skipped.
    pub async fn on_price_update(&self, symbol: &str, price: Decimal) -> Result<(), GatewayError> {
        // Find all entries for this symbol that are actively tracking
        let entries_to_update: Vec<String> = self
            .entries_by_placeholder
            .iter()
            .filter(|entry| entry.symbol == symbol && entry.status.is_active())
            .map(|entry| entry.placeholder_id.clone())
            .collect();

        if entries_to_update.is_empty() {
            return Ok(());
        }

        debug!(
            symbol,
            price = %price,
            entry_count = entries_to_update.len(),
            "Processing price update for trailing stops"
        );

        for placeholder_id in entries_to_update {
            // Collect entry data while holding the lock briefly
            let action = {
                let Some(entry) = self.entries_by_placeholder.get(&placeholder_id) else {
                    continue;
                };

                // Skip if not active
                if !entry.status.is_active() {
                    continue;
                }

                // Determine what action to take based on current state
                match &entry.status {
                    TrailingEntryStatus::Tracking {
                        peak_price: current_peak,
                        stop_level: current_stop,
                        ..
                    } => {
                        // In Tracking state: place the initial stop order
                        // If price improved, use new stop level; otherwise use current
                        let (new_peak, new_stop) =
                            if entry.is_peak_improvement(*current_peak, price) {
                                (price, entry.calculate_stop(price))
                            } else {
                                (*current_peak, *current_stop)
                            };

                        Some(PriceUpdateAction::PlaceStop {
                            symbol: entry.symbol.clone(),
                            side: entry.side,
                            quantity: entry.quantity,
                            stop_level: new_stop,
                            new_peak,
                        })
                    }
                    TrailingEntryStatus::Active {
                        stop_order_id,
                        peak_price: current_peak,
                        stop_level: old_stop_level,
                        ..
                    } => {
                        // In Active state: check for price improvement
                        if !entry.is_peak_improvement(*current_peak, price) {
                            // No improvement, nothing to do
                            continue;
                        }

                        // Calculate new stop level with updated peak
                        let new_stop_level = entry.calculate_stop(price);

                        // Check if adjustment is significant enough
                        let should_adjust =
                            super::types::should_adjust_stop(*old_stop_level, new_stop_level);

                        if should_adjust {
                            Some(PriceUpdateAction::AdjustStop {
                                stop_order_id: stop_order_id.clone(),
                                symbol: entry.symbol.clone(),
                                side: entry.side,
                                quantity: entry.quantity,
                                new_stop_level,
                                new_peak: price,
                            })
                        } else {
                            // Just update state, no order modification needed
                            Some(PriceUpdateAction::UpdateStateOnly {
                                new_peak: price,
                                new_stop_level,
                            })
                        }
                    }
                    _ => None,
                }
            }; // Lock released here

            // Now perform the action (async operations, no lock held)
            if let Some(action) = action {
                self.execute_price_update_action(&placeholder_id, action)
                    .await;
            }
        }

        Ok(())
    }

    /// Handles the `PlaceStop` action: places initial stop and updates state if skipped.
    /// Handles disconnection of the price source for a symbol.
    ///
    /// Transitions all `Tracking` or `Active` entries for the symbol to
    /// `PriceSourceUnavailable` so the recovery task can re-subscribe.
    /// Any existing stop orders at the broker remain in place (static protection).
    fn on_price_source_disconnected(&self, symbol: &str) {
        let entries: Vec<String> = self
            .entries_by_placeholder
            .iter()
            .filter(|e| e.symbol == symbol && e.status.is_active())
            .map(|e| e.placeholder_id.clone())
            .collect();

        for placeholder_id in entries {
            if let Some(mut entry) = self.entries_by_placeholder.get_mut(&placeholder_id) {
                let registered_at = entry.created_at_utc;
                let reason = match &entry.status {
                    TrailingEntryStatus::Active { stop_order_id, .. } => {
                        format!(
                            "Price source disconnected (stop order {stop_order_id} still active at broker)"
                        )
                    }
                    _ => "Price source disconnected".to_string(),
                };

                entry.set_status(TrailingEntryStatus::PriceSourceUnavailable {
                    registered_at,
                    reason,
                    recovery_failure_count: 0,
                    last_recovery_attempt: None,
                });

                warn!(
                    placeholder_id = %entry.placeholder_id,
                    symbol,
                    "Trailing stop tracking degraded — stop is static until price source recovers"
                );
            }
        }

        // Clean up the symbol task entry
        self.symbol_tasks.remove(symbol);
    }

    async fn handle_place_stop_action(
        &self,
        placeholder_id: &str,
        symbol: &str,
        side: crate::models::Side,
        quantity: Decimal,
        stop_level: Decimal,
        new_peak: Decimal,
    ) {
        let placed = self
            .place_initial_stop(placeholder_id, symbol, side, quantity, stop_level)
            .await
            .unwrap_or(false);

        if !placed && let Some(mut entry) = self.entries_by_placeholder.get_mut(placeholder_id) {
            let new_stop_level = entry.calculate_stop(new_peak);
            let new_status = if let TrailingEntryStatus::Tracking { started_at, .. } = &entry.status
            {
                Some(TrailingEntryStatus::Tracking {
                    peak_price: new_peak,
                    stop_level: new_stop_level,
                    started_at: *started_at,
                })
            } else {
                None
            };

            if let Some(status) = new_status {
                entry.set_status(status);
                debug!(
                    placeholder_id = %entry.placeholder_id,
                    new_peak = %new_peak,
                    new_stop = %new_stop_level,
                    "Updated Tracking state (placement skipped)"
                );
            }
        }
    }

    /// Executes a single price update action (place, adjust, or state-only update).
    async fn execute_price_update_action(&self, placeholder_id: &str, action: PriceUpdateAction) {
        match action {
            PriceUpdateAction::PlaceStop {
                symbol,
                side,
                quantity,
                stop_level,
                new_peak,
            } => {
                self.handle_place_stop_action(
                    placeholder_id,
                    &symbol,
                    side,
                    quantity,
                    stop_level,
                    new_peak,
                )
                .await;
            }
            PriceUpdateAction::AdjustStop {
                stop_order_id,
                symbol,
                side,
                quantity,
                new_stop_level,
                new_peak,
            } => {
                let result = self
                    .adjust_stop_order(
                        placeholder_id,
                        &stop_order_id,
                        &symbol,
                        new_stop_level,
                        side,
                        quantity,
                    )
                    .await;

                if result.is_ok()
                    && result.as_ref().unwrap().is_none()
                    && let Some(mut entry) = self.entries_by_placeholder.get_mut(placeholder_id)
                {
                    let new_status = if let TrailingEntryStatus::Active {
                        ref stop_order_id,
                        last_adjusted_at,
                        ..
                    } = entry.status
                    {
                        Some(TrailingEntryStatus::Active {
                            stop_order_id: stop_order_id.clone(),
                            peak_price: new_peak,
                            stop_level: new_stop_level,
                            last_adjusted_at,
                        })
                    } else {
                        None
                    };

                    if let Some(status) = new_status {
                        entry.set_status(status);
                    }
                }
            }
            PriceUpdateAction::UpdateStateOnly {
                new_peak,
                new_stop_level,
            } => {
                if let Some(mut entry) = self.entries_by_placeholder.get_mut(placeholder_id) {
                    let new_status = match &entry.status {
                        TrailingEntryStatus::Tracking { started_at, .. } => {
                            Some(TrailingEntryStatus::Tracking {
                                peak_price: new_peak,
                                stop_level: new_stop_level,
                                started_at: *started_at,
                            })
                        }
                        TrailingEntryStatus::Active {
                            stop_order_id,
                            last_adjusted_at,
                            ..
                        } => Some(TrailingEntryStatus::Active {
                            stop_order_id: stop_order_id.clone(),
                            peak_price: new_peak,
                            stop_level: new_stop_level,
                            last_adjusted_at: *last_adjusted_at,
                        }),
                        _ => None,
                    };

                    if let Some(status) = new_status {
                        entry.set_status(status);
                        debug!(
                            placeholder_id = %entry.placeholder_id,
                            new_peak = %new_peak,
                            new_stop = %new_stop_level,
                            "Updated trailing stop state (below threshold)"
                        );
                    }
                }
            }
        }
    }

    // =========================================================================
    // Recovery: PriceSourceUnavailable -> Tracking
    // =========================================================================

    /// Calculates the backoff duration for a given failure count.
    ///
    /// Formula: `recovery_interval * 2^(failure_count - 1)`, capped at
    /// `max_recovery_backoff`. Returns `Duration::ZERO` if `failure_count` is 0.
    fn calculate_recovery_backoff(&self, failure_count: u32) -> Duration {
        if failure_count == 0 {
            return Duration::ZERO;
        }
        // Cap exponent to prevent overflow (2^20 = 1M, already way past max_backoff)
        let exponent = failure_count.saturating_sub(1).min(20);
        let multiplier = 2u32.saturating_pow(exponent);
        let backoff = self
            .config
            .recovery_interval
            .checked_mul(multiplier)
            .unwrap_or(self.config.max_recovery_backoff);
        backoff.min(self.config.max_recovery_backoff)
    }

    /// Scans for entries stuck in `PriceSourceUnavailable` and attempts recovery.
    ///
    /// For each unavailable entry:
    /// - If the entry has exceeded its TTL, transitions to `Failed`
    /// - Otherwise, retries `try_activate_tracking()`
    ///   - Success: entry moves to `Tracking` (price source now available)
    ///   - Failure: entry stays in `PriceSourceUnavailable`
    ///
    /// Returns statistics about the recovery attempt.
    pub async fn retry_unavailable_entries(&self) -> RecoveryStats {
        // Collect placeholder IDs of all PriceSourceUnavailable entries
        let unavailable_ids: Vec<String> = self
            .entries_by_placeholder
            .iter()
            .filter(|e| matches!(e.status, TrailingEntryStatus::PriceSourceUnavailable { .. }))
            .map(|e| e.placeholder_id.clone())
            .collect();

        let mut stats = RecoveryStats {
            attempted: unavailable_ids.len(),
            ..RecoveryStats::default()
        };

        if unavailable_ids.is_empty() {
            return stats;
        }

        for placeholder_id in &unavailable_ids {
            self.try_recover_entry(placeholder_id, &mut stats).await;
        }

        if stats.recovered > 0 || stats.expired > 0 || stats.skipped > 0 {
            info!(
                attempted = stats.attempted,
                recovered = stats.recovered,
                expired = stats.expired,
                still_unavailable = stats.still_unavailable,
                skipped = stats.skipped,
                "Recovery scan complete"
            );
        }

        stats
    }

    /// Attempts to recover a single `PriceSourceUnavailable` entry.
    ///
    /// Handles TTL expiry, exponential backoff, and activation retry.
    async fn try_recover_entry(&self, placeholder_id: &str, stats: &mut RecoveryStats) {
        let entry = match self.entries_by_placeholder.get(placeholder_id) {
            Some(e) => e.clone(),
            None => return,
        };

        // Check TTL
        if entry.created_at.elapsed() > self.config.entry_ttl {
            if let Some(mut e) = self.entries_by_placeholder.get_mut(placeholder_id) {
                e.set_status(TrailingEntryStatus::Failed {
                    error: "Entry TTL expired while waiting for price source".to_string(),
                    retry_count: 0,
                    failed_at: chrono::Utc::now(),
                });
            }
            stats.expired += 1;
            debug!(placeholder_id, "Trailing stop entry expired (TTL exceeded)");
            return;
        }

        // Exponential backoff
        if let TrailingEntryStatus::PriceSourceUnavailable {
            recovery_failure_count,
            last_recovery_attempt,
            ..
        } = &entry.status
            && *recovery_failure_count > 0
            && let Some(last_attempt) = last_recovery_attempt
        {
            let backoff = self.calculate_recovery_backoff(*recovery_failure_count);
            if last_attempt.elapsed() < backoff {
                debug!(
                    placeholder_id,
                    failure_count = recovery_failure_count,
                    backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
                    "Skipping entry (exponential backoff)"
                );
                stats.skipped += 1;
                return;
            }
        }

        // Try to activate tracking
        let mut entry_clone = entry;
        match self.try_activate_tracking(&mut entry_clone).await {
            Ok(()) => {
                if let Some(mut e) = self.entries_by_placeholder.get_mut(placeholder_id) {
                    if matches!(e.status, TrailingEntryStatus::PriceSourceUnavailable { .. }) {
                        e.set_status(entry_clone.status);
                        stats.recovered += 1;
                        info!(
                            placeholder_id,
                            "Trailing stop recovered from PriceSourceUnavailable"
                        );
                    } else {
                        debug!(
                            placeholder_id,
                            current_status = ?e.status,
                            "Entry state changed during recovery, skipping update"
                        );
                    }
                }
            }
            Err(e) => {
                if let Some(mut entry_ref) = self.entries_by_placeholder.get_mut(placeholder_id)
                    && let TrailingEntryStatus::PriceSourceUnavailable {
                        ref mut recovery_failure_count,
                        ref mut last_recovery_attempt,
                        ..
                    } = entry_ref.status
                {
                    *recovery_failure_count = recovery_failure_count.saturating_add(1);
                    *last_recovery_attempt = Some(Instant::now());
                }
                debug!(
                    placeholder_id,
                    error = %e,
                    "Recovery attempt failed, entry still unavailable"
                );
                stats.still_unavailable += 1;
            }
        }
    }

    /// Starts a background task that periodically retries unavailable entries.
    ///
    /// The task runs until cancelled via [`stop_recovery_task()`] or
    /// [`stop_all_tasks()`]. Uses `recovery_interval` from config.
    ///
    /// # Requirements
    ///
    /// Handler must be created via [`new_arc()`] for the task to have a
    /// valid self-reference.
    ///
    /// [`new_arc()`]: TrailingStopHandler::new_arc
    /// [`stop_recovery_task()`]: TrailingStopHandler::stop_recovery_task
    /// [`stop_all_tasks()`]: TrailingStopHandler::stop_all_tasks
    pub fn start_recovery_task(&self) {
        let handler = {
            let Ok(self_ref) = self.self_ref.read() else {
                warn!("Cannot start recovery task: self_ref lock poisoned");
                return;
            };

            let Some(arc) = self_ref.as_ref().and_then(std::sync::Weak::upgrade) else {
                debug!("Cannot start recovery task: no self reference (use new_arc())");
                return;
            };
            arc
        };

        let interval = self.config.recovery_interval;
        let cancel_token = self.recovery_token.clone();

        info!(
            interval_secs = interval.as_secs(),
            "Starting trailing stop recovery task"
        );

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel_token.cancelled() => {
                        info!("Recovery task cancelled, shutting down");
                        break;
                    }
                    () = tokio::time::sleep(interval) => {
                        handler.retry_unavailable_entries().await;
                    }
                }
            }
        });
    }

    /// Stops the recovery background task.
    ///
    /// Cancels the token used by [`start_recovery_task()`], causing
    /// the background loop to exit on its next iteration.
    ///
    /// [`start_recovery_task()`]: TrailingStopHandler::start_recovery_task
    pub fn stop_recovery_task(&self) {
        self.recovery_token.cancel();
        info!("Recovery task cancellation requested");
    }

    /// Decrements the refcount for a symbol's tracking task.
    /// Cancels the task when refcount reaches zero.
    fn decrement_symbol_task(&self, symbol: &str) {
        if let Some(mut entry) = self.symbol_tasks.get_mut(symbol) {
            entry.1 = entry.1.saturating_sub(1);
            if entry.1 == 0 {
                entry.0.cancel();
                drop(entry);
                self.symbol_tasks.remove(symbol);
                debug!(
                    symbol,
                    "Symbol tracking task cancelled (last entry removed)"
                );
            } else {
                debug!(
                    symbol,
                    refcount = entry.1,
                    "Symbol task refcount decremented"
                );
            }
        }
    }

    /// Stops all background tasks (recovery + per-symbol tracking).
    ///
    /// Cancels the recovery task and all symbol tracking tasks.
    /// Call this during graceful shutdown.
    pub fn stop_all_tasks(&self) {
        self.recovery_token.cancel();

        for entry in &self.symbol_tasks {
            entry.value().0.cancel();
        }

        info!(
            symbol_tasks = self.symbol_tasks.len(),
            "All background tasks cancellation requested"
        );
    }
}

/// Statistics from a recovery scan of `PriceSourceUnavailable` entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryStats {
    /// Number of `PriceSourceUnavailable` entries found.
    pub attempted: usize,
    /// Number successfully transitioned to `Tracking`.
    pub recovered: usize,
    /// Number expired past TTL (moved to `Failed`).
    pub expired: usize,
    /// Number still unavailable after retry attempt.
    pub still_unavailable: usize,
    /// Number skipped due to exponential backoff (not yet time to retry).
    pub skipped: usize,
}

/// Actions to take during price update processing.
///
/// Used to collect intent while holding the entry lock briefly,
/// then release the lock before performing async operations.
enum PriceUpdateAction {
    /// Place the initial stop order (Tracking -> Active)
    PlaceStop {
        symbol: String,
        side: crate::models::Side,
        quantity: Decimal,
        stop_level: Decimal,
        new_peak: Decimal,
    },
    /// Adjust an existing stop order (Active state)
    AdjustStop {
        stop_order_id: String,
        symbol: String,
        side: crate::models::Side,
        quantity: Decimal,
        new_stop_level: Decimal,
        new_peak: Decimal,
    },
    /// Just update internal state (improvement below threshold)
    UpdateStateOnly {
        new_peak: Decimal,
        new_stop_level: Decimal,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OrderType, Side, TimeInForce, TrailingType};
    use crate::trailing_stop::executor::mock::MockStopOrderExecutor;
    use crate::trailing_stop::price_source::UnimplementedPriceSource;
    use rust_decimal_macros::dec;

    fn create_test_handler() -> TrailingStopHandler {
        let price_source =
            Arc::new(UnimplementedPriceSource::new("binance")) as Arc<dyn PriceSource>;
        let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
        TrailingStopHandler::with_defaults(price_source, executor, TradingPlatform::BinanceSpotLive)
    }

    fn create_trailing_stop_request(
        symbol: &str,
        side: Side,
        quantity: Decimal,
        trailing_distance: Decimal,
        trailing_type: TrailingType,
    ) -> OrderRequest {
        OrderRequest {
            symbol: symbol.to_string(),
            side,
            quantity,
            order_type: OrderType::TrailingStop,
            time_in_force: TimeInForce::Gtc,
            limit_price: None,
            stop_price: Some(dec!(95)), // Initial stop price
            trailing_distance: Some(trailing_distance),
            trailing_type: Some(trailing_type),
            stop_loss: None,
            take_profit: None,
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

    // =========================================================================
    // Registration Tests
    // =========================================================================

    #[tokio::test]
    async fn register_creates_entry_with_placeholder() {
        let handler = create_test_handler();
        let request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );

        let result = handler.register("order-123", &request).unwrap();

        assert_eq!(result.placeholder_id, "trailing:order-123");
        assert_eq!(handler.entry_count(), 1);
    }

    #[tokio::test]
    async fn register_enters_pending_state() {
        let handler = create_test_handler();
        let request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );

        let result = handler.register("order-123", &request).unwrap();

        let entry = handler.get_entry(&result.placeholder_id).unwrap();
        assert!(matches!(entry.status, TrailingEntryStatus::Pending));
    }

    #[tokio::test]
    async fn register_defaults_to_percent_trailing_type() {
        let handler = create_test_handler();
        let mut request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );
        request.trailing_type = None; // Remove explicit type

        let result = handler.register("order-123", &request).unwrap();

        let entry = handler.get_entry(&result.placeholder_id).unwrap();
        assert_eq!(entry.trailing_type, TrailingType::Percent);
    }

    // =========================================================================
    // Validation Tests
    // =========================================================================

    #[tokio::test]
    async fn register_rejects_non_trailing_stop_order() {
        let handler = create_test_handler();
        let mut request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );
        request.order_type = OrderType::Market;

        let result = handler.register("order-123", &request);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GatewayError::InvalidRequest { .. }
        ));
    }

    #[tokio::test]
    async fn register_rejects_missing_trailing_distance() {
        let handler = create_test_handler();
        let mut request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );
        request.trailing_distance = None;

        let result = handler.register("order-123", &request);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GatewayError::InvalidRequest { .. }
        ));
    }

    #[tokio::test]
    async fn register_rejects_zero_trailing_distance() {
        let handler = create_test_handler();
        let mut request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );
        request.trailing_distance = Some(Decimal::ZERO);

        let result = handler.register("order-123", &request);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GatewayError::InvalidRequest { .. }
        ));
    }

    #[tokio::test]
    async fn register_rejects_negative_trailing_distance() {
        let handler = create_test_handler();
        let mut request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );
        request.trailing_distance = Some(dec!(-5));

        let result = handler.register("order-123", &request);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GatewayError::InvalidRequest { .. }
        ));
    }

    // =========================================================================
    // Cancellation Tests
    // =========================================================================

    #[tokio::test]
    async fn cancel_removes_entry() {
        let handler = create_test_handler();
        let request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );

        let result = handler.register("order-123", &request).unwrap();

        let cancelled = handler
            .cancel(&result.placeholder_id, CancellationReason::UserRequested)
            .unwrap();

        assert!(cancelled);
        let entry = handler.get_entry(&result.placeholder_id).unwrap();
        assert!(matches!(
            entry.status,
            TrailingEntryStatus::Cancelled { .. }
        ));
    }

    #[tokio::test]
    async fn cancel_returns_false_for_unknown_id() {
        let handler = create_test_handler();

        let cancelled = handler
            .cancel("trailing:unknown", CancellationReason::UserRequested)
            .unwrap();

        assert!(!cancelled);
    }

    #[tokio::test]
    async fn on_primary_cancelled_cancels_entry() {
        let handler = create_test_handler();
        let request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );

        let result = handler.register("order-123", &request).unwrap();

        handler.on_primary_cancelled("order-123").unwrap();

        let entry = handler.get_entry(&result.placeholder_id).unwrap();
        match entry.status {
            TrailingEntryStatus::Cancelled { reason, .. } => {
                assert_eq!(reason, CancellationReason::PrimaryCancelled);
            }
            _ => panic!("Expected Cancelled status"),
        }
    }

    #[tokio::test]
    async fn on_primary_rejected_cancels_entry() {
        let handler = create_test_handler();
        let request = create_trailing_stop_request(
            "C:BTCUSD",
            Side::Sell,
            dec!(1),
            dec!(5),
            TrailingType::Percent,
        );

        let result = handler.register("order-123", &request).unwrap();

        handler.on_primary_rejected("order-123").unwrap();

        let entry = handler.get_entry(&result.placeholder_id).unwrap();
        match entry.status {
            TrailingEntryStatus::Cancelled { reason, .. } => {
                assert_eq!(reason, CancellationReason::PrimaryRejected);
            }
            _ => panic!("Expected Cancelled status"),
        }
    }

    // =========================================================================
    // Phase 2: Tracking Activation Tests (with working price source)
    // =========================================================================

    mod tracking_activation {
        use super::*;
        use crate::trailing_stop::price_source::MockPriceSource;

        fn create_handler_with_mock_price_source() -> (TrailingStopHandler, Arc<MockPriceSource>) {
            let price_source = Arc::new(MockPriceSource::new());
            let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
            let config = TrailingStopConfig {
                activation_timeout: std::time::Duration::from_millis(10),
                ..TrailingStopConfig::default()
            };
            let handler = TrailingStopHandler::new(
                Arc::clone(&price_source) as Arc<dyn PriceSource>,
                executor,
                TradingPlatform::BinanceSpotLive,
                config,
            );
            (handler, price_source)
        }

        #[tokio::test]
        async fn register_stays_pending_until_primary_fills() {
            // Arrange
            let (handler, price_source) = create_handler_with_mock_price_source();
            price_source.set_price("C:BTCUSD", dec!(50000));

            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );

            // Act - register only, do NOT call on_primary_filled
            let result = handler.register("order-123", &request).unwrap();

            // Assert - should be Pending, not Tracking
            let entry = handler.get_entry(&result.placeholder_id).unwrap();
            assert!(
                matches!(entry.status, TrailingEntryStatus::Pending),
                "Expected Pending state before primary fill, got {:?}",
                entry.status
            );
        }

        #[tokio::test]
        async fn on_primary_filled_activates_tracking() {
            // Arrange
            let (handler, price_source) = create_handler_with_mock_price_source();
            price_source.set_price("C:BTCUSD", dec!(50000));

            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5), // 5% trailing distance
                TrailingType::Percent,
            );

            let result = handler.register("order-123", &request).unwrap();

            // Act - simulate primary fill
            let activated = handler
                .on_primary_filled("order-123", dec!(50000))
                .await
                .unwrap();

            // Assert - should be in Tracking state
            assert!(activated, "on_primary_filled should return true");
            let entry = handler.get_entry(&result.placeholder_id).unwrap();
            assert!(
                matches!(entry.status, TrailingEntryStatus::Tracking { .. }),
                "Expected Tracking state after fill, got {:?}",
                entry.status
            );
        }

        #[tokio::test]
        async fn on_primary_filled_sets_peak_price() {
            // Arrange
            let (handler, price_source) = create_handler_with_mock_price_source();
            price_source.set_price("C:BTCUSD", dec!(50000));

            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );

            handler.register("order-123", &request).unwrap();

            // Act
            handler
                .on_primary_filled("order-123", dec!(50000))
                .await
                .unwrap();

            // Assert - peak price should be set to current price
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match entry.status {
                TrailingEntryStatus::Tracking { peak_price, .. } => {
                    assert_eq!(
                        peak_price,
                        dec!(50000),
                        "Peak price should match current price"
                    );
                }
                _ => panic!("Expected Tracking state, got {:?}", entry.status),
            }
        }

        #[tokio::test]
        async fn on_primary_filled_calculates_stop_level() {
            // Arrange
            let (handler, price_source) = create_handler_with_mock_price_source();
            price_source.set_price("C:BTCUSD", dec!(50000));

            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5), // 5% trailing distance
                TrailingType::Percent,
            );

            handler.register("order-123", &request).unwrap();

            // Act
            handler
                .on_primary_filled("order-123", dec!(50000))
                .await
                .unwrap();

            // Assert - stop level should be 5% below peak (50000 * 0.95 = 47500)
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match entry.status {
                TrailingEntryStatus::Tracking { stop_level, .. } => {
                    assert_eq!(
                        stop_level,
                        dec!(47500),
                        "Stop level should be 5% below peak"
                    );
                }
                _ => panic!("Expected Tracking state, got {:?}", entry.status),
            }
        }

        #[tokio::test]
        async fn on_primary_filled_with_absolute_trailing() {
            // Arrange
            let (handler, price_source) = create_handler_with_mock_price_source();
            price_source.set_price("C:BTCUSD", dec!(50000));

            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(1000), // $1000 absolute trailing distance
                TrailingType::Absolute,
            );

            handler.register("order-123", &request).unwrap();

            // Act
            handler
                .on_primary_filled("order-123", dec!(50000))
                .await
                .unwrap();

            // Assert - stop level should be $1000 below peak (50000 - 1000 = 49000)
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match entry.status {
                TrailingEntryStatus::Tracking { stop_level, .. } => {
                    assert_eq!(
                        stop_level,
                        dec!(49000),
                        "Stop level should be $1000 below peak"
                    );
                }
                _ => panic!("Expected Tracking state, got {:?}", entry.status),
            }
        }

        #[tokio::test]
        async fn on_primary_filled_buy_trailing_stop_calculates_stop_above_peak() {
            // Arrange - BUY trailing stop (for short position protection)
            let (handler, price_source) = create_handler_with_mock_price_source();
            price_source.set_price("C:BTCUSD", dec!(50000));

            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Buy,
                dec!(1),
                dec!(5), // 5% trailing distance
                TrailingType::Percent,
            );

            handler.register("order-123", &request).unwrap();

            // Act
            handler
                .on_primary_filled("order-123", dec!(50000))
                .await
                .unwrap();

            // Assert - stop level should be 5% above peak (50000 * 1.05 = 52500)
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match entry.status {
                TrailingEntryStatus::Tracking {
                    stop_level,
                    peak_price,
                    ..
                } => {
                    assert_eq!(
                        peak_price,
                        dec!(50000),
                        "Peak price should match current price"
                    );
                    assert_eq!(
                        stop_level,
                        dec!(52500),
                        "Stop level should be 5% above peak for BUY"
                    );
                }
                _ => panic!("Expected Tracking state, got {:?}", entry.status),
            }
        }

        #[tokio::test]
        async fn on_primary_filled_without_price_enters_unavailable() {
            // Arrange - price source has no price for symbol
            let (handler, _price_source) = create_handler_with_mock_price_source();
            // Note: NOT setting price for C:BTCUSD

            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );

            handler.register("order-123", &request).unwrap();

            // Act - primary fills but no price available
            handler
                .on_primary_filled("order-123", dec!(50000))
                .await
                .unwrap();

            // Assert - should fall back to PriceSourceUnavailable
            let entry = handler.get_entry("trailing:order-123").unwrap();
            assert!(
                matches!(
                    entry.status,
                    TrailingEntryStatus::PriceSourceUnavailable { .. }
                ),
                "Expected PriceSourceUnavailable state when no price available, got {:?}",
                entry.status
            );
        }
    }

    // =========================================================================
    // Modify Failure Verification Tests
    // =========================================================================

    mod modify_failure_verification {
        use super::*;
        use crate::trailing_stop::executor::StopOrderStatus;
        use crate::trailing_stop::executor::mock::MockStopOrderExecutor;
        use crate::trailing_stop::price_source::MockPriceSource;

        /// Sets up a handler with an entry in Active state (stop order placed).
        ///
        /// Returns (handler, price_source, executor) with entry "trailing:order-123"
        /// Active at peak=50000, stop=47500, order_id="mock-1".
        async fn setup_active_entry() -> (
            TrailingStopHandler,
            Arc<MockPriceSource>,
            Arc<MockStopOrderExecutor>,
        ) {
            let price_source = Arc::new(MockPriceSource::new());
            let executor = Arc::new(MockStopOrderExecutor::new());
            let handler = TrailingStopHandler::with_defaults(
                Arc::clone(&price_source) as Arc<dyn PriceSource>,
                Arc::clone(&executor) as Arc<dyn StopOrderExecutor>,
                TradingPlatform::BinanceSpotLive,
            );

            // Set initial price
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Register entry - stays Pending
            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5), // 5% trailing
                TrailingType::Percent,
            );
            handler.register("order-123", &request).unwrap();

            // Simulate primary fill - activates tracking (Pending -> Tracking)
            handler
                .on_primary_filled("order-123", dec!(50000))
                .await
                .unwrap();

            // Price update triggers PlaceStop (Tracking -> Active)
            handler
                .on_price_update("C:BTCUSD", dec!(50000))
                .await
                .unwrap();

            // Verify Active state with expected values
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match &entry.status {
                TrailingEntryStatus::Active {
                    stop_order_id,
                    peak_price,
                    stop_level,
                    ..
                } => {
                    assert_eq!(stop_order_id, "mock-1");
                    assert_eq!(*peak_price, dec!(50000));
                    assert_eq!(*stop_level, dec!(47500));
                }
                other => panic!("Expected Active state, got {:?}", other),
            }

            // Reset call counters so test assertions only count
            // modify/verify attempts, not the initial place_stop_order
            executor.reset();

            (handler, price_source, executor)
        }

        #[tokio::test]
        async fn modify_failure_triggers_order_status_verification() {
            let (handler, _price_source, executor) = setup_active_entry().await;

            // Configure modify to fail
            executor.set_modify_error(GatewayError::Internal {
                message: "timeout".to_string(),
                source: None,
            });

            // Configure verification to show old stop price (truly failed)
            executor.set_get_status_result(Ok(StopOrderStatus {
                order_id: "mock-1".to_string(),
                stop_price: dec!(47500),
                is_active: true,
            }));

            // Price goes up 50000 -> 55000 (triggers adjustment)
            handler
                .on_price_update("C:BTCUSD", dec!(55000))
                .await
                .unwrap();

            // Verify get_stop_order_status was called after modify failure
            assert_eq!(
                executor.get_status_call_count(),
                1,
                "Should verify order status after modify failure"
            );
        }

        #[tokio::test]
        async fn modify_failure_verified_truly_failed_preserves_old_state() {
            let (handler, _price_source, executor) = setup_active_entry().await;

            // Configure modify to fail
            executor.set_modify_error(GatewayError::Internal {
                message: "timeout".to_string(),
                source: None,
            });

            // Verification shows old stop price -> truly failed
            executor.set_get_status_result(Ok(StopOrderStatus {
                order_id: "mock-1".to_string(),
                stop_price: dec!(47500),
                is_active: true,
            }));

            // Price goes up 50000 -> 55000
            handler
                .on_price_update("C:BTCUSD", dec!(55000))
                .await
                .unwrap();

            // Entry should still have original values (not the desired 52250/55000)
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match entry.status {
                TrailingEntryStatus::Active {
                    stop_level,
                    peak_price,
                    stop_order_id,
                    ..
                } => {
                    assert_eq!(
                        stop_level,
                        dec!(47500),
                        "Stop level should remain at original value"
                    );
                    assert_eq!(
                        peak_price,
                        dec!(50000),
                        "Peak price should remain at original value"
                    );
                    assert_eq!(stop_order_id, "mock-1", "Order ID should remain unchanged");
                }
                other => panic!("Expected Active state, got {:?}", other),
            }
        }

        #[tokio::test]
        async fn modify_failure_verified_secretly_succeeded_updates_state() {
            let (handler, _price_source, executor) = setup_active_entry().await;

            // Configure modify to fail (timeout, but it actually went through)
            executor.set_modify_error(GatewayError::Internal {
                message: "timeout".to_string(),
                source: None,
            });

            // Verification shows NEW stop price -> secretly succeeded!
            // Price 55000, 5% trailing -> stop at 52250
            executor.set_get_status_result(Ok(StopOrderStatus {
                order_id: "mock-1".to_string(),
                stop_price: dec!(52250),
                is_active: true,
            }));

            // Price goes up 50000 -> 55000
            handler
                .on_price_update("C:BTCUSD", dec!(55000))
                .await
                .unwrap();

            // Entry should be updated to reflect the verified state
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match entry.status {
                TrailingEntryStatus::Active {
                    stop_level,
                    stop_order_id,
                    ..
                } => {
                    assert_eq!(
                        stop_level,
                        dec!(52250),
                        "Stop level should reflect verified state"
                    );
                    assert_eq!(
                        stop_order_id, "mock-1",
                        "Order ID should match verified order"
                    );
                }
                other => panic!("Expected Active state, got {:?}", other),
            }
        }

        #[tokio::test]
        async fn modify_failure_verification_also_fails_preserves_old_state() {
            let (handler, _price_source, executor) = setup_active_entry().await;

            // Configure modify to fail
            executor.set_modify_error(GatewayError::Internal {
                message: "timeout".to_string(),
                source: None,
            });

            // Verification also fails
            executor.set_get_status_result(Err(GatewayError::Internal {
                message: "network error".to_string(),
                source: None,
            }));

            // Price goes up 50000 -> 55000
            handler
                .on_price_update("C:BTCUSD", dec!(55000))
                .await
                .unwrap();

            // Entry should still have original values (conservative approach)
            let entry = handler.get_entry("trailing:order-123").unwrap();
            match entry.status {
                TrailingEntryStatus::Active {
                    stop_level,
                    peak_price,
                    ..
                } => {
                    assert_eq!(
                        stop_level,
                        dec!(47500),
                        "Stop level should remain unchanged when verification fails"
                    );
                    assert_eq!(
                        peak_price,
                        dec!(50000),
                        "Peak price should remain unchanged when verification fails"
                    );
                }
                other => panic!("Expected Active state, got {:?}", other),
            }
        }
    }

    // =========================================================================
    // Recovery Tests (PriceSourceUnavailable -> Tracking)
    // =========================================================================

    mod recovery {
        use super::*;
        use crate::trailing_stop::price_source::MockPriceSource;
        use std::time::Duration;

        fn create_handler_with_mock(
            mut config: TrailingStopConfig,
        ) -> (TrailingStopHandler, Arc<MockPriceSource>) {
            // Use short activation timeout for tests to avoid 5s waits
            config.activation_timeout = Duration::from_millis(10);
            let price_source = Arc::new(MockPriceSource::new());
            let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
            let handler = TrailingStopHandler::new(
                Arc::clone(&price_source) as Arc<dyn PriceSource>,
                executor,
                TradingPlatform::BinanceSpotLive,
                config,
            );
            (handler, price_source)
        }

        /// Registers a trailing stop and fills the primary with no price available,
        /// entering PriceSourceUnavailable.
        async fn register_unavailable(
            handler: &TrailingStopHandler,
            order_id: &str,
            symbol: &str,
        ) -> String {
            let request = create_trailing_stop_request(
                symbol,
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );
            let result = handler.register(order_id, &request).unwrap();
            // Simulate primary fill with no price available -> PriceSourceUnavailable
            handler
                .on_primary_filled(order_id, dec!(50000))
                .await
                .unwrap();
            let entry = handler.get_entry(&result.placeholder_id).unwrap();
            assert!(
                matches!(
                    entry.status,
                    TrailingEntryStatus::PriceSourceUnavailable { .. }
                ),
                "Expected PriceSourceUnavailable, got {:?}",
                entry.status
            );
            result.placeholder_id
        }

        // =================================================================
        // calculate_recovery_backoff() formula tests (pure, no async)
        // =================================================================

        #[test]
        fn backoff_zero_failures_returns_zero() {
            let (handler, _) = create_handler_with_mock(TrailingStopConfig::default());
            assert_eq!(handler.calculate_recovery_backoff(0), Duration::ZERO);
        }

        #[test]
        fn backoff_one_failure_returns_base_interval() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_secs(10),
                max_recovery_backoff: Duration::from_secs(300),
                ..TrailingStopConfig::default()
            };
            let (handler, _) = create_handler_with_mock(config);
            // 10s * 2^0 = 10s
            assert_eq!(
                handler.calculate_recovery_backoff(1),
                Duration::from_secs(10)
            );
        }

        #[test]
        fn backoff_doubles_per_failure() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_secs(10),
                max_recovery_backoff: Duration::from_secs(3000),
                ..TrailingStopConfig::default()
            };
            let (handler, _) = create_handler_with_mock(config);
            // count=1: 10 * 2^0 = 10s
            assert_eq!(
                handler.calculate_recovery_backoff(1),
                Duration::from_secs(10)
            );
            // count=2: 10 * 2^1 = 20s
            assert_eq!(
                handler.calculate_recovery_backoff(2),
                Duration::from_secs(20)
            );
            // count=3: 10 * 2^2 = 40s
            assert_eq!(
                handler.calculate_recovery_backoff(3),
                Duration::from_secs(40)
            );
            // count=4: 10 * 2^3 = 80s
            assert_eq!(
                handler.calculate_recovery_backoff(4),
                Duration::from_secs(80)
            );
            // count=5: 10 * 2^4 = 160s
            assert_eq!(
                handler.calculate_recovery_backoff(5),
                Duration::from_secs(160)
            );
        }

        #[test]
        fn backoff_capped_at_max() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_secs(10),
                max_recovery_backoff: Duration::from_secs(60),
                ..TrailingStopConfig::default()
            };
            let (handler, _) = create_handler_with_mock(config);
            // count=4: 10 * 2^3 = 80s -> capped at 60s
            assert_eq!(
                handler.calculate_recovery_backoff(4),
                Duration::from_secs(60)
            );
            // count=10: 10 * 2^9 = 5120s -> capped at 60s
            assert_eq!(
                handler.calculate_recovery_backoff(10),
                Duration::from_secs(60)
            );
        }

        #[test]
        fn backoff_handles_high_failure_count_without_overflow() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_secs(10),
                max_recovery_backoff: Duration::from_secs(300),
                ..TrailingStopConfig::default()
            };
            let (handler, _) = create_handler_with_mock(config);
            // Exponent capped at 20, so even u32::MAX won't overflow
            let result = handler.calculate_recovery_backoff(u32::MAX);
            assert_eq!(result, Duration::from_secs(300)); // capped at max
        }

        #[tokio::test]
        async fn recovery_activates_entry_when_price_becomes_available() {
            let (handler, price_source) = create_handler_with_mock(TrailingStopConfig::default());

            // Register without price -> PriceSourceUnavailable
            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // Now set price
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Retry
            let stats = handler.retry_unavailable_entries().await;

            // Should have recovered
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.recovered, 1);
            assert_eq!(stats.still_unavailable, 0);

            // Entry should now be Tracking
            let entry = handler.get_entry(&placeholder_id).unwrap();
            match entry.status {
                TrailingEntryStatus::Tracking {
                    peak_price,
                    stop_level,
                    ..
                } => {
                    assert_eq!(peak_price, dec!(50000));
                    assert_eq!(stop_level, dec!(47500)); // 5% below 50000
                }
                other => panic!("Expected Tracking, got {:?}", other),
            }
        }

        #[tokio::test]
        async fn recovery_expires_entry_past_ttl() {
            let config = TrailingStopConfig {
                entry_ttl: Duration::from_millis(0),
                ..TrailingStopConfig::default()
            };
            let (handler, _price_source) = create_handler_with_mock(config);

            // Register without price -> PriceSourceUnavailable
            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // Brief sleep so TTL expires
            tokio::time::sleep(Duration::from_millis(1)).await;

            // Retry
            let stats = handler.retry_unavailable_entries().await;

            // Should have expired
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.expired, 1);
            assert_eq!(stats.recovered, 0);

            // Entry should be Failed
            let entry = handler.get_entry(&placeholder_id).unwrap();
            match &entry.status {
                TrailingEntryStatus::Failed { error, .. } => {
                    assert!(
                        error.contains("TTL expired"),
                        "Error should mention TTL: {}",
                        error
                    );
                }
                other => panic!("Expected Failed, got {:?}", other),
            }
        }

        #[tokio::test]
        async fn recovery_leaves_entry_unchanged_when_still_unavailable() {
            let (handler, _price_source) = create_handler_with_mock(TrailingStopConfig::default());

            // Register without price -> PriceSourceUnavailable
            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // Retry without setting price
            let stats = handler.retry_unavailable_entries().await;

            // Should still be unavailable
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.still_unavailable, 1);
            assert_eq!(stats.recovered, 0);

            // Entry should still be PriceSourceUnavailable
            let entry = handler.get_entry(&placeholder_id).unwrap();
            assert!(matches!(
                entry.status,
                TrailingEntryStatus::PriceSourceUnavailable { .. }
            ));
        }

        #[tokio::test]
        async fn recovery_skips_non_unavailable_entries() {
            let (handler, price_source) = create_handler_with_mock(TrailingStopConfig::default());

            // Register and fill WITH price -> Tracking
            price_source.set_price("C:BTCUSD", dec!(50000));
            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );
            handler.register("order-1", &request).unwrap();
            handler
                .on_primary_filled("order-1", dec!(50000))
                .await
                .unwrap();

            // Verify it's Tracking
            let entry = handler.get_entry("trailing:order-1").unwrap();
            assert!(matches!(entry.status, TrailingEntryStatus::Tracking { .. }));

            // Retry should find nothing to do
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.attempted, 0);
        }

        #[tokio::test]
        async fn recovery_handles_mixed_entries() {
            let (handler, price_source) = create_handler_with_mock(TrailingStopConfig::default());

            // Entry 1: unavailable, will recover
            let _id1 = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // Entry 2: unavailable, won't recover (different symbol, no price)
            let _id2 = register_unavailable(&handler, "order-2", "C:ETHUSD").await;

            // Entry 3: Tracking (already has price, filled)
            price_source.set_price("C:SOLUSD", dec!(100));
            let request = create_trailing_stop_request(
                "C:SOLUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );
            handler.register("order-3", &request).unwrap();
            handler
                .on_primary_filled("order-3", dec!(100))
                .await
                .unwrap();

            // Now set price for entry 1 only
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Retry
            let stats = handler.retry_unavailable_entries().await;

            // 2 unavailable attempted, 1 recovered, 1 still unavailable
            assert_eq!(stats.attempted, 2);
            assert_eq!(stats.recovered, 1);
            assert_eq!(stats.still_unavailable, 1);
            assert_eq!(stats.expired, 0);
        }

        #[tokio::test]
        async fn recovery_no_entries_returns_empty_stats() {
            let (handler, _price_source) = create_handler_with_mock(TrailingStopConfig::default());

            let stats = handler.retry_unavailable_entries().await;

            assert_eq!(stats.attempted, 0);
            assert_eq!(stats.recovered, 0);
            assert_eq!(stats.expired, 0);
            assert_eq!(stats.still_unavailable, 0);
        }

        #[tokio::test]
        async fn recovery_does_not_overwrite_concurrent_cancellation() {
            let (handler, price_source) = create_handler_with_mock(TrailingStopConfig::default());

            // Register without price -> PriceSourceUnavailable
            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // Cancel the entry BEFORE setting price
            handler
                .cancel(&placeholder_id, CancellationReason::UserRequested)
                .unwrap();

            // Verify it's Cancelled
            let entry = handler.get_entry(&placeholder_id).unwrap();
            assert!(matches!(
                entry.status,
                TrailingEntryStatus::Cancelled { .. }
            ));

            // Now set price (recovery would succeed if it tried)
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Retry -- should find no PriceSourceUnavailable entries
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(
                stats.attempted, 0,
                "Cancelled entry should not be attempted"
            );

            // Entry should still be Cancelled
            let entry = handler.get_entry(&placeholder_id).unwrap();
            assert!(matches!(
                entry.status,
                TrailingEntryStatus::Cancelled { .. }
            ));
        }

        #[tokio::test]
        async fn recovery_task_activates_entry_after_interval() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(50),
                activation_timeout: Duration::from_millis(10),
                ..TrailingStopConfig::default()
            };
            let price_source = Arc::new(MockPriceSource::new());
            let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
            let handler = TrailingStopHandler::new_arc(
                Arc::clone(&price_source) as Arc<dyn PriceSource>,
                executor,
                TradingPlatform::BinanceSpotLive,
                config,
            );

            // Register and fill without price -> PriceSourceUnavailable
            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );
            let result = handler.register("order-1", &request).unwrap();
            handler
                .on_primary_filled("order-1", dec!(50000))
                .await
                .unwrap();

            // Verify PriceSourceUnavailable
            let entry = handler.get_entry(&result.placeholder_id).unwrap();
            assert!(matches!(
                entry.status,
                TrailingEntryStatus::PriceSourceUnavailable { .. }
            ));

            // Start recovery task
            handler.start_recovery_task();

            // Set price
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Wait for recovery task to run
            // recovery_interval=50ms + activation_timeout=10ms + margin
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Entry should now be Tracking
            let entry = handler.get_entry(&result.placeholder_id).unwrap();
            assert!(
                matches!(entry.status, TrailingEntryStatus::Tracking { .. }),
                "Expected Tracking after recovery task, got {:?}",
                entry.status
            );
        }

        #[tokio::test]
        async fn stop_recovery_task_stops_the_loop() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(50),
                activation_timeout: Duration::from_millis(10),
                ..TrailingStopConfig::default()
            };
            let price_source = Arc::new(MockPriceSource::new());
            let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
            let handler = TrailingStopHandler::new_arc(
                Arc::clone(&price_source) as Arc<dyn PriceSource>,
                executor,
                TradingPlatform::BinanceSpotLive,
                config,
            );

            // Register and fill without price -> PriceSourceUnavailable
            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );
            let result = handler.register("order-1", &request).unwrap();
            handler
                .on_primary_filled("order-1", dec!(50000))
                .await
                .unwrap();

            // Start recovery task, then stop it
            handler.start_recovery_task();
            handler.stop_recovery_task();

            // Set price so recovery WOULD activate the entry if it ran
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Wait longer than recovery_interval -- if task still ran, entry would activate
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Entry should still be PriceSourceUnavailable (recovery stopped)
            let entry = handler.get_entry(&result.placeholder_id).unwrap();
            assert!(
                matches!(
                    entry.status,
                    TrailingEntryStatus::PriceSourceUnavailable { .. }
                ),
                "Expected PriceSourceUnavailable (recovery stopped), got {:?}",
                entry.status
            );
        }

        #[tokio::test]
        async fn stop_all_tasks_cancels_recovery_and_tracking() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(50),
                activation_timeout: Duration::from_millis(10),
                ..TrailingStopConfig::default()
            };
            let price_source = Arc::new(MockPriceSource::new());
            // Set price so registration succeeds and spawns a tracking task
            price_source.set_price("C:BTCUSD", dec!(50000));

            let executor = Arc::new(MockStopOrderExecutor::new()) as Arc<dyn StopOrderExecutor>;
            let handler = TrailingStopHandler::new_arc(
                Arc::clone(&price_source) as Arc<dyn PriceSource>,
                executor,
                TradingPlatform::BinanceSpotLive,
                config,
            );

            // Register and fill with price -> spawns tracking task
            let request = create_trailing_stop_request(
                "C:BTCUSD",
                Side::Sell,
                dec!(1),
                dec!(5),
                TrailingType::Percent,
            );
            handler.register("order-1", &request).unwrap();
            handler
                .on_primary_filled("order-1", dec!(50000))
                .await
                .unwrap();

            // Start recovery task too
            handler.start_recovery_task();

            // Verify symbol tracking task exists
            assert_eq!(handler.symbol_tasks.len(), 1);

            // Stop everything
            handler.stop_all_tasks();

            // Brief yield for cancellation to propagate
            tokio::time::sleep(Duration::from_millis(10)).await;

            // All symbol tasks should be cancelled (tokens cancelled)
            for entry in &handler.symbol_tasks {
                assert!(
                    entry.value().0.is_cancelled(),
                    "Symbol tracking task should be cancelled"
                );
            }
        }

        #[tokio::test]
        async fn recovery_skips_recently_failed_entry() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(100),
                max_recovery_backoff: Duration::from_secs(60),
                ..TrailingStopConfig::default()
            };
            let (handler, _price_source) = create_handler_with_mock(config);

            // Register without price -> PriceSourceUnavailable
            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // First retry -- fails (no price), increments failure_count to 1
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.still_unavailable, 1);
            assert_eq!(stats.skipped, 0);

            // Verify failure count was incremented
            let entry = handler.get_entry(&placeholder_id).unwrap();
            match &entry.status {
                TrailingEntryStatus::PriceSourceUnavailable {
                    recovery_failure_count,
                    ..
                } => {
                    assert_eq!(*recovery_failure_count, 1);
                }
                other => panic!("Expected PriceSourceUnavailable, got {:?}", other),
            }

            // Immediately retry -- should skip (backoff = 100ms, not elapsed yet)
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.skipped, 1);
            assert_eq!(stats.still_unavailable, 0);
        }

        #[tokio::test]
        async fn recovery_retries_entry_after_backoff_elapses() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(50),
                max_recovery_backoff: Duration::from_secs(60),
                ..TrailingStopConfig::default()
            };
            let (handler, price_source) = create_handler_with_mock(config);

            // Register without price -> PriceSourceUnavailable
            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // First retry -- fails, failure_count becomes 1
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.still_unavailable, 1);
            assert_eq!(stats.skipped, 0);

            // Backoff for failure_count=1 is 50ms * 2^0 = 50ms
            // Wait for backoff to elapse
            tokio::time::sleep(Duration::from_millis(60)).await;

            // Set price so recovery succeeds this time
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Retry -- backoff elapsed, should attempt and succeed
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.attempted, 1);
            assert_eq!(stats.skipped, 0);
            assert_eq!(stats.recovered, 1);

            // Entry should be Tracking
            let entry = handler.get_entry(&placeholder_id).unwrap();
            assert!(
                matches!(entry.status, TrailingEntryStatus::Tracking { .. }),
                "Expected Tracking, got {:?}",
                entry.status
            );
        }

        #[tokio::test]
        async fn recovery_backoff_increases_with_failure_count() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(50),
                max_recovery_backoff: Duration::from_secs(60),
                ..TrailingStopConfig::default()
            };
            let (handler, _price_source) = create_handler_with_mock(config);

            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // 1st retry -- fails, failure_count=1
            handler.retry_unavailable_entries().await;

            // Wait for 1st backoff (50ms) to elapse
            tokio::time::sleep(Duration::from_millis(60)).await;

            // 2nd retry -- fails, failure_count=2
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(
                stats.still_unavailable, 1,
                "Should attempt after 1st backoff"
            );

            // Verify failure count is 2
            let entry = handler.get_entry(&placeholder_id).unwrap();
            match &entry.status {
                TrailingEntryStatus::PriceSourceUnavailable {
                    recovery_failure_count,
                    ..
                } => {
                    assert_eq!(*recovery_failure_count, 2);
                }
                other => panic!("Expected PriceSourceUnavailable, got {:?}", other),
            }

            // 2nd backoff = 50ms * 2^1 = 100ms
            // Wait only 60ms -- should still skip
            tokio::time::sleep(Duration::from_millis(60)).await;
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(
                stats.skipped, 1,
                "Should skip -- 2nd backoff (100ms) not elapsed"
            );

            // Wait remaining ~50ms
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(
                stats.still_unavailable, 1,
                "Should attempt after 2nd backoff elapses"
            );

            // Verify failure count is now 3
            let entry = handler.get_entry(&placeholder_id).unwrap();
            match &entry.status {
                TrailingEntryStatus::PriceSourceUnavailable {
                    recovery_failure_count,
                    ..
                } => {
                    assert_eq!(*recovery_failure_count, 3);
                }
                other => panic!("Expected PriceSourceUnavailable, got {:?}", other),
            }
        }

        #[tokio::test]
        async fn recovery_backoff_capped_at_max() {
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(50),
                max_recovery_backoff: Duration::from_millis(200),
                ..TrailingStopConfig::default()
            };
            let (handler, _price_source) = create_handler_with_mock(config);

            let placeholder_id = register_unavailable(&handler, "order-1", "C:BTCUSD").await;

            // Accumulate many failures: 1st, 2nd, 3rd, 4th, 5th
            // Backoff: 50, 100, 200, 400->200(cap), 800->200(cap)
            for i in 1..=5 {
                // Wait for previous backoff to elapse (cap is 200ms)
                tokio::time::sleep(Duration::from_millis(210)).await;
                let stats = handler.retry_unavailable_entries().await;
                assert_eq!(
                    stats.still_unavailable, 1,
                    "Attempt {i} should not be skipped after waiting max backoff"
                );
            }

            // Verify failure count is 5
            let entry = handler.get_entry(&placeholder_id).unwrap();
            match &entry.status {
                TrailingEntryStatus::PriceSourceUnavailable {
                    recovery_failure_count,
                    ..
                } => {
                    assert_eq!(*recovery_failure_count, 5);
                }
                other => panic!("Expected PriceSourceUnavailable, got {:?}", other),
            }

            // With failure_count=5, raw backoff = 50 * 2^4 = 800ms
            // But capped at 200ms. Verify we can retry after 200ms, not 800ms.
            tokio::time::sleep(Duration::from_millis(210)).await;
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(
                stats.still_unavailable, 1,
                "Should attempt after max backoff (200ms), not raw backoff (800ms)"
            );
        }

        #[tokio::test]
        async fn recovery_stats_track_skipped_entries() {
            // Use 500ms interval so backoff (500ms for count=1, 1000ms for count=2)
            // is well above the per-entry processing time in try_activate_tracking.
            let config = TrailingStopConfig {
                recovery_interval: Duration::from_millis(500),
                max_recovery_backoff: Duration::from_secs(60),
                ..TrailingStopConfig::default()
            };
            let (handler, price_source) = create_handler_with_mock(config);

            // Register 3 entries without price
            let _id1 = register_unavailable(&handler, "order-1", "C:BTCUSD").await;
            let _id2 = register_unavailable(&handler, "order-2", "C:ETHUSD").await;
            let _id3 = register_unavailable(&handler, "order-3", "C:SOLUSD").await;

            // 1st scan -- all 3 attempted, all fail (failure_count -> 1)
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.attempted, 3);
            assert_eq!(stats.still_unavailable, 3);
            assert_eq!(stats.skipped, 0);

            // Set price for one entry so it recovers next attempt
            price_source.set_price("C:BTCUSD", dec!(50000));

            // Wait for 1st backoff (500ms) to elapse
            tokio::time::sleep(Duration::from_millis(510)).await;

            // 2nd scan -- all 3 attempted: 1 recovers, 2 fail again (failure_count -> 2)
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.attempted, 3);
            assert_eq!(stats.recovered, 1);
            assert_eq!(stats.still_unavailable, 2);
            assert_eq!(stats.skipped, 0);

            // Immediately scan again -- remaining 2 should be skipped
            // (backoff for count=2 is 1000ms, scan processing is ~300ms)
            let stats = handler.retry_unavailable_entries().await;
            assert_eq!(stats.attempted, 2);
            assert_eq!(stats.skipped, 2);
            assert_eq!(stats.still_unavailable, 0);
            assert_eq!(stats.recovered, 0);
        }
    }
}
