//! Core domain types for exit order management.
//!
//! Contains the primary data structures for tracking pending stop-loss
//! and take-profit (exit) orders, including the state machine types that
//! track their lifecycle. Also includes configuration and placement result types.
//!
//! There are TWO cancellation result types:
//! - `CancelExitResult` (struct): Simplified result for the `ExitHandling` trait.
//! - `CancelExitResultInternal` (enum): Full internal result with state details.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::models::{Side, TradingPlatform};

// =============================================================================
// Exit Leg Type
// =============================================================================

/// Type of exit order leg: stop-loss or take-profit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitLegType {
    /// Stop-loss order to limit downside.
    StopLoss,
    /// Take-profit order to lock in gains.
    TakeProfit,
}

impl ExitLegType {
    /// Returns the prefix used in placeholder IDs.
    #[must_use]
    pub const fn prefix(&self) -> &'static str {
        match self {
            Self::StopLoss => "exit:sl:",
            Self::TakeProfit => "exit:tp:",
        }
    }

    /// Returns the short suffix used in deterministic client order IDs.
    ///
    /// Used by exit order placement to generate `{primary_order_id}-{suffix}`
    /// for idempotent retry protection.
    #[must_use]
    pub const fn client_order_suffix(&self) -> &'static str {
        match self {
            Self::StopLoss => "sl",
            Self::TakeProfit => "tp",
        }
    }
}

impl std::fmt::Display for ExitLegType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StopLoss => write!(f, "stop_loss"),
            Self::TakeProfit => write!(f, "take_profit"),
        }
    }
}

// =============================================================================
// Actual Order
// =============================================================================

/// A single actual order placed at the provider for a pending exit entry.
///
/// When partial fills occur, multiple `ActualOrder`s may be created for a
/// single pending entry, each tracking a proportional exit order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActualOrder {
    /// The actual order ID returned by the provider.
    pub order_id: String,
    /// Quantity for this specific order (not cumulative).
    pub quantity: Decimal,
    /// Timestamp when this order was placed at the provider.
    pub placed_at: DateTime<Utc>,
    /// Current status of this order at the provider.
    pub status: ActualOrderStatus,
    /// Type of exit order (stop-loss or take-profit).
    pub leg_type: ExitLegType,
}

impl ActualOrder {
    /// Returns the remaining unfilled quantity for this order.
    #[must_use]
    pub fn remaining_qty(&self) -> Decimal {
        match &self.status {
            ActualOrderStatus::Open => self.quantity,
            ActualOrderStatus::PartiallyFilled { filled_qty, .. } => {
                (self.quantity - filled_qty).max(Decimal::ZERO)
            }
            ActualOrderStatus::Filled { .. }
            | ActualOrderStatus::PendingCancel { .. }
            | ActualOrderStatus::Cancelled { .. }
            | ActualOrderStatus::Rejected { .. }
            | ActualOrderStatus::Expired { .. }
            | ActualOrderStatus::Unknown => Decimal::ZERO,
        }
    }

    /// Returns true if this order is actively providing position coverage.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.status,
            ActualOrderStatus::Open | ActualOrderStatus::PartiallyFilled { .. }
        )
    }

    /// Returns true if this order is in a terminal state.
    #[allow(dead_code)]
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ActualOrderStatus::Filled { .. }
                | ActualOrderStatus::Cancelled { .. }
                | ActualOrderStatus::Rejected { .. }
                | ActualOrderStatus::Expired { .. }
        )
    }
}

// =============================================================================
// Actual Order Status
// =============================================================================

/// Status of an individual actual order at the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActualOrderStatus {
    /// Order is active on the exchange.
    Open,
    /// Order is partially filled.
    PartiallyFilled {
        filled_qty: Decimal,
        updated_at: DateTime<Utc>,
    },
    /// Order was fully filled by the provider.
    Filled { filled_at: DateTime<Utc> },
    /// Cancellation has been requested but not yet confirmed.
    PendingCancel { requested_at: DateTime<Utc> },
    /// Order was cancelled.
    Cancelled { cancelled_at: DateTime<Utc> },
    /// Order was rejected by the provider.
    Rejected {
        rejected_at: DateTime<Utc>,
        reason: String,
    },
    /// Order expired.
    Expired { expired_at: DateTime<Utc> },
    /// Order status is unknown.
    Unknown,
}

// =============================================================================
// Cancellation Reason
// =============================================================================

/// Reason why a pending SL/TP entry was cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationReason {
    /// Primary order was cancelled before filling.
    PrimaryCancelled,
    /// Primary order was rejected by the provider.
    PrimaryRejected,
    /// Sibling SL/TP was filled (OCO behavior).
    SiblingFilled,
    /// User explicitly cancelled via API.
    UserRequested,
    /// Position was closed before SL/TP triggered.
    PositionClosed,
    /// System shutdown or cleanup.
    SystemCleanup,
    /// Native OCO order was successfully placed, replacing the backup.
    OcoSucceeded,
}

impl std::fmt::Display for CancellationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrimaryCancelled => write!(f, "primary_cancelled"),
            Self::PrimaryRejected => write!(f, "primary_rejected"),
            Self::SiblingFilled => write!(f, "sibling_filled"),
            Self::UserRequested => write!(f, "user_requested"),
            Self::PositionClosed => write!(f, "position_closed"),
            Self::SystemCleanup => write!(f, "system_cleanup"),
            Self::OcoSucceeded => write!(f, "oco_succeeded"),
        }
    }
}

// =============================================================================
// Exit Entry Status
// =============================================================================

/// Status of a pending exit order entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExitEntryStatus {
    /// Waiting for primary order to fill.
    Pending,
    /// Primary order partially filled; exit orders placed for filled portion.
    PartiallyTriggered {
        filled_qty: Decimal,
        total_qty: Decimal,
        actual_orders: SmallVec<[ActualOrder; 4]>,
    },
    /// Exit orders successfully placed with the provider.
    Placed {
        actual_orders: SmallVec<[ActualOrder; 4]>,
    },
    /// Exit order placement failed after all retries.
    ///
    /// When transitioning from `PartiallyTriggered` or `Placed`, preserves any
    /// exit orders that were successfully placed for earlier partial fills.
    /// These orders remain live at the provider and must not be lost from tracking.
    Failed {
        error: String,
        retry_count: u32,
        failed_at: DateTime<Utc>,
        #[serde(default)]
        actual_orders: SmallVec<[ActualOrder; 4]>,
    },
    /// Primary order was cancelled before filling.
    Cancelled {
        cancelled_at: DateTime<Utc>,
        reason: CancellationReason,
    },
    /// Entry expired due to TTL cleanup.
    Expired { expired_at: DateTime<Utc> },
}

/// Status information for a pending exit entry including audit timestamps.
#[derive(Debug, Clone)]
pub struct ExitEntryStatusInfo {
    /// The current status of the entry.
    pub status: ExitEntryStatus,
    /// When this entry was created (wall-clock time).
    pub created_at: DateTime<Utc>,
    /// When the status was last updated.
    pub updated_at: Option<DateTime<Utc>>,
}

// =============================================================================
// Placeholder ID Helpers
// =============================================================================

/// Generates a placeholder ID for an exit order.
///
/// # Format
/// - Stop-loss: `exit:sl:{primary_order_id}`
/// - Take-profit: `exit:tp:{primary_order_id}`
#[must_use]
pub fn generate_placeholder_id(order_type: ExitLegType, primary_order_id: &str) -> String {
    format!("{}{}", order_type.prefix(), primary_order_id)
}

/// Parses a placeholder ID to extract the order type and primary order ID.
///
/// Returns `None` if the ID is not a valid placeholder format.
#[must_use]
#[allow(dead_code)]
pub fn parse_placeholder_id(placeholder_id: &str) -> Option<(ExitLegType, String)> {
    placeholder_id
        .strip_prefix("exit:sl:")
        .map(|primary_id| (ExitLegType::StopLoss, primary_id.to_string()))
        .or_else(|| {
            placeholder_id
                .strip_prefix("exit:tp:")
                .map(|primary_id| (ExitLegType::TakeProfit, primary_id.to_string()))
        })
}

/// Returns the opposite order side.
///
/// SL/TP orders exit the position, so they must be on the opposite side
/// of the primary order.
#[must_use]
pub const fn opposite_side(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

/// Parses a side string into a `Side` enum.
///
/// Accepts "buy", "sell" (case-insensitive).
/// Returns `None` for invalid input.
#[allow(dead_code)]
#[must_use]
pub fn parse_side(s: &str) -> Option<Side> {
    match s.to_lowercase().as_str() {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

/// Converts a `Side` enum to its lowercase string representation.
#[allow(dead_code)]
#[must_use]
pub const fn side_to_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

// =============================================================================
// Exit Entry Info
// =============================================================================

/// Information about an exit entry needed for reconciliation.
#[derive(Debug, Clone)]
pub struct ExitEntryInfo {
    /// ID of the primary order that has pending exit entries.
    pub primary_order_id: String,
    /// Type of exit order (stop-loss or take-profit).
    pub order_type: ExitLegType,
    /// When this pending entry was created (wall-clock time).
    pub created_at: DateTime<Utc>,
}

/// Information about a failed exit entry for re-broadcasting on reconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedExitInfo {
    /// ID of the primary order whose exit failed.
    pub primary_order_id: String,
    /// Trading symbol (e.g., "BTCUSD").
    pub symbol: String,
    /// Type of exit order that failed (stop-loss or take-profit).
    pub order_type: ExitLegType,
    /// Error that caused the failure.
    pub error: String,
    /// When the failure occurred.
    pub failed_at: DateTime<Utc>,
    /// IDs of exit orders that are still live at the provider.
    pub actual_order_ids: Vec<String>,
}

// =============================================================================
// Cancelled Exit Info
// =============================================================================

/// Information about a cancelled exit entry.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CancelledExitInfo {
    /// Placeholder ID that was returned in the original order response.
    pub placeholder_id: String,
    /// Order ID of the primary order that was cancelled.
    pub primary_order_id: String,
    /// Type of exit order (stop-loss or take-profit).
    pub leg_type: ExitLegType,
    /// Trading platform (e.g., "alpaca", "binance").
    pub platform: String,
    /// Trading symbol (e.g., "BTC/USD", "AAPL").
    pub symbol: String,
    /// Order side of the exit order.
    pub side: Side,
    /// Trigger price for the exit order.
    pub trigger_price: Decimal,
    /// Quantity protected by this exit order.
    pub quantity: Decimal,
}

// =============================================================================
// Cancel Exit Result (Simplified -- for ExitHandling trait)
// =============================================================================

/// Simplified result of cancelling exit orders (returned by `ExitHandling` trait).
#[derive(Debug, Clone)]
pub struct CancelExitResult {
    /// Order IDs that were successfully cancelled.
    pub cancelled: Vec<String>,
    /// Order IDs that failed to cancel.
    pub failed: Vec<String>,
}

// =============================================================================
// Cancel Exit Result (Internal -- full state machine)
// =============================================================================

/// Internal result of attempting to cancel an exit entry.
///
/// This enum provides explicit handling for all possible states, avoiding
/// race conditions by making the state transition atomic within `cancel_entry()`.
#[derive(Debug, Clone)]
pub enum CancelExitResultInternal {
    /// Entry was pending and is now cancelled.
    CancelledPending(CancelledExitInfo),

    /// Entry was already placed at the provider - caller must cancel at exchange.
    NeedsCancelAtProvider {
        /// All actual orders at the exchange that need cancellation.
        actual_orders: SmallVec<[ActualOrder; 4]>,
        /// Trading context for the response.
        info: CancelledExitInfo,
    },

    /// Entry was partially triggered - some orders placed, still waiting for more fills.
    NeedsCancelPartiallyTriggered {
        /// Actual orders already placed at the exchange.
        actual_orders: SmallVec<[ActualOrder; 4]>,
        /// Trading context for the response.
        info: CancelledExitInfo,
    },

    /// Entry was in `Failed` state but has live orders at the provider.
    NeedsCancelFailed {
        /// Actual orders still live at the provider from before the failure.
        actual_orders: SmallVec<[ActualOrder; 4]>,
        /// Trading context for the response.
        info: CancelledExitInfo,
    },

    /// Entry was already in a terminal state (Cancelled, Failed with no orders, Expired).
    AlreadyTerminal {
        /// The terminal status.
        status: ExitEntryStatus,
    },
}

// =============================================================================
// Exit Entry
// =============================================================================

/// Parameters for creating a new [`ExitEntry`].
///
/// Groups the required fields to keep `ExitEntry::new` under the argument limit.
pub struct ExitEntryParams {
    /// ID of the primary order this exit order is attached to.
    pub primary_order_id: String,
    /// Whether this is a stop-loss or take-profit order.
    pub order_type: ExitLegType,
    /// Trading symbol (e.g., "BTC/USD", "AAPL").
    pub symbol: String,
    /// Order side -- opposite of primary order.
    pub side: Side,
    /// Price at which to trigger the order.
    pub trigger_price: Decimal,
    /// Limit price for stop-limit orders (None = stop-market).
    pub limit_price: Option<Decimal>,
    /// Quantity to place.
    pub quantity: Decimal,
    /// Trading platform (e.g., `AlpacaLive`, `BinanceSpotLive`).
    pub platform: TradingPlatform,
}

/// A pending exit order (stop-loss or take-profit) waiting to be placed.
///
/// Created when a primary order with `stop_loss` or `take_profit` is submitted
/// to a provider that doesn't support native bracket orders.
#[derive(Clone)]
#[allow(dead_code)]
pub struct ExitEntry {
    /// Placeholder ID returned to the client (e.g., `exit:sl:abc123`).
    pub placeholder_id: String,
    /// ID of the primary order this exit order is attached to.
    pub primary_order_id: String,
    /// Whether this is a stop-loss or take-profit order.
    pub order_type: ExitLegType,
    /// Trading symbol (e.g., "BTC/USD", "AAPL").
    pub symbol: String,
    /// Order side - opposite of primary order.
    pub side: Side,
    /// Price at which to trigger the order.
    pub trigger_price: Decimal,
    /// Limit price for stop-limit orders (None = stop-market).
    pub limit_price: Option<Decimal>,
    /// Current quantity to place (may be adjusted for partial fills).
    pub quantity: Decimal,
    /// Original quantity from the primary order.
    pub original_quantity: Decimal,
    /// Current status of this pending entry.
    pub status: ExitEntryStatus,
    /// When this entry was created (monotonic, for TTL cleanup).
    pub created_at: Instant,
    /// When this entry was created (wall-clock, for audit trail).
    pub created_at_utc: DateTime<Utc>,
    /// When the status was last updated.
    pub updated_at: Option<DateTime<Utc>>,
    /// Trading platform (e.g., `AlpacaLive`, `BinanceSpotLive`).
    pub platform: TradingPlatform,
    /// Sibling placeholder ID for OCO-like behavior.
    pub sibling_id: Option<String>,
    /// Last fill quantity that was attempted for placement.
    pub last_attempted_fill_qty: Option<Decimal>,
    /// Version counter for optimistic concurrency control.
    pub version: u64,
}

impl std::fmt::Debug for ExitEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExitEntry")
            .field("placeholder_id", &self.placeholder_id)
            .field("primary_order_id", &self.primary_order_id)
            .field("order_type", &self.order_type)
            .field("symbol", &self.symbol)
            .field("side", &self.side)
            .field("trigger_price", &self.trigger_price)
            .field("limit_price", &self.limit_price)
            .field("quantity", &self.quantity)
            .field("original_quantity", &self.original_quantity)
            .field("status", &self.status)
            .field("created_at", &self.created_at)
            .field("created_at_utc", &self.created_at_utc)
            .field("updated_at", &self.updated_at)
            .field("platform", &self.platform)
            .field("sibling_id", &self.sibling_id)
            .field("last_attempted_fill_qty", &self.last_attempted_fill_qty)
            .field("version", &self.version)
            .finish()
    }
}

impl ExitEntry {
    /// Creates a new exit entry from the given parameters.
    #[must_use]
    pub fn new(params: ExitEntryParams) -> Self {
        let placeholder_id = generate_placeholder_id(params.order_type, &params.primary_order_id);
        let now_utc = Utc::now();

        Self {
            placeholder_id,
            primary_order_id: params.primary_order_id,
            order_type: params.order_type,
            symbol: params.symbol,
            side: params.side,
            trigger_price: params.trigger_price,
            limit_price: params.limit_price,
            quantity: params.quantity,
            original_quantity: params.quantity,
            status: ExitEntryStatus::Pending,
            created_at: Instant::now(),
            created_at_utc: now_utc,
            updated_at: None,
            platform: params.platform,
            sibling_id: None,
            last_attempted_fill_qty: None,
            version: 0,
        }
    }

    /// Updates the status and sets the `updated_at` timestamp.
    /// Also increments the version for optimistic concurrency control.
    pub fn set_status(&mut self, status: ExitEntryStatus) {
        self.status = status;
        self.updated_at = Some(Utc::now());
        self.version += 1;
    }

    /// Increments the version counter after a modification.
    pub fn increment_version(&mut self) {
        self.version += 1;
        self.updated_at = Some(Utc::now());
    }

    /// Returns the current version for optimistic concurrency checks.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Checks if this entry has already been triggered (placed or failed).
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_triggered(&self) -> bool {
        matches!(
            self.status,
            ExitEntryStatus::Placed { .. } | ExitEntryStatus::Failed { .. }
        )
    }

    /// Checks if this entry is still pending placement.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(
            self.status,
            ExitEntryStatus::Pending | ExitEntryStatus::PartiallyTriggered { .. }
        )
    }

    /// Returns the time from creation to first placement, if the entry has been placed.
    #[must_use]
    #[allow(dead_code)]
    pub fn placement_latency(&self) -> Option<std::time::Duration> {
        let first_placed_at = match &self.status {
            ExitEntryStatus::Placed { actual_orders }
            | ExitEntryStatus::PartiallyTriggered { actual_orders, .. }
                if !actual_orders.is_empty() =>
            {
                Some(actual_orders[0].placed_at)
            }
            _ => None,
        };

        first_placed_at.and_then(|placed| (placed - self.created_at_utc).to_std().ok())
    }

    /// Returns the total lifecycle duration if the entry is in a terminal state.
    #[must_use]
    #[allow(dead_code)]
    pub fn lifecycle_duration(&self) -> Option<std::time::Duration> {
        let terminal_at = match &self.status {
            ExitEntryStatus::Failed { failed_at, .. } => Some(*failed_at),
            ExitEntryStatus::Cancelled { cancelled_at, .. } => Some(*cancelled_at),
            ExitEntryStatus::Expired { expired_at } => Some(*expired_at),
            _ => None,
        };

        terminal_at.and_then(|t| (t - self.created_at_utc).to_std().ok())
    }
}

// =============================================================================
// Registered Exit Legs
// =============================================================================

/// Result of registering exit orders for a primary order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegisteredExitLegs {
    /// Placeholder ID for stop-loss, if requested.
    pub stop_loss_id: Option<String>,
    /// Placeholder ID for take-profit, if requested.
    pub take_profit_id: Option<String>,
}

impl RegisteredExitLegs {
    /// Creates an empty registration result (no exit orders requested).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Checks if any exit orders were registered.
    #[must_use]
    pub const fn has_any(&self) -> bool {
        self.stop_loss_id.is_some() || self.take_profit_id.is_some()
    }
}

/// Result of pre-emptive backup registration for OCO flow.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExitBackupRegistration {
    /// Unique identifier for this backup registration.
    pub id: String,
    /// Placeholder IDs for the backup exit entries.
    pub placeholder_ids: Vec<String>,
}

// =============================================================================
// Exit Handler Configuration
// =============================================================================

/// Configuration for the Exit Handler.
#[derive(Debug, Clone)]
pub struct ExitHandlerConfig {
    /// Maximum pending entries before rejecting new registrations.
    pub max_pending_entries: usize,
    /// TTL for pending entries (cleanup stale entries).
    pub entry_ttl: Duration,
    /// Circuit breaker failure threshold.
    pub circuit_breaker_threshold: usize,
    /// Circuit breaker failure window.
    pub circuit_breaker_window: Duration,
    /// Minimum quantity threshold for exit orders (dust prevention).
    pub min_exit_order_qty: Decimal,
    /// Retry delays for order placement.
    pub retry_delays: Vec<Duration>,
}

impl Default for ExitHandlerConfig {
    fn default() -> Self {
        Self {
            max_pending_entries: 10_000,
            entry_ttl: Duration::from_secs(86400), // 24 hours
            circuit_breaker_threshold: 3,
            circuit_breaker_window: Duration::from_secs(300), // 5 minutes
            min_exit_order_qty: dec!(0.001),
            retry_delays: vec![
                Duration::from_millis(100),
                Duration::from_millis(300),
                Duration::from_millis(1000),
            ],
        }
    }
}

impl ExitHandlerConfig {
    /// Create config with provider-specific retry delays.
    #[must_use]
    pub fn for_platform(_platform: TradingPlatform) -> Self {
        // All platforms use the same default retry delays for now.
        // Customize per-platform when provider-specific tuning is needed.
        Self::default()
    }
}

// =============================================================================
// Sibling Cancellation
// =============================================================================

/// Result of attempting to cancel/reduce a sibling order.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SiblingCancellation {
    /// Placeholder ID of the sibling that was cancelled/reduced.
    pub sibling_placeholder_id: String,
    /// All actual order IDs that were cancelled at the provider.
    pub actual_order_ids: Vec<String>,
    /// Quantity that was cancelled/reduced.
    pub qty_cancelled: Decimal,
    /// Whether the sibling had been placed at the provider.
    pub was_placed: bool,
    /// Whether all cancellations succeeded.
    pub cancel_success: bool,
    /// Error messages if any cancellations failed.
    pub errors: Vec<String>,
}

impl SiblingCancellation {
    /// Create a cancellation result for a sibling that was placed at the provider (single order).
    #[allow(dead_code)]
    pub fn placed(
        sibling_id: impl Into<String>,
        actual_order_id: impl Into<String>,
        qty: Decimal,
        result: Result<(), crate::error::GatewayError>,
    ) -> Self {
        Self {
            sibling_placeholder_id: sibling_id.into(),
            actual_order_ids: vec![actual_order_id.into()],
            qty_cancelled: qty,
            was_placed: true,
            cancel_success: result.is_ok(),
            errors: result
                .err()
                .map(|e| vec![e.to_string()])
                .unwrap_or_default(),
        }
    }

    /// Create a cancellation result for multiple placed orders.
    pub fn placed_multiple(
        sibling_id: impl Into<String>,
        cancelled_order_ids: Vec<String>,
        qty: Decimal,
        errors: Vec<String>,
    ) -> Self {
        Self {
            sibling_placeholder_id: sibling_id.into(),
            actual_order_ids: cancelled_order_ids,
            qty_cancelled: qty,
            was_placed: true,
            cancel_success: errors.is_empty(),
            errors,
        }
    }

    /// Create a cancellation result for a sibling that was still pending.
    pub fn pending(sibling_id: impl Into<String>, qty: Decimal) -> Self {
        Self {
            sibling_placeholder_id: sibling_id.into(),
            actual_order_ids: Vec::new(),
            qty_cancelled: qty,
            was_placed: false,
            cancel_success: true,
            errors: Vec::new(),
        }
    }
}

// =============================================================================
// Placement Result Types
// =============================================================================

/// Result of a single exit order placement attempt.
#[derive(Debug, Clone)]
pub struct PlacementResult {
    /// The placeholder ID that was processed.
    pub placeholder_id: String,
    /// Type of order (stop-loss or take-profit).
    pub order_type: ExitLegType,
    /// Outcome of the placement attempt.
    pub outcome: PlacementOutcome,
    /// Result of sibling cancellation, if applicable.
    pub sibling_cancellation: Option<SiblingCancellation>,
}

/// Outcome of an exit order placement attempt.
#[derive(Debug, Clone)]
pub enum PlacementOutcome {
    /// Order was successfully placed.
    Success {
        /// The actual order ID from the provider.
        actual_order_id: String,
        /// The placed order details.
        #[allow(dead_code)]
        order: Box<crate::models::Order>,
    },
    /// Order placement failed after all retries.
    Failed {
        /// Human-readable error message.
        error: String,
        /// Number of retry attempts made.
        retry_count: u32,
    },
    /// Entry was already processed (double-fill prevention).
    AlreadyProcessed,
    /// Placement deferred until full fill (wash trade prevention).
    Deferred {
        /// Reason for deferral.
        reason: String,
    },
}

impl PlacementOutcome {
    /// Check if this outcome represents a successful placement.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    /// Check if this outcome represents a failure.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

impl PlacementResult {
    /// Create a successful placement result.
    #[must_use]
    #[allow(dead_code)]
    pub fn success(
        placeholder_id: String,
        order_type: ExitLegType,
        order: crate::models::Order,
    ) -> Self {
        Self {
            placeholder_id,
            order_type,
            outcome: PlacementOutcome::Success {
                actual_order_id: order.id.clone(),
                order: Box::new(order),
            },
            sibling_cancellation: None,
        }
    }

    /// Create a successful placement result with sibling cancellation info.
    #[must_use]
    pub fn success_with_sibling(
        placeholder_id: String,
        order_type: ExitLegType,
        order: crate::models::Order,
        sibling_cancellation: Option<SiblingCancellation>,
    ) -> Self {
        Self {
            placeholder_id,
            order_type,
            outcome: PlacementOutcome::Success {
                actual_order_id: order.id.clone(),
                order: Box::new(order),
            },
            sibling_cancellation,
        }
    }

    /// Create a failed placement result.
    #[must_use]
    pub const fn failed(
        placeholder_id: String,
        order_type: ExitLegType,
        error: String,
        retry_count: u32,
    ) -> Self {
        Self {
            placeholder_id,
            order_type,
            outcome: PlacementOutcome::Failed { error, retry_count },
            sibling_cancellation: None,
        }
    }

    /// Create an already-processed result.
    #[must_use]
    pub const fn already_processed(placeholder_id: String, order_type: ExitLegType) -> Self {
        Self {
            placeholder_id,
            order_type,
            outcome: PlacementOutcome::AlreadyProcessed,
            sibling_cancellation: None,
        }
    }

    /// Create a deferred result (waiting for full fill).
    #[must_use]
    pub const fn deferred(placeholder_id: String, order_type: ExitLegType, reason: String) -> Self {
        Self {
            placeholder_id,
            order_type,
            outcome: PlacementOutcome::Deferred { reason },
            sibling_cancellation: None,
        }
    }

    /// Check if this placement was successful.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self.outcome, PlacementOutcome::Success { .. })
    }

    /// Check if this placement failed.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        matches!(self.outcome, PlacementOutcome::Failed { .. })
    }

    /// Check if this placement was deferred (waiting for full fill).
    #[must_use]
    pub const fn is_deferred(&self) -> bool {
        matches!(self.outcome, PlacementOutcome::Deferred { .. })
    }

    /// Get the actual order ID if placement was successful.
    #[must_use]
    #[allow(dead_code)]
    pub fn actual_order_id(&self) -> Option<&str> {
        match &self.outcome {
            PlacementOutcome::Success {
                actual_order_id, ..
            } => Some(actual_order_id),
            _ => None,
        }
    }

    /// Get the sibling cancellation result, if any.
    #[must_use]
    #[allow(dead_code)]
    pub const fn sibling_cancellation(&self) -> Option<&SiblingCancellation> {
        self.sibling_cancellation.as_ref()
    }
}

// =============================================================================
// Error Classification
// =============================================================================

/// Classification of errors for circuit breaker behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Network errors, timeouts, connection refused, unknown errors.
    /// These count toward the circuit breaker threshold.
    InfrastructureFailure,
    /// HTTP 429 rate limit errors.
    /// These do NOT count - exponential backoff handles them.
    RateLimited,
    /// Maintenance, symbol halted, market closed.
    /// These do NOT count - temporary exchange state.
    ExchangeUnavailable,
}

impl ErrorCategory {
    /// Returns true if this error category should count toward the circuit breaker.
    #[must_use]
    pub const fn should_count_toward_breaker(&self) -> bool {
        matches!(self, Self::InfrastructureFailure)
    }
}

/// Error type for `place_order_with_retry` that distinguishes between
/// different failure modes requiring different handling.
#[derive(Debug)]
#[allow(dead_code)]
pub enum PlaceOrderError {
    /// Normal placement failure after exhausting retries.
    Failed {
        /// Human-readable error message.
        message: String,
        /// Number of retry attempts made.
        retry_count: u32,
        /// Error classification for circuit breaker behavior.
        category: ErrorCategory,
    },
    /// Wash trade detected by the provider.
    WashTrade {
        /// Human-readable error message from provider.
        message: String,
    },
    /// Insufficient balance for the requested quantity.
    InsufficientBalance {
        /// Human-readable error message from provider.
        message: String,
        /// The quantity we tried to place.
        requested_quantity: Decimal,
        /// The quantity actually available in the position.
        available_quantity: Decimal,
    },
}

impl PlaceOrderError {
    /// Check if this is a wash trade error.
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_wash_trade(&self) -> bool {
        matches!(self, Self::WashTrade { .. })
    }

    /// Check if this is an insufficient balance error.
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_insufficient_balance(&self) -> bool {
        matches!(self, Self::InsufficientBalance { .. })
    }

    /// Get the error message.
    #[must_use]
    #[allow(dead_code)]
    pub fn message(&self) -> &str {
        match self {
            Self::Failed { message, .. }
            | Self::WashTrade { message }
            | Self::InsufficientBalance { message, .. } => message,
        }
    }
}

// =============================================================================
// Exit Order Type (simplified, for EventRouter compatibility)
// =============================================================================

/// Type of exit order (simplified enum for `EventRouter` pattern matching).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitOrderType {
    /// Stop-loss order.
    StopLoss,
    /// Take-profit order.
    TakeProfit,
}

impl From<ExitLegType> for ExitOrderType {
    fn from(leg_type: ExitLegType) -> Self {
        match leg_type {
            ExitLegType::StopLoss => Self::StopLoss,
            ExitLegType::TakeProfit => Self::TakeProfit,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_placeholder_id_take_profit() {
        let result = parse_placeholder_id("exit:tp:order-456");
        assert_eq!(
            result,
            Some((ExitLegType::TakeProfit, "order-456".to_string()))
        );
    }

    #[test]
    fn test_parse_placeholder_id_invalid() {
        assert_eq!(parse_placeholder_id("not-a-placeholder"), None);
        assert_eq!(parse_placeholder_id("exit:xx:order-123"), None);
        assert_eq!(parse_placeholder_id("pending:sl:order-123"), None);
    }

    #[test]
    fn test_opposite_side() {
        assert_eq!(opposite_side(Side::Buy), Side::Sell);
        assert_eq!(opposite_side(Side::Sell), Side::Buy);
    }

    #[test]
    fn test_client_order_suffix_stop_loss() {
        assert_eq!(ExitLegType::StopLoss.client_order_suffix(), "sl");
    }

    #[test]
    fn test_client_order_suffix_take_profit() {
        assert_eq!(ExitLegType::TakeProfit.client_order_suffix(), "tp");
    }
}
