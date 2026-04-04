//! Fill handling and order placement for exit orders.

use chrono::Utc;
use rust_decimal::Decimal;
use smallvec::smallvec;
use tracing::{debug, error, info, warn};

use crate::adapter::TradingAdapter;
use crate::error::GatewayError;
use crate::models::OrderRequest;

use super::{
    ActualOrder, ActualOrderStatus, ErrorCategory, ExitEntry, ExitEntryStatus, ExitLegType,
    PlaceOrderError, PlacementResult, calculate_active_coverage,
};

use super::ExitHandler;

/// Parse available quantity from an insufficient balance error message.
///
/// Alpaca format: "insufficient balance for X (requested: Y, available: Z)"
fn parse_available_quantity(error_msg: &str) -> Option<Decimal> {
    error_msg
        .split("available:")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.trim().parse::<Decimal>().ok())
        .filter(|qty| *qty > Decimal::ZERO)
}

/// Classify an error for circuit breaker behavior.
///
/// This determines whether a failed order placement should count toward
/// the circuit breaker threshold.
fn classify_error(error: &GatewayError) -> ErrorCategory {
    // Layer 1: Direct variant matching (reliable)
    match error {
        GatewayError::RateLimited { .. } => return ErrorCategory::RateLimited,
        GatewayError::ProviderUnavailable { .. } => return ErrorCategory::ExchangeUnavailable,
        _ => {}
    }

    // Layer 2: String pattern matching (fallback)
    let error_str = error.to_string().to_lowercase();

    // Rate limit text patterns (backup for ProviderError wrapping rate limit)
    if error_str.contains("rate limit") || error_str.contains("too many requests") {
        return ErrorCategory::RateLimited;
    }

    // Exchange unavailable - Alpaca patterns
    if error_str.contains("market is closed")
        || error_str.contains("is not tradable")
        || error_str.contains("symbol is halted")
        || error_str.contains("not accepting orders")
    {
        return ErrorCategory::ExchangeUnavailable;
    }

    // Exchange unavailable - Binance patterns
    if error_str.contains("-1013")
        || error_str.contains("-4014")
        || error_str.contains("maintenance")
    {
        return ErrorCategory::ExchangeUnavailable;
    }

    // Default: infrastructure failure (counts toward breaker)
    ErrorCategory::InfrastructureFailure
}

impl ExitHandler {
    /// Handle a fill event for a primary order.
    ///
    /// Processes all pending exit entries for the given primary order,
    /// placing proportional orders based on the fill quantity.
    pub async fn handle_fill_internal(
        &self,
        primary_order_id: &str,
        filled_qty: Decimal,
        total_qty: Decimal,
        position_qty: Option<Decimal>,
        adapter: &dyn TradingAdapter,
    ) -> Vec<PlacementResult> {
        let placeholder_ids = self.get_placeholders_for_primary(primary_order_id);

        if placeholder_ids.is_empty() {
            debug!(primary_order_id, "No pending exit entries for this order");
            return Vec::new();
        }

        // Validate quantities
        if filled_qty <= Decimal::ZERO || total_qty <= Decimal::ZERO {
            warn!(
                primary_order_id,
                %filled_qty, %total_qty, "Ignoring fill with invalid quantities"
            );
            return Vec::new();
        }

        let is_partial_fill = filled_qty < total_qty;

        info!(
            primary_order_id,
            placeholder_count = placeholder_ids.len(),
            %filled_qty,
            %total_qty,
            position_qty = ?position_qty,
            is_partial_fill,
            "Processing fill event for exit orders"
        );

        let mut results = Vec::with_capacity(placeholder_ids.len());

        for placeholder_id in placeholder_ids {
            let result = self
                .process_entry_fill(
                    &placeholder_id,
                    filled_qty,
                    total_qty,
                    position_qty,
                    is_partial_fill,
                    adapter,
                )
                .await;
            results.push(result);
        }

        // Log summary
        let success_count = results.iter().filter(|r| r.is_success()).count();
        let failed_count = results.iter().filter(|r| r.is_failed()).count();
        let deferred_count = results.iter().filter(|r| r.is_deferred()).count();

        info!(
            primary_order_id,
            success_count, failed_count, deferred_count, is_partial_fill, "Fill handling complete"
        );

        results
    }

    /// Validate an exit entry is eligible for fill processing.
    ///
    /// Checks that the entry exists, is not in a terminal state, and has not
    /// already been processed for this fill quantity. Returns the cloned entry
    /// on success, or an early-return  on failure.
    fn validate_entry_for_fill(
        &self,
        placeholder_id: &str,
        filled_qty: Decimal,
    ) -> Result<ExitEntry, Box<PlacementResult>> {
        // Get entry
        let entry = if let Some(entry_ref) = self.pending_by_placeholder.get(placeholder_id) {
            entry_ref.value().clone()
        } else {
            warn!(placeholder_id, "Entry not found for fill processing");
            return Err(Box::new(PlacementResult::failed(
                placeholder_id.to_string(),
                ExitLegType::StopLoss,
                "Entry not found".to_string(),
                0,
            )));
        };

        // Skip terminal states
        let is_terminal = matches!(
            entry.status,
            ExitEntryStatus::Cancelled { .. }
                | ExitEntryStatus::Failed { .. }
                | ExitEntryStatus::Expired { .. }
        );
        if is_terminal {
            debug!(
                placeholder_id,
                status = ?entry.status,
                "Skipping fill for terminal entry"
            );
            return Err(Box::new(PlacementResult::deferred(
                placeholder_id.to_string(),
                entry.order_type,
                format!("Entry in terminal state: {:?}", entry.status),
            )));
        }

        // Duplicate fill detection (wash trade prevention)
        if let Some(last_attempted) = entry.last_attempted_fill_qty
            && filled_qty <= last_attempted
        {
            debug!(
                placeholder_id,
                %filled_qty, %last_attempted, "Skipping duplicate fill attempt"
            );
            return Err(Box::new(PlacementResult::already_processed(
                placeholder_id.to_string(),
                entry.order_type,
            )));
        }

        Ok(entry)
    }

    /// Calculate the exit order quantity after accounting for existing coverage.
    ///
    /// Returns  if an order should be placed, or
    ///  if the quantity is below the dust threshold.
    fn calculate_exit_quantity(
        &self,
        placeholder_id: &str,
        entry: &ExitEntry,
        filled_qty: Decimal,
        total_qty: Decimal,
        position_qty: Option<Decimal>,
        is_partial_fill: bool,
    ) -> Result<(Decimal, Vec<ActualOrder>), Box<PlacementResult>> {
        let existing_orders = Self::get_actual_orders_from_entry(entry);
        let already_covered = calculate_active_coverage(&existing_orders, entry.order_type);

        let target_qty = if is_partial_fill {
            filled_qty
        } else {
            position_qty.unwrap_or(total_qty)
        };

        let qty_to_place = target_qty - already_covered;

        if qty_to_place < self.config.min_exit_order_qty {
            debug!(
                placeholder_id,
                %qty_to_place,
                threshold = %self.config.min_exit_order_qty,
                "Quantity below dust threshold"
            );
            return Err(Box::new(PlacementResult::deferred(
                placeholder_id.to_string(),
                entry.order_type,
                format!(
                    "Quantity {qty_to_place} below minimum {}",
                    self.config.min_exit_order_qty
                ),
            )));
        }

        Ok((qty_to_place, existing_orders))
    }

    /// Process a single entry for a fill event.
    pub(super) async fn process_entry_fill(
        &self,
        placeholder_id: &str,
        filled_qty: Decimal,
        total_qty: Decimal,
        position_qty: Option<Decimal>,
        is_partial_fill: bool,
        adapter: &dyn TradingAdapter,
    ) -> PlacementResult {
        let entry = match self.validate_entry_for_fill(placeholder_id, filled_qty) {
            Ok(entry) => entry,
            Err(result) => return *result,
        };

        self.update_last_attempted_fill_qty(placeholder_id, &filled_qty);

        let (qty_to_place, existing_orders) = match self.calculate_exit_quantity(
            placeholder_id,
            &entry,
            filled_qty,
            total_qty,
            position_qty,
            is_partial_fill,
        ) {
            Ok(result) => result,
            Err(result) => return *result,
        };

        // Check circuit breaker
        if self.is_circuit_breaker_open_internal().await {
            warn!(placeholder_id, "Circuit breaker open - deferring placement");
            return PlacementResult::deferred(
                placeholder_id.to_string(),
                entry.order_type,
                "Circuit breaker open".to_string(),
            );
        }

        // Build and place order
        let order_request =
            Self::build_exit_order_request(&entry, qty_to_place, existing_orders.len());
        let placement_result = self
            .place_order_with_retry(&order_request, adapter, placeholder_id, &entry)
            .await;

        match placement_result {
            Ok((placed_order, _retry_count)) => {
                let partial_fill = if is_partial_fill {
                    Some((filled_qty, total_qty))
                } else {
                    None
                };
                self.handle_successful_placement(
                    placeholder_id,
                    &entry,
                    placed_order,
                    partial_fill,
                    adapter,
                )
                .await
            }
            Err((err, retry_count)) => {
                self.handle_placement_error(placeholder_id, &entry, err, retry_count)
                    .await
            }
        }
    }

    /// Handle a successful order placement: update entry state and cancel siblings.
    ///
    /// `partial_fill` is `Some((filled_qty, total_qty))` for partial fills, `None` for full fills.
    async fn handle_successful_placement(
        &self,
        placeholder_id: &str,
        entry: &ExitEntry,
        placed_order: crate::models::Order,
        partial_fill: Option<(Decimal, Decimal)>,
        adapter: &dyn TradingAdapter,
    ) -> PlacementResult {
        let order_id = placed_order.id.clone();
        let placed_qty = placed_order.quantity;

        info!(
            placeholder_id,
            order_id = %order_id,
            placed_qty = %placed_qty,
            entry_type = ?entry.order_type,
            "Successfully placed exit order"
        );

        let new_order = ActualOrder {
            order_id: order_id.clone(),
            quantity: placed_qty,
            placed_at: Utc::now(),
            status: ActualOrderStatus::Open,
            leg_type: entry.order_type,
        };

        if let Some((filled_qty, total_qty)) = partial_fill {
            self.update_entry_partial_fill(
                placeholder_id,
                new_order.clone(),
                filled_qty,
                total_qty,
            );
        } else {
            self.update_entry_to_placed(placeholder_id, new_order.clone());
        }

        // Handle sibling cancellation (OCO behavior)
        if let Some(sibling_id) = &entry.sibling_id {
            let sibling_cancellation = self
                .cancel_sibling_proportionally(sibling_id, placed_qty, adapter)
                .await;

            if let Some(cancellation) = &sibling_cancellation {
                debug!(
                    placeholder_id,
                    sibling_id,
                    cancelled_qty = %cancellation.qty_cancelled,
                    "Cancelled sibling order"
                );
            }

            return PlacementResult::success_with_sibling(
                placeholder_id.to_string(),
                entry.order_type,
                placed_order.clone(),
                sibling_cancellation,
            );
        }

        PlacementResult::success(placeholder_id.to_string(), entry.order_type, placed_order)
    }

    /// Handle a placement error by classifying it and returning the appropriate result.
    async fn handle_placement_error(
        &self,
        placeholder_id: &str,
        entry: &ExitEntry,
        err: PlaceOrderError,
        retry_count: u32,
    ) -> PlacementResult {
        match err {
            PlaceOrderError::InsufficientBalance { message, .. } => {
                warn!(
                    placeholder_id,
                    message = %message,
                    "Insufficient balance for exit order - deferring"
                );
                PlacementResult::deferred(
                    placeholder_id.to_string(),
                    entry.order_type,
                    format!("Insufficient balance: {message}"),
                )
            }
            PlaceOrderError::WashTrade { message } => {
                warn!(
                    placeholder_id,
                    message = %message,
                    "Wash trade detected - deferring"
                );
                PlacementResult::deferred(
                    placeholder_id.to_string(),
                    entry.order_type,
                    format!("Wash trade prevention: {message}"),
                )
            }
            PlaceOrderError::Failed {
                message, category, ..
            } => {
                if category.should_count_toward_breaker() {
                    self.circuit_breaker.write().await.record_failure();
                    warn!(
                        placeholder_id,
                        retry_count, "Exit order failed, counted toward circuit breaker"
                    );
                } else {
                    info!(
                        placeholder_id,
                        ?category,
                        retry_count,
                        "Exit order failed (not counted toward circuit breaker)"
                    );
                }

                self.update_entry_to_failed(placeholder_id, &message, retry_count);

                error!(
                    placeholder_id,
                    error = %message,
                    "Failed to place exit order"
                );
                PlacementResult::failed(
                    placeholder_id.to_string(),
                    entry.order_type,
                    message,
                    retry_count,
                )
            }
        }
    }

    /// Place an order with retry logic.
    async fn place_order_with_retry(
        &self,
        order: &OrderRequest,
        adapter: &dyn TradingAdapter,
        placeholder_id: &str,
        _entry: &ExitEntry,
    ) -> Result<(crate::models::Order, u32), (PlaceOrderError, u32)> {
        let max_attempts = self.config.retry_delays.len() + 1;
        let mut retry_count: u32 = 0;

        for attempt in 0..max_attempts {
            match adapter.submit_order(order).await {
                Ok(order_handle) => match adapter.get_order(&order_handle.id).await {
                    Ok(full_order) => {
                        return Ok((full_order, retry_count));
                    }
                    Err(e) => {
                        // Build a minimal order from the handle if we can't fetch details
                        let fallback_order = crate::models::Order {
                            id: order_handle.id.clone(),
                            client_order_id: order.client_order_id.clone(),
                            symbol: order.symbol.clone(),
                            side: order.side,
                            order_type: order.order_type,
                            quantity: order.quantity,
                            filled_quantity: Decimal::ZERO,
                            remaining_quantity: order.quantity,
                            limit_price: order.limit_price,
                            stop_price: order.stop_price,
                            stop_loss: order.stop_loss,
                            take_profit: order.take_profit,
                            trailing_distance: order.trailing_distance,
                            trailing_type: order.trailing_type,
                            average_fill_price: None,
                            status: order_handle.status,
                            reject_reason: None,
                            position_id: order.position_id.clone(),
                            reduce_only: Some(order.reduce_only),
                            post_only: Some(order.post_only),
                            hidden: Some(order.hidden),
                            display_quantity: order.display_quantity,
                            oco_group_id: order.oco_group_id.clone(),
                            correlation_id: None,
                            time_in_force: order.time_in_force,
                            created_at: Utc::now(),
                            updated_at: Utc::now(),
                        };
                        warn!(
                            placeholder_id,
                            order_id = %order_handle.id,
                            error = %e,
                            "Order placed but could not fetch full details"
                        );
                        return Ok((fallback_order, retry_count));
                    }
                },
                Err(e) => {
                    if let Some((place_err, count)) = self.classify_and_handle_placement_error(
                        &e,
                        order,
                        attempt,
                        placeholder_id,
                        retry_count,
                    ) {
                        return Err((place_err, count));
                    }
                    // If None was returned, the error was retryable and we should continue
                    retry_count = u32::try_from(attempt + 1).unwrap_or(u32::MAX);
                    let delay = self.config.retry_delays[attempt];
                    warn!(
                        placeholder_id,
                        attempt = attempt + 1,
                        max_attempts,
                        delay_ms = delay.as_millis(),
                        error = %e,
                        "Order placement failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        // If we get here, we've somehow exhausted the loop without returning.
        Err((
            PlaceOrderError::Failed {
                message: "Exhausted all retry attempts".to_string(),
                retry_count,
                category: ErrorCategory::InfrastructureFailure,
            },
            retry_count,
        ))
    }

    /// Classify a placement error and return early if it is non-retryable.
    ///
    /// Returns `Some((error, retry_count))` if the caller should return immediately,
    /// or `None` if the error is retryable and the loop should continue.
    fn classify_and_handle_placement_error(
        &self,
        e: &GatewayError,
        order: &OrderRequest,
        attempt: usize,
        _placeholder_id: &str,
        retry_count: u32,
    ) -> Option<(PlaceOrderError, u32)> {
        let error_str = e.to_string();
        let error_str_lower = error_str.to_lowercase();

        if error_str_lower.contains("insufficient")
            || error_str_lower.contains("balance")
            || error_str_lower.contains("not enough")
        {
            let available_quantity = parse_available_quantity(&error_str).unwrap_or(Decimal::ZERO);

            return Some((
                PlaceOrderError::InsufficientBalance {
                    message: error_str,
                    requested_quantity: order.quantity,
                    available_quantity,
                },
                retry_count,
            ));
        }

        if error_str_lower.contains("wash") || error_str_lower.contains("self-trade") {
            return Some((
                PlaceOrderError::WashTrade {
                    message: e.to_string(),
                },
                retry_count,
            ));
        }

        // If this was the last attempt, classify and return the error
        if attempt >= self.config.retry_delays.len() {
            let current_retry_count = u32::try_from(attempt + 1).unwrap_or(u32::MAX);
            let category = classify_error(e);
            return Some((
                PlaceOrderError::Failed {
                    message: e.to_string(),
                    retry_count: current_retry_count,
                    category,
                },
                current_retry_count,
            ));
        }

        // Retryable -- caller should continue the loop
        None
    }

    /// Build an exit order request with the specified quantity.
    ///
    /// `existing_order_count` is the number of actual orders already placed for
    /// this exit entry. Used to generate unique-but-deterministic `client_order_id`
    /// values across partial fills while preserving idempotency within retries.
    fn build_exit_order_request(
        entry: &ExitEntry,
        quantity: Decimal,
        existing_order_count: usize,
    ) -> OrderRequest {
        use crate::models::{OrderType, TimeInForce};

        let (order_type, stop_price, limit_price) = match entry.order_type {
            ExitLegType::StopLoss => entry.limit_price.map_or(
                (OrderType::Stop, Some(entry.trigger_price), None),
                |limit| (OrderType::StopLimit, Some(entry.trigger_price), Some(limit)),
            ),
            ExitLegType::TakeProfit => (OrderType::Limit, None, Some(entry.trigger_price)),
        };

        let suffix = entry.order_type.client_order_suffix();
        let client_order_id = if existing_order_count == 0 {
            format!("{}-{}", entry.primary_order_id, suffix)
        } else {
            format!(
                "{}-{}-{}",
                entry.primary_order_id, suffix, existing_order_count
            )
        };

        OrderRequest {
            symbol: entry.symbol.clone(),
            side: entry.side,
            quantity,
            order_type,
            time_in_force: TimeInForce::Gtc,
            limit_price,
            stop_price,
            client_order_id: Some(client_order_id),
            trailing_distance: None,
            trailing_type: None,
            stop_loss: None,
            take_profit: None,
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

    /// Update last attempted fill quantity for wash trade prevention.
    pub(super) fn update_last_attempted_fill_qty(&self, placeholder_id: &str, fill_qty: &Decimal) {
        if let Some(mut entry) = self.pending_by_placeholder.get_mut(placeholder_id) {
            entry.last_attempted_fill_qty = Some(*fill_qty);
            entry.updated_at = Some(Utc::now());
        }
    }

    /// Update entry state for partial fill.
    pub(super) fn update_entry_partial_fill(
        &self,
        placeholder_id: &str,
        new_order: ActualOrder,
        filled_qty: Decimal,
        total_qty: Decimal,
    ) {
        if let Some(mut entry) = self.pending_by_placeholder.get_mut(placeholder_id) {
            let entry = entry.value_mut();

            match &mut entry.status {
                ExitEntryStatus::Pending => {
                    entry.status = ExitEntryStatus::PartiallyTriggered {
                        filled_qty,
                        total_qty,
                        actual_orders: smallvec![new_order],
                    };
                }
                ExitEntryStatus::PartiallyTriggered {
                    filled_qty: existing_filled,
                    actual_orders,
                    ..
                } => {
                    *existing_filled = filled_qty;
                    actual_orders.push(new_order);
                }
                _ => {
                    debug!(
                        placeholder_id,
                        "Ignoring partial fill update to terminal state"
                    );
                }
            }
            entry.updated_at = Some(Utc::now());
        }
    }

    /// Update entry state to Placed (full fill complete).
    pub(super) fn update_entry_to_placed(&self, placeholder_id: &str, new_order: ActualOrder) {
        if let Some(mut entry) = self.pending_by_placeholder.get_mut(placeholder_id) {
            let entry = entry.value_mut();

            let mut all_orders = match &entry.status {
                ExitEntryStatus::PartiallyTriggered { actual_orders, .. } => actual_orders.clone(),
                _ => smallvec![],
            };
            all_orders.push(new_order);

            entry.status = ExitEntryStatus::Placed {
                actual_orders: all_orders,
            };
            entry.updated_at = Some(Utc::now());
        }
    }

    /// Update entry state to Failed, preserving any existing actual orders.
    pub(super) fn update_entry_to_failed(
        &self,
        placeholder_id: &str,
        error_message: &str,
        retry_count: u32,
    ) {
        if let Some(mut entry) = self.pending_by_placeholder.get_mut(placeholder_id) {
            let existing_orders = match &entry.status {
                ExitEntryStatus::PartiallyTriggered { actual_orders, .. }
                | ExitEntryStatus::Placed { actual_orders } => actual_orders.clone(),
                _ => smallvec![],
            };
            entry.set_status(ExitEntryStatus::Failed {
                error: error_message.to_string(),
                retry_count,
                failed_at: Utc::now(),
                actual_orders: existing_orders,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit_management::ExitEntryParams;
    use crate::models::{Side, TradingPlatform};
    use rust_decimal_macros::dec;

    #[test]
    fn test_classify_network_error_default() {
        let error = GatewayError::ProviderError {
            message: "Connection refused".to_string(),
            provider: Some("alpaca".to_string()),
            source: None,
        };
        assert_eq!(classify_error(&error), ErrorCategory::InfrastructureFailure);
    }

    #[test]
    fn test_error_category_should_count_infrastructure() {
        assert!(ErrorCategory::InfrastructureFailure.should_count_toward_breaker());
    }

    #[test]
    fn test_error_category_should_not_count_rate_limited() {
        assert!(!ErrorCategory::RateLimited.should_count_toward_breaker());
    }

    #[test]
    fn test_error_category_should_not_count_exchange_unavailable() {
        assert!(!ErrorCategory::ExchangeUnavailable.should_count_toward_breaker());
    }

    #[test]
    fn test_build_exit_order_sets_client_order_id_for_stop_loss() {
        let entry = ExitEntry::new(ExitEntryParams {
            primary_order_id: "primary-order-123".to_string(),
            order_type: ExitLegType::StopLoss,
            symbol: "BTC/USDT".to_string(),
            side: Side::Sell,
            trigger_price: dec!(50000),
            limit_price: None,
            quantity: dec!(1),
            platform: TradingPlatform::BinanceSpotLive,
        });

        let order = ExitHandler::build_exit_order_request(&entry, dec!(1), 0);

        assert_eq!(
            order.client_order_id,
            Some("primary-order-123-sl".to_string()),
            "Stop-loss exit order should have deterministic client_order_id"
        );
    }

    #[test]
    fn test_build_exit_order_sets_client_order_id_for_take_profit() {
        let entry = ExitEntry::new(ExitEntryParams {
            primary_order_id: "primary-order-456".to_string(),
            order_type: ExitLegType::TakeProfit,
            symbol: "BTC/USDT".to_string(),
            side: Side::Sell,
            trigger_price: dec!(60000),
            limit_price: None,
            quantity: dec!(1),
            platform: TradingPlatform::BinanceSpotLive,
        });

        let order = ExitHandler::build_exit_order_request(&entry, dec!(1), 0);

        assert_eq!(
            order.client_order_id,
            Some("primary-order-456-tp".to_string()),
            "Take-profit exit order should have deterministic client_order_id"
        );
    }

    #[test]
    fn test_build_exit_order_client_order_id_is_deterministic() {
        let entry = ExitEntry::new(ExitEntryParams {
            primary_order_id: "order-789".to_string(),
            order_type: ExitLegType::StopLoss,
            symbol: "ETH/USDT".to_string(),
            side: Side::Sell,
            trigger_price: dec!(3000),
            limit_price: None,
            quantity: dec!(2),
            platform: TradingPlatform::AlpacaLive,
        });

        let order1 = ExitHandler::build_exit_order_request(&entry, dec!(2), 0);
        let order2 = ExitHandler::build_exit_order_request(&entry, dec!(2), 0);

        assert_eq!(
            order1.client_order_id, order2.client_order_id,
            "Same inputs must produce same client_order_id for idempotency"
        );
    }

    #[test]
    fn test_build_exit_order_partial_fills_get_unique_client_order_ids() {
        let entry = ExitEntry::new(ExitEntryParams {
            primary_order_id: "order-999".to_string(),
            order_type: ExitLegType::StopLoss,
            symbol: "BTC/USDT".to_string(),
            side: Side::Sell,
            trigger_price: dec!(50000),
            limit_price: None,
            quantity: dec!(1),
            platform: TradingPlatform::BinanceSpotLive,
        });

        let first = ExitHandler::build_exit_order_request(&entry, dec!(0.5), 0);
        let second = ExitHandler::build_exit_order_request(&entry, dec!(0.3), 1);
        let third = ExitHandler::build_exit_order_request(&entry, dec!(0.2), 2);

        assert_eq!(
            first.client_order_id,
            Some("order-999-sl".to_string()),
            "First exit order uses base format"
        );
        assert_eq!(
            second.client_order_id,
            Some("order-999-sl-1".to_string()),
            "Second partial fill appends sequence number"
        );
        assert_eq!(
            third.client_order_id,
            Some("order-999-sl-2".to_string()),
            "Third partial fill appends sequence number"
        );

        // All three must be unique to avoid provider duplicate rejection
        assert_ne!(first.client_order_id, second.client_order_id);
        assert_ne!(second.client_order_id, third.client_order_id);
        assert_ne!(first.client_order_id, third.client_order_id);
    }

    // =========================================================================
    // update_entry_to_failed: actual_orders preservation
    // =========================================================================

    use crate::state::StateManager;
    use std::sync::Arc;

    fn create_test_handler() -> ExitHandler {
        let state_manager = Arc::new(StateManager::new());
        ExitHandler::with_defaults(state_manager, TradingPlatform::BinanceSpotLive)
    }

    fn create_test_entry_with_status(
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
            trigger_price: dec!(50000),
            limit_price: None,
            quantity: dec!(1),
            platform: TradingPlatform::BinanceSpotLive,
        });
        entry.set_status(status);
        entry
    }

    #[test]
    fn test_update_entry_to_failed_preserves_actual_orders_from_partially_triggered() {
        let handler = create_test_handler();

        let existing_order = ActualOrder {
            order_id: "exit-order-123".to_string(),
            quantity: dec!(0.5),
            placed_at: Utc::now(),
            status: ActualOrderStatus::Open,
            leg_type: ExitLegType::StopLoss,
        };

        let entry = create_test_entry_with_status(
            "primary-1",
            ExitLegType::StopLoss,
            "BTCUSDT",
            ExitEntryStatus::PartiallyTriggered {
                filled_qty: dec!(0.5),
                total_qty: dec!(1.0),
                actual_orders: smallvec![existing_order],
            },
        );

        let placeholder_id = entry.placeholder_id.clone();
        handler.insert_entry(entry);

        // Transition to Failed
        handler.update_entry_to_failed(&placeholder_id, "Connection timeout", 3);

        // Verify the Failed state preserves actual_orders
        let status = handler
            .get_status(&placeholder_id)
            .expect("entry should exist");
        match status {
            ExitEntryStatus::Failed {
                actual_orders,
                error,
                retry_count,
                ..
            } => {
                assert_eq!(
                    actual_orders.len(),
                    1,
                    "Should preserve the 1 existing order"
                );
                assert_eq!(actual_orders[0].order_id, "exit-order-123");
                assert_eq!(actual_orders[0].quantity, dec!(0.5));
                assert_eq!(error, "Connection timeout");
                assert_eq!(retry_count, 3);
            }
            other => panic!("Expected Failed status, got: {other:?}"),
        }
    }

    #[test]
    fn test_update_entry_to_failed_from_pending_has_empty_actual_orders() {
        let handler = create_test_handler();

        let entry = create_test_entry_with_status(
            "primary-2",
            ExitLegType::TakeProfit,
            "ETHUSDT",
            ExitEntryStatus::Pending,
        );

        let placeholder_id = entry.placeholder_id.clone();
        handler.insert_entry(entry);

        handler.update_entry_to_failed(&placeholder_id, "Network error", 2);

        let status = handler
            .get_status(&placeholder_id)
            .expect("entry should exist");
        match status {
            ExitEntryStatus::Failed {
                actual_orders,
                error,
                ..
            } => {
                assert!(
                    actual_orders.is_empty(),
                    "Pending entry has no orders to preserve"
                );
                assert_eq!(error, "Network error");
            }
            other => panic!("Expected Failed status, got: {other:?}"),
        }
    }

    #[test]
    fn test_get_actual_orders_from_entry_returns_orders_from_failed() {
        let _handler = create_test_handler();

        let existing_order = ActualOrder {
            order_id: "exit-order-456".to_string(),
            quantity: dec!(0.3),
            placed_at: Utc::now(),
            status: ActualOrderStatus::Open,
            leg_type: ExitLegType::StopLoss,
        };

        let entry = create_test_entry_with_status(
            "primary-3",
            ExitLegType::StopLoss,
            "BTCUSDT",
            ExitEntryStatus::Failed {
                error: "Some error".to_string(),
                retry_count: 1,
                failed_at: Utc::now(),
                actual_orders: smallvec![existing_order],
            },
        );

        let orders = ExitHandler::get_actual_orders_from_entry(&entry);
        assert_eq!(orders.len(), 1, "Should return orders from Failed state");
        assert_eq!(orders[0].order_id, "exit-order-456");
        assert_eq!(
            orders[0].quantity,
            dec!(0.3),
            "Quantity should be preserved"
        );
        assert_eq!(
            orders[0].leg_type,
            ExitLegType::StopLoss,
            "Leg type should be preserved"
        );
    }

    #[test]
    fn test_update_entry_to_failed_preserves_actual_orders_from_placed() {
        let handler = create_test_handler();

        let existing_order = ActualOrder {
            order_id: "exit-order-789".to_string(),
            quantity: dec!(1.0),
            placed_at: Utc::now(),
            status: ActualOrderStatus::Open,
            leg_type: ExitLegType::TakeProfit,
        };

        let entry = create_test_entry_with_status(
            "primary-4",
            ExitLegType::TakeProfit,
            "ETHUSDT",
            ExitEntryStatus::Placed {
                actual_orders: smallvec![existing_order],
            },
        );

        let placeholder_id = entry.placeholder_id.clone();
        handler.insert_entry(entry);

        handler.update_entry_to_failed(&placeholder_id, "Late confirmation failure", 2);

        let status = handler
            .get_status(&placeholder_id)
            .expect("entry should exist");
        match status {
            ExitEntryStatus::Failed {
                actual_orders,
                error,
                ..
            } => {
                assert_eq!(
                    actual_orders.len(),
                    1,
                    "Should preserve the order from Placed state"
                );
                assert_eq!(actual_orders[0].order_id, "exit-order-789");
                assert_eq!(error, "Late confirmation failure");
            }
            other => panic!("Expected Failed status, got: {other:?}"),
        }
    }

    #[test]
    fn test_update_entry_to_failed_preserves_multiple_actual_orders() {
        let handler = create_test_handler();

        let orders = smallvec![
            ActualOrder {
                order_id: "exit-order-a".to_string(),
                quantity: dec!(0.3),
                placed_at: Utc::now(),
                status: ActualOrderStatus::Open,
                leg_type: ExitLegType::StopLoss,
            },
            ActualOrder {
                order_id: "exit-order-b".to_string(),
                quantity: dec!(0.4),
                placed_at: Utc::now(),
                status: ActualOrderStatus::Open,
                leg_type: ExitLegType::StopLoss,
            },
            ActualOrder {
                order_id: "exit-order-c".to_string(),
                quantity: dec!(0.3),
                placed_at: Utc::now(),
                status: ActualOrderStatus::Open,
                leg_type: ExitLegType::StopLoss,
            },
        ];

        let entry = create_test_entry_with_status(
            "primary-5",
            ExitLegType::StopLoss,
            "BTCUSDT",
            ExitEntryStatus::PartiallyTriggered {
                filled_qty: dec!(1.0),
                total_qty: dec!(1.0),
                actual_orders: orders,
            },
        );

        let placeholder_id = entry.placeholder_id.clone();
        handler.insert_entry(entry);

        handler.update_entry_to_failed(&placeholder_id, "Network timeout on 4th fill", 5);

        let status = handler
            .get_status(&placeholder_id)
            .expect("entry should exist");
        match status {
            ExitEntryStatus::Failed { actual_orders, .. } => {
                assert_eq!(
                    actual_orders.len(),
                    3,
                    "Should preserve all 3 orders from PartiallyTriggered"
                );
                assert_eq!(actual_orders[0].order_id, "exit-order-a");
                assert_eq!(actual_orders[1].order_id, "exit-order-b");
                assert_eq!(actual_orders[2].order_id, "exit-order-c");
            }
            other => panic!("Expected Failed status, got: {other:?}"),
        }
    }
}
