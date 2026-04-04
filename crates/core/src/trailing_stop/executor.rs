//! Stop order executor trait for trailing stop order lifecycle.
//!
//! This module provides the abstraction layer between the `TrailingStopHandler`
//! and the actual exchange API. This allows:
//!
//! - Handler to remain testable with mock executors
//! - Different providers to implement their own order placement logic
//! - Clean separation between tracking logic and order execution
//!
//! # Trait Design
//!
//! The `StopOrderExecutor` trait provides three operations:
//!
//! - `place_stop_order`: Initial stop order placement
//! - `modify_stop_order`: Atomic cancel-replace for stop adjustment
//! - `cancel_stop_order`: Cancel an active stop order
//!
//! # Error Handling
//!
//! All operations return `Result<T, GatewayError>`. The handler uses these
//! errors to:
//!
//! - Transition entries to `Failed` state on placement errors
//! - Keep old stops active on modification errors (atomic semantics)
//! - Record failures in the circuit breaker

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::error::GatewayError;
use crate::models::Side;

/// Request to place a stop order at the provider.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Public API - fields used by provider-specific executors
pub struct StopOrderRequest {
    /// Trading symbol (e.g., "BTCUSDT")
    pub symbol: String,
    /// Order side (Sell for long protection, Buy for short protection)
    pub side: Side,
    /// Quantity to trade when stop is triggered
    pub quantity: Decimal,
    /// Stop trigger price
    pub stop_price: Decimal,
    /// Limit price for the order after trigger (for `STOP_LOSS_LIMIT` orders)
    pub limit_price: Decimal,
}

/// Request to modify an existing stop order.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Public API - fields used by provider-specific executors
pub struct ModifyStopRequest {
    /// The provider's order ID for the existing stop
    pub order_id: String,
    /// Trading symbol
    pub symbol: String,
    /// Order side (Sell for long protection, Buy for short protection)
    pub side: Side,
    /// Quantity to trade when stop is triggered
    pub quantity: Decimal,
    /// New stop trigger price
    pub new_stop_price: Decimal,
    /// New limit price after trigger
    pub new_limit_price: Decimal,
}

/// Result of placing or modifying a stop order.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Public API - fields used by provider-specific executors
pub struct PlacedStopOrder {
    /// Provider's order ID for the stop
    pub order_id: String,
    /// The actual stop price (may differ slightly due to tick size)
    pub stop_price: Decimal,
    /// When the order was placed/modified
    pub placed_at: DateTime<Utc>,
}

/// Current status of a stop order at the provider.
///
/// Used to verify order state after ambiguous failures (e.g., timeouts
/// during modification), allowing the handler to reconcile its internal
/// state with the actual provider state.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Public API - is_active will be used for triggered detection
pub struct StopOrderStatus {
    /// Provider's order ID
    pub order_id: String,
    /// Current stop trigger price at the provider
    pub stop_price: Decimal,
    /// Whether the order is still active (open/new)
    pub is_active: bool,
}

/// Trait for executing stop orders at a trading provider.
///
/// Implementors handle the actual HTTP/WebSocket communication with
/// the provider's API.
#[async_trait]
#[allow(dead_code)] // Public API - cancel_stop_order used by handler
pub trait StopOrderExecutor: Send + Sync {
    /// Place a new stop order at the provider.
    ///
    /// # Arguments
    ///
    /// * `request` - Details for the stop order
    ///
    /// # Returns
    ///
    /// On success, returns the placed order details including the provider's order ID.
    ///
    /// # Errors
    ///
    /// - Network/connectivity issues
    /// - Insufficient balance
    /// - Invalid symbol
    /// - Rate limiting
    async fn place_stop_order(
        &self,
        request: &StopOrderRequest,
    ) -> Result<PlacedStopOrder, GatewayError>;

    /// Modify an existing stop order with atomic cancel-replace.
    ///
    /// Uses atomic cancel-replace semantics: either both the cancel and
    /// new order succeed, or neither does. This prevents leaving the
    /// position unprotected during modification.
    ///
    /// # Arguments
    ///
    /// * `request` - Details for the modification
    ///
    /// # Returns
    ///
    /// On success, returns the new order details.
    ///
    /// # Errors
    ///
    /// - Original order already filled/cancelled
    /// - Network/connectivity issues
    /// - Cancel-replace rejected by provider
    async fn modify_stop_order(
        &self,
        request: &ModifyStopRequest,
    ) -> Result<PlacedStopOrder, GatewayError>;

    /// Cancel an existing stop order.
    ///
    /// # Arguments
    ///
    /// * `order_id` - Provider's order ID
    /// * `symbol` - Trading symbol (required by some providers)
    ///
    /// # Errors
    ///
    /// - Order already filled/cancelled
    /// - Network/connectivity issues
    async fn cancel_stop_order(&self, order_id: &str, symbol: &str) -> Result<(), GatewayError>;

    /// Query the current status of a stop order at the provider.
    ///
    /// Used after ambiguous modify failures (e.g., timeouts) to verify
    /// whether the modification actually succeeded at the provider.
    /// This allows the handler to reconcile its internal state with reality.
    ///
    /// # Arguments
    ///
    /// * `order_id` - Provider's order ID
    /// * `symbol` - Trading symbol (required by some providers)
    ///
    /// # Returns
    ///
    /// Current status of the order at the provider.
    ///
    /// # Errors
    ///
    /// - Order not found
    /// - Network/connectivity issues
    async fn get_stop_order_status(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> Result<StopOrderStatus, GatewayError>;
}

/// Mock executor for core's own unit tests.
///
/// External consumers should use `tektii_gateway_test_support::mock_executor::MockStopOrderExecutor`.
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone)]
    pub enum ExecutorCall {
        Place(StopOrderRequest),
        Modify(ModifyStopRequest),
        Cancel { order_id: String, symbol: String },
        GetStatus { order_id: String, symbol: String },
    }

    pub struct MockStopOrderExecutor {
        calls: RwLock<Vec<ExecutorCall>>,
        order_counter: AtomicUsize,
        next_place_error: RwLock<Option<GatewayError>>,
        next_modify_error: RwLock<Option<GatewayError>>,
        next_cancel_error: RwLock<Option<GatewayError>>,
        next_get_status_result: RwLock<Option<Result<StopOrderStatus, GatewayError>>>,
    }

    impl MockStopOrderExecutor {
        #[must_use]
        pub fn new() -> Self {
            Self {
                calls: RwLock::new(Vec::new()),
                order_counter: AtomicUsize::new(1),
                next_place_error: RwLock::new(None),
                next_modify_error: RwLock::new(None),
                next_cancel_error: RwLock::new(None),
                next_get_status_result: RwLock::new(None),
            }
        }

        pub fn set_place_error(&self, error: GatewayError) {
            *self.next_place_error.write().unwrap() = Some(error);
        }

        pub fn set_modify_error(&self, error: GatewayError) {
            *self.next_modify_error.write().unwrap() = Some(error);
        }

        pub fn set_cancel_error(&self, error: GatewayError) {
            *self.next_cancel_error.write().unwrap() = Some(error);
        }

        pub fn set_get_status_result(&self, result: Result<StopOrderStatus, GatewayError>) {
            *self.next_get_status_result.write().unwrap() = Some(result);
        }

        #[must_use]
        pub fn calls(&self) -> Vec<ExecutorCall> {
            self.calls.read().unwrap().clone()
        }

        #[must_use]
        pub fn place_call_count(&self) -> usize {
            self.calls
                .read()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, ExecutorCall::Place(_)))
                .count()
        }

        #[must_use]
        pub fn modify_call_count(&self) -> usize {
            self.calls
                .read()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, ExecutorCall::Modify(_)))
                .count()
        }

        #[must_use]
        pub fn cancel_call_count(&self) -> usize {
            self.calls
                .read()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, ExecutorCall::Cancel { .. }))
                .count()
        }

        #[must_use]
        pub fn get_status_call_count(&self) -> usize {
            self.calls
                .read()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, ExecutorCall::GetStatus { .. }))
                .count()
        }

        pub fn reset(&self) {
            self.calls.write().unwrap().clear();
            *self.next_place_error.write().unwrap() = None;
            *self.next_modify_error.write().unwrap() = None;
            *self.next_cancel_error.write().unwrap() = None;
            *self.next_get_status_result.write().unwrap() = None;
        }
    }

    impl Default for MockStopOrderExecutor {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl StopOrderExecutor for MockStopOrderExecutor {
        async fn place_stop_order(
            &self,
            request: &StopOrderRequest,
        ) -> Result<PlacedStopOrder, GatewayError> {
            self.calls
                .write()
                .unwrap()
                .push(ExecutorCall::Place(request.clone()));
            if let Some(error) = self.next_place_error.write().unwrap().take() {
                return Err(error);
            }
            let order_id = format!("mock-{}", self.order_counter.fetch_add(1, Ordering::SeqCst));
            Ok(PlacedStopOrder {
                order_id,
                stop_price: request.stop_price,
                placed_at: Utc::now(),
            })
        }

        async fn modify_stop_order(
            &self,
            request: &ModifyStopRequest,
        ) -> Result<PlacedStopOrder, GatewayError> {
            self.calls
                .write()
                .unwrap()
                .push(ExecutorCall::Modify(request.clone()));
            if let Some(error) = self.next_modify_error.write().unwrap().take() {
                return Err(error);
            }
            let order_id = format!("mock-{}", self.order_counter.fetch_add(1, Ordering::SeqCst));
            Ok(PlacedStopOrder {
                order_id,
                stop_price: request.new_stop_price,
                placed_at: Utc::now(),
            })
        }

        async fn cancel_stop_order(
            &self,
            order_id: &str,
            symbol: &str,
        ) -> Result<(), GatewayError> {
            self.calls.write().unwrap().push(ExecutorCall::Cancel {
                order_id: order_id.to_string(),
                symbol: symbol.to_string(),
            });
            if let Some(error) = self.next_cancel_error.write().unwrap().take() {
                return Err(error);
            }
            Ok(())
        }

        async fn get_stop_order_status(
            &self,
            order_id: &str,
            symbol: &str,
        ) -> Result<StopOrderStatus, GatewayError> {
            self.calls.write().unwrap().push(ExecutorCall::GetStatus {
                order_id: order_id.to_string(),
                symbol: symbol.to_string(),
            });
            if let Some(result) = self.next_get_status_result.write().unwrap().take() {
                return result;
            }
            Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn stop_order_request_has_required_fields() {
        let request = StopOrderRequest {
            symbol: "BTCUSDT".to_string(),
            side: Side::Sell,
            quantity: dec!(1.5),
            stop_price: dec!(47500),
            limit_price: dec!(47400),
        };

        assert_eq!(request.symbol, "BTCUSDT");
        assert_eq!(request.side, Side::Sell);
        assert_eq!(request.quantity, dec!(1.5));
        assert_eq!(request.stop_price, dec!(47500));
        assert_eq!(request.limit_price, dec!(47400));
    }

    #[test]
    fn modify_stop_request_has_required_fields() {
        let request = ModifyStopRequest {
            order_id: "order-123".to_string(),
            symbol: "BTCUSDT".to_string(),
            side: Side::Sell,
            quantity: dec!(1.5),
            new_stop_price: dec!(48000),
            new_limit_price: dec!(47900),
        };

        assert_eq!(request.order_id, "order-123");
        assert_eq!(request.symbol, "BTCUSDT");
        assert_eq!(request.side, Side::Sell);
        assert_eq!(request.quantity, dec!(1.5));
        assert_eq!(request.new_stop_price, dec!(48000));
        assert_eq!(request.new_limit_price, dec!(47900));
    }

    #[test]
    fn placed_stop_order_has_required_fields() {
        let placed = PlacedStopOrder {
            order_id: "order-456".to_string(),
            stop_price: dec!(47500),
            placed_at: Utc::now(),
        };

        assert_eq!(placed.order_id, "order-456");
        assert_eq!(placed.stop_price, dec!(47500));
    }
}
