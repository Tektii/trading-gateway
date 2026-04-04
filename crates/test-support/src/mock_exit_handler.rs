//! Mock `ExitHandling` implementation for integration tests.
//!
//! Records `handle_fill` calls for assertion and returns pre-configured
//! failed entries for reconnection rebroadcast testing.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use rust_decimal::Decimal;

use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayResult;
use tektii_gateway_core::exit_management::{
    CancelExitResult, ExitHandling, FailedExitInfo, PlacementResult,
};

/// A recorded `handle_fill` call.
#[derive(Debug, Clone)]
pub struct HandleFillCall {
    pub primary_order_id: String,
    pub filled_qty: Decimal,
    pub total_qty: Decimal,
    pub position_qty: Option<Decimal>,
}

/// Mock exit handler that records calls for test assertions.
pub struct MockExitHandler {
    fill_calls: Mutex<Vec<HandleFillCall>>,
    failed_entries: Mutex<Vec<FailedExitInfo>>,
    circuit_breaker_open: AtomicBool,
}

impl MockExitHandler {
    /// Create a new mock with no pre-configured state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fill_calls: Mutex::new(Vec::new()),
            failed_entries: Mutex::new(Vec::new()),
            circuit_breaker_open: AtomicBool::new(false),
        }
    }

    /// Pre-configure failed entries that `get_failed_entries()` will return.
    #[must_use]
    pub fn with_failed_entries(self, entries: Vec<FailedExitInfo>) -> Self {
        *self.failed_entries.lock().expect("lock") = entries;
        self
    }

    /// Get all recorded `handle_fill` calls.
    pub fn fill_calls(&self) -> Vec<HandleFillCall> {
        self.fill_calls.lock().expect("lock").clone()
    }
}

impl Default for MockExitHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExitHandling for MockExitHandler {
    async fn handle_fill(
        &self,
        primary_order_id: &str,
        filled_qty: Decimal,
        total_qty: Decimal,
        position_qty: Option<Decimal>,
        _adapter: &dyn TradingAdapter,
    ) -> Vec<PlacementResult> {
        self.fill_calls.lock().expect("lock").push(HandleFillCall {
            primary_order_id: primary_order_id.to_string(),
            filled_qty,
            total_qty,
            position_qty,
        });
        Vec::new()
    }

    async fn handle_cancellation(
        &self,
        _primary_order_id: &str,
    ) -> GatewayResult<CancelExitResult> {
        Ok(CancelExitResult {
            cancelled: vec![],
            failed: vec![],
        })
    }

    fn has_pending_for_primary(&self, _primary_order_id: &str) -> bool {
        false
    }

    async fn cancel_for_position_close(
        &self,
        _symbol: &str,
        _adapter: &dyn TradingAdapter,
    ) -> Vec<String> {
        vec![]
    }

    async fn is_circuit_breaker_open(&self) -> bool {
        self.circuit_breaker_open.load(Ordering::Relaxed)
    }

    fn get_failed_entries(&self) -> Vec<FailedExitInfo> {
        self.failed_entries.lock().expect("lock").clone()
    }

    fn clear_failed_entries(&self) -> usize {
        let mut entries = self.failed_entries.lock().expect("lock");
        let count = entries.len();
        entries.clear();
        count
    }
}
