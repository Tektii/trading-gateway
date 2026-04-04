//! Exit order management (stop-loss and take-profit).
//!
//! This module provides unified exit order management for trading strategies,
//! handling stop-loss (SL) and take-profit (TP) orders that are placed progressively
//! as primary orders fill.
//!
//! # Architecture
//!
//! - **`ExitHandling`**: Trait for exit order management (used by `EventRouter`)
//! - **`ExitHandler`**: Per-adapter handler implementing `ExitHandling`
//! - **Types**: Core domain types (`ExitEntry`, `ExitEntryStatus`, `ActualOrder`, etc.)
//! - **Coverage**: Position coverage calculation for active orders
//! - **Circuit Breaker**: Risk management to halt placements after failures

pub mod cancellation;
pub mod circuit_breaker;
pub mod coverage;
pub mod fill_handler;
pub mod handler;
pub mod registry;
pub mod types;

pub use circuit_breaker::{CircuitState, ExitOrderCircuitBreaker};
pub use handler::{ExitHandler, ExitHandling};
pub use registry::ExitHandlerRegistry;

pub use types::{
    ActualOrder, ActualOrderStatus, CancelExitResult, CancelExitResultInternal, CancellationReason,
    CancelledExitInfo, ErrorCategory, ExitBackupRegistration, ExitEntry, ExitEntryInfo,
    ExitEntryParams, ExitEntryStatus, ExitEntryStatusInfo, ExitHandlerConfig, ExitLegType,
    ExitOrderType, FailedExitInfo, PlaceOrderError, PlacementOutcome, PlacementResult,
    RegisteredExitLegs, SiblingCancellation, generate_placeholder_id, opposite_side,
    parse_placeholder_id, parse_side, side_to_str,
};

pub use coverage::{ExitCoverage, build_coverage, calculate_active_coverage};
