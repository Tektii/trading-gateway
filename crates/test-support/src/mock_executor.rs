//! Mock stop order executor for testing trailing stop order lifecycle.
//!
//! Records all calls and allows configuring responses, enabling
//! verification of handler-executor interaction patterns.

use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Utc;

use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::trailing_stop::{
    ModifyStopRequest, PlacedStopOrder, StopOrderExecutor, StopOrderRequest, StopOrderStatus,
};

/// Recorded call to the mock executor.
#[derive(Debug, Clone)]
pub enum ExecutorCall {
    Place(StopOrderRequest),
    Modify(ModifyStopRequest),
    Cancel { order_id: String, symbol: String },
    GetStatus { order_id: String, symbol: String },
}

/// Mock implementation of `StopOrderExecutor` for testing.
///
/// Records all calls and allows configuring responses.
pub struct MockStopOrderExecutor {
    /// Recorded calls for verification
    calls: RwLock<Vec<ExecutorCall>>,
    /// Counter for generating order IDs
    order_counter: AtomicUsize,
    /// Next place_stop_order result (None = success)
    next_place_error: RwLock<Option<GatewayError>>,
    /// Next modify_stop_order result (None = success)
    next_modify_error: RwLock<Option<GatewayError>>,
    /// Next cancel_stop_order result (None = success)
    next_cancel_error: RwLock<Option<GatewayError>>,
    /// Next get_stop_order_status result (None = not found error)
    next_get_status_result: RwLock<Option<Result<StopOrderStatus, GatewayError>>>,
}

impl MockStopOrderExecutor {
    /// Create a new mock executor.
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

    /// Configure the next `place_stop_order` call to fail.
    pub fn set_place_error(&self, error: GatewayError) {
        *self.next_place_error.write().unwrap() = Some(error);
    }

    /// Configure the next `modify_stop_order` call to fail.
    pub fn set_modify_error(&self, error: GatewayError) {
        *self.next_modify_error.write().unwrap() = Some(error);
    }

    /// Configure the next `cancel_stop_order` call to fail.
    pub fn set_cancel_error(&self, error: GatewayError) {
        *self.next_cancel_error.write().unwrap() = Some(error);
    }

    /// Configure the next `get_stop_order_status` result.
    pub fn set_get_status_result(&self, result: Result<StopOrderStatus, GatewayError>) {
        *self.next_get_status_result.write().unwrap() = Some(result);
    }

    /// Get all recorded calls.
    #[must_use]
    pub fn calls(&self) -> Vec<ExecutorCall> {
        self.calls.read().unwrap().clone()
    }

    /// Get the number of `place_stop_order` calls.
    #[must_use]
    pub fn place_call_count(&self) -> usize {
        self.calls
            .read()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, ExecutorCall::Place(_)))
            .count()
    }

    /// Get the number of `modify_stop_order` calls.
    #[must_use]
    pub fn modify_call_count(&self) -> usize {
        self.calls
            .read()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, ExecutorCall::Modify(_)))
            .count()
    }

    /// Get the number of `cancel_stop_order` calls.
    #[must_use]
    pub fn cancel_call_count(&self) -> usize {
        self.calls
            .read()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, ExecutorCall::Cancel { .. }))
            .count()
    }

    /// Get the number of `get_stop_order_status` calls.
    #[must_use]
    pub fn get_status_call_count(&self) -> usize {
        self.calls
            .read()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, ExecutorCall::GetStatus { .. }))
            .count()
    }

    /// Clear all recorded calls and reset error state.
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

    async fn cancel_stop_order(&self, order_id: &str, symbol: &str) -> Result<(), GatewayError> {
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
