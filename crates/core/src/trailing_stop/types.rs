//! Core domain types for trailing stop order management.
//!
//! Contains the primary data structures for tracking trailing stop orders,
//! including state machine types for lifecycle management and stop price
//! calculation functions.
//!
//! - State types and transitions
//! - Stop price calculations
//! - Registration handling
//! - Real-time price feed integration via [`WebSocketPriceSource`](super::ws_price_source::WebSocketPriceSource)
//! - Active stop order modification
//! - Price tracking loop

use std::time::Instant;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use crate::models::{Side, TradingPlatform, TrailingType};

/// Minimum price improvement percentage before modifying stop order.
/// Prevents excessive API calls for tiny price movements.
pub const MIN_PRICE_IMPROVEMENT_PCT: Decimal = dec!(0.1); // 0.1%

/// Generates a placeholder ID for a trailing stop order.
///
/// # Format
/// `trailing:{primary_order_id}`
#[must_use]
pub fn generate_placeholder_id(primary_order_id: &str) -> String {
    format!("trailing:{primary_order_id}")
}

/// Parses a placeholder ID to extract the primary order ID.
///
/// Returns `None` if the ID is not a valid trailing stop placeholder format.
#[must_use]
pub fn parse_placeholder_id(placeholder_id: &str) -> Option<String> {
    placeholder_id
        .strip_prefix("trailing:")
        .map(std::string::ToString::to_string)
}

/// Calculates the stop price for a trailing stop order.
///
/// # Arguments
///
/// * `peak_price` - The current peak price (highest for SELL, lowest for BUY)
/// * `trailing_distance` - The trailing distance value
/// * `trailing_type` - Whether the distance is absolute or percentage
/// * `side` - The order side (determines direction of stop)
///
/// # Returns
///
/// The calculated stop price.
///
/// # Examples
///
/// ```ignore
/// // SELL trailing stop: peak $100, trail 5%
/// let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Percent, Side::Sell);
/// assert_eq!(stop, dec!(95));
///
/// // BUY trailing stop: peak $100, trail $5
/// let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Absolute, Side::Buy);
/// assert_eq!(stop, dec!(105));
/// ```
#[must_use]
pub fn calculate_stop_price(
    peak_price: Decimal,
    trailing_distance: Decimal,
    trailing_type: TrailingType,
    side: Side,
) -> Decimal {
    match (side, trailing_type) {
        // SELL stop: protect long position by selling if price drops
        (Side::Sell, TrailingType::Absolute) => peak_price - trailing_distance,
        (Side::Sell, TrailingType::Percent) => {
            peak_price * (Decimal::ONE - trailing_distance / dec!(100))
        }
        // BUY stop: protect short position by buying if price rises
        (Side::Buy, TrailingType::Absolute) => peak_price + trailing_distance,
        (Side::Buy, TrailingType::Percent) => {
            peak_price * (Decimal::ONE + trailing_distance / dec!(100))
        }
    }
}

/// Determines if a new price represents a peak improvement.
///
/// For SELL orders, a new high is an improvement.
/// For BUY orders, a new low is an improvement.
///
/// # Arguments
///
/// * `current_peak` - The current peak price
/// * `new_price` - The new price to check
/// * `side` - The order side
///
/// # Returns
///
/// `true` if the new price represents an improvement over the current peak.
#[must_use]
pub fn is_peak_improvement(current_peak: Decimal, new_price: Decimal, side: Side) -> bool {
    match side {
        Side::Sell => new_price > current_peak, // Higher is better for SELL
        Side::Buy => new_price < current_peak,  // Lower is better for BUY
    }
}

/// Checks if a stop price adjustment is significant enough to warrant modification.
///
/// Uses [`MIN_PRICE_IMPROVEMENT_PCT`] as the threshold.
///
/// # Arguments
///
/// * `current_stop` - The current stop price
/// * `new_stop` - The proposed new stop price
///
/// # Returns
///
/// `true` if the adjustment is significant enough.
#[must_use]
pub fn should_adjust_stop(current_stop: Decimal, new_stop: Decimal) -> bool {
    if current_stop.is_zero() {
        return true; // First placement
    }

    let improvement = ((new_stop - current_stop).abs() / current_stop) * dec!(100);
    improvement >= MIN_PRICE_IMPROVEMENT_PCT
}

/// Reason why a trailing stop entry was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationReason {
    /// Primary order was cancelled before filling.
    PrimaryCancelled,
    /// Primary order was rejected by the provider.
    PrimaryRejected,
    /// User explicitly cancelled via API.
    UserRequested,
    /// Position was closed.
    PositionClosed,
    /// System shutdown or cleanup.
    SystemCleanup,
}

impl std::fmt::Display for CancellationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrimaryCancelled => write!(f, "primary_cancelled"),
            Self::PrimaryRejected => write!(f, "primary_rejected"),
            Self::UserRequested => write!(f, "user_requested"),
            Self::PositionClosed => write!(f, "position_closed"),
            Self::SystemCleanup => write!(f, "system_cleanup"),
        }
    }
}

/// Status of a trailing stop entry.
///
/// # State Machine
///
/// ```text
/// Pending ─────────────────────────────────────────────> Cancelled (primary cancelled)
///    │
///    │ (fill event + price source available)
///    ▼
/// Tracking ────────────────────────────────────────────> PriceSourceUnavailable (no feed)
///    │                                                          │
///    │ (first price, place stop)                                │ (feed restored)
///    ▼                                                          ▼
/// Active <─────────────────────────────────────────────────────┘
///    │
///    │ (stop triggered)
///    ▼
/// Triggered
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TrailingEntryStatus {
    /// Waiting for primary order to fill before starting to track.
    Pending,

    /// Primary filled, tracking started, initial stop placed.
    Tracking {
        /// Current peak price (highest for SELL, lowest for BUY)
        peak_price: Decimal,
        /// Current calculated stop level
        stop_level: Decimal,
        /// When tracking started
        started_at: DateTime<Utc>,
    },

    /// Stop order has been placed at the provider, continuing to track.
    Active {
        /// Provider's order ID for the stop
        stop_order_id: String,
        /// Current peak price
        peak_price: Decimal,
        /// Current stop level (where the order is placed)
        stop_level: Decimal,
        /// Last time the stop was adjusted
        last_adjusted_at: DateTime<Utc>,
    },

    /// Stop order was triggered and filled (terminal).
    Triggered {
        /// The stop order ID that was filled
        stop_order_id: String,
        /// Price at which the stop was filled
        fill_price: Decimal,
        /// When the stop was triggered
        triggered_at: DateTime<Utc>,
    },

    /// Manually cancelled by user (terminal).
    Cancelled {
        /// When cancelled
        cancelled_at: DateTime<Utc>,
        /// Why cancelled
        reason: CancellationReason,
    },

    /// Failed to place/adjust stop after retries (terminal).
    Failed {
        /// Error message
        error: String,
        /// Number of retry attempts
        retry_count: u32,
        /// When failed
        failed_at: DateTime<Utc>,
    },

    /// Price source unavailable - tracking not active (recoverable).
    ///
    /// This is the state for Phase 1 implementation where WebSocket
    /// integration is not yet complete. Entry can transition back to
    /// Tracking when price source becomes available.
    PriceSourceUnavailable {
        /// When registered
        registered_at: DateTime<Utc>,
        /// Why unavailable
        reason: String,
        /// Number of consecutive recovery failures (for exponential backoff).
        recovery_failure_count: u32,
        /// When the last recovery attempt was made (monotonic, for backoff timing).
        /// Skipped during serialization -- resets to `None` on deserialize,
        /// meaning the entry will be retried immediately after restart.
        #[serde(skip)]
        last_recovery_attempt: Option<Instant>,
    },
}

impl TrailingEntryStatus {
    /// Returns true if this status is terminal (no further transitions possible).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Triggered { .. } | Self::Cancelled { .. } | Self::Failed { .. }
        )
    }

    /// Returns true if this entry is actively being tracked.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Tracking { .. } | Self::Active { .. })
    }

    /// Returns true if this entry is waiting for activation.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending | Self::PriceSourceUnavailable { .. })
    }
}

/// A trailing stop order being tracked by the gateway.
///
/// Created when a trailing stop order is submitted to a provider that doesn't
/// support native trailing stops (e.g., Binance).
#[derive(Clone)]
pub struct TrailingEntry {
    /// Placeholder ID returned to the client (e.g., `trailing:abc123`).
    pub placeholder_id: String,

    /// ID of the primary order this trailing stop is attached to.
    pub primary_order_id: String,

    /// Trading symbol (e.g., "C:BTCUSD", "BTCUSDT").
    pub symbol: String,

    /// Order side (SELL for long exit, BUY for short exit).
    pub side: Side,

    /// Quantity for the trailing stop order.
    pub quantity: Decimal,

    /// Trailing distance value.
    pub trailing_distance: Decimal,

    /// How to interpret the trailing distance.
    pub trailing_type: TrailingType,

    /// Current status of this entry.
    pub status: TrailingEntryStatus,

    /// When this entry was created (monotonic, for TTL cleanup).
    pub created_at: Instant,

    /// When this entry was created (wall-clock, for audit trail).
    pub created_at_utc: DateTime<Utc>,

    /// When the status was last updated.
    pub updated_at: Option<DateTime<Utc>>,

    /// Trading platform.
    pub platform: TradingPlatform,

    /// Version counter for optimistic concurrency control.
    pub version: u64,
}

impl std::fmt::Debug for TrailingEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrailingEntry")
            .field("placeholder_id", &self.placeholder_id)
            .field("primary_order_id", &self.primary_order_id)
            .field("symbol", &self.symbol)
            .field("side", &self.side)
            .field("quantity", &self.quantity)
            .field("trailing_distance", &self.trailing_distance)
            .field("trailing_type", &self.trailing_type)
            .field("status", &self.status)
            .field("created_at_utc", &self.created_at_utc)
            .field("updated_at", &self.updated_at)
            .field("platform", &self.platform)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl TrailingEntry {
    /// Creates a new trailing stop entry.
    #[must_use]
    pub fn new(
        primary_order_id: &str,
        symbol: String,
        side: Side,
        quantity: Decimal,
        trailing_distance: Decimal,
        trailing_type: TrailingType,
        platform: TradingPlatform,
    ) -> Self {
        let placeholder_id = generate_placeholder_id(primary_order_id);
        let now_utc = Utc::now();

        Self {
            placeholder_id,
            primary_order_id: primary_order_id.to_string(),
            symbol,
            side,
            quantity,
            trailing_distance,
            trailing_type,
            status: TrailingEntryStatus::Pending,
            created_at: Instant::now(),
            created_at_utc: now_utc,
            updated_at: None,
            platform,
            version: 0,
        }
    }

    /// Updates the status and sets the `updated_at` timestamp.
    pub fn set_status(&mut self, status: TrailingEntryStatus) {
        self.status = status;
        self.updated_at = Some(Utc::now());
        self.version += 1;
    }

    /// Calculates the stop price given a peak price.
    #[must_use]
    pub fn calculate_stop(&self, peak_price: Decimal) -> Decimal {
        calculate_stop_price(
            peak_price,
            self.trailing_distance,
            self.trailing_type,
            self.side,
        )
    }

    /// Checks if a new price represents a peak improvement.
    #[must_use]
    pub fn is_peak_improvement(&self, current_peak: Decimal, new_price: Decimal) -> bool {
        is_peak_improvement(current_peak, new_price, self.side)
    }

    /// Returns the current peak price if tracking is active.
    #[must_use]
    pub const fn current_peak(&self) -> Option<Decimal> {
        match &self.status {
            TrailingEntryStatus::Tracking { peak_price, .. }
            | TrailingEntryStatus::Active { peak_price, .. } => Some(*peak_price),
            _ => None,
        }
    }

    /// Returns the current stop level if tracking is active.
    #[must_use]
    pub const fn current_stop_level(&self) -> Option<Decimal> {
        match &self.status {
            TrailingEntryStatus::Tracking { stop_level, .. }
            | TrailingEntryStatus::Active { stop_level, .. } => Some(*stop_level),
            _ => None,
        }
    }
}

/// Result of registering a trailing stop order.
#[derive(Debug, Clone)]
pub struct RegisteredTrailingStop {
    /// Placeholder ID returned to the client.
    pub placeholder_id: String,
}

/// Configuration for the Trailing Stop Handler.
#[derive(Debug, Clone)]
pub struct TrailingStopConfig {
    /// Maximum pending entries before rejecting new registrations.
    pub max_pending_entries: usize,
    /// TTL for pending entries (cleanup stale entries).
    pub entry_ttl: std::time::Duration,
    /// Minimum price improvement threshold for stop adjustment.
    pub min_adjustment_threshold_pct: Decimal,
    /// How often to scan for recoverable `PriceSourceUnavailable` entries.
    pub recovery_interval: std::time::Duration,
    /// Maximum backoff duration for per-entry exponential backoff.
    ///
    /// After each failed recovery attempt, the entry's backoff doubles:
    /// `recovery_interval * 2^(failure_count - 1)`, capped at this value.
    pub max_recovery_backoff: std::time::Duration,
    /// Timeout for waiting for a first price update when no cached price exists.
    pub activation_timeout: std::time::Duration,
}

impl Default for TrailingStopConfig {
    fn default() -> Self {
        Self {
            max_pending_entries: 10_000,
            entry_ttl: std::time::Duration::from_secs(86400), // 24 hours
            min_adjustment_threshold_pct: MIN_PRICE_IMPROVEMENT_PCT,
            recovery_interval: std::time::Duration::from_secs(10),
            max_recovery_backoff: std::time::Duration::from_secs(300), // 5 minutes
            activation_timeout: std::time::Duration::from_secs(5),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Placeholder ID Tests
    // =========================================================================

    #[test]
    fn generate_placeholder_id_creates_correct_format() {
        let id = generate_placeholder_id("order-123");
        assert_eq!(id, "trailing:order-123");
    }

    #[test]
    fn parse_placeholder_id_extracts_primary_id() {
        let result = parse_placeholder_id("trailing:order-456");
        assert_eq!(result, Some("order-456".to_string()));
    }

    #[test]
    fn parse_placeholder_id_returns_none_for_invalid() {
        assert_eq!(parse_placeholder_id("not-a-placeholder"), None);
        assert_eq!(parse_placeholder_id("exit:sl:order-123"), None);
        assert_eq!(parse_placeholder_id("trailing:"), Some(String::new()));
    }

    // =========================================================================
    // Stop Price Calculation Tests
    // =========================================================================

    #[test]
    fn sell_stop_absolute_calculates_correctly() {
        // Peak $100, trail $5 -> stop at $95
        let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Absolute, Side::Sell);
        assert_eq!(stop, dec!(95));
    }

    #[test]
    fn sell_stop_percent_calculates_correctly() {
        // Peak $100, trail 5% -> stop at $95
        let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Percent, Side::Sell);
        assert_eq!(stop, dec!(95));
    }

    #[test]
    fn buy_stop_absolute_calculates_correctly() {
        // Peak $100, trail $5 -> stop at $105
        let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Absolute, Side::Buy);
        assert_eq!(stop, dec!(105));
    }

    #[test]
    fn buy_stop_percent_calculates_correctly() {
        // Peak $100, trail 5% -> stop at $105
        let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Percent, Side::Buy);
        assert_eq!(stop, dec!(105));
    }

    #[test]
    fn sell_stop_percent_with_fractional_values() {
        // Peak $150.50, trail 2.5% -> stop at $146.7375
        let stop = calculate_stop_price(dec!(150.50), dec!(2.5), TrailingType::Percent, Side::Sell);
        assert_eq!(stop, dec!(146.7375));
    }

    #[test]
    fn buy_stop_percent_with_fractional_values() {
        // Peak $50.25, trail 3% -> stop at $51.7575
        let stop = calculate_stop_price(dec!(50.25), dec!(3), TrailingType::Percent, Side::Buy);
        assert_eq!(stop, dec!(51.7575));
    }

    // =========================================================================
    // Peak Improvement Tests
    // =========================================================================

    #[test]
    fn sell_peak_improvement_when_price_higher() {
        assert!(is_peak_improvement(dec!(100), dec!(101), Side::Sell));
        assert!(is_peak_improvement(dec!(100), dec!(100.01), Side::Sell));
    }

    #[test]
    fn sell_no_improvement_when_price_lower_or_equal() {
        assert!(!is_peak_improvement(dec!(100), dec!(99), Side::Sell));
        assert!(!is_peak_improvement(dec!(100), dec!(100), Side::Sell));
    }

    #[test]
    fn buy_peak_improvement_when_price_lower() {
        assert!(is_peak_improvement(dec!(100), dec!(99), Side::Buy));
        assert!(is_peak_improvement(dec!(100), dec!(99.99), Side::Buy));
    }

    #[test]
    fn buy_no_improvement_when_price_higher_or_equal() {
        assert!(!is_peak_improvement(dec!(100), dec!(101), Side::Buy));
        assert!(!is_peak_improvement(dec!(100), dec!(100), Side::Buy));
    }

    // =========================================================================
    // Stop Adjustment Threshold Tests
    // =========================================================================

    #[test]
    fn should_adjust_when_first_placement() {
        assert!(should_adjust_stop(Decimal::ZERO, dec!(100)));
    }

    #[test]
    fn should_adjust_when_significant_improvement() {
        // 0.2% improvement (above 0.1% threshold)
        assert!(should_adjust_stop(dec!(100), dec!(100.20)));
    }

    #[test]
    fn should_not_adjust_when_tiny_improvement() {
        // 0.05% improvement (below 0.1% threshold)
        assert!(!should_adjust_stop(dec!(100), dec!(100.05)));
    }

    #[test]
    fn should_adjust_at_exact_threshold() {
        // Exactly 0.1% improvement
        assert!(should_adjust_stop(dec!(100), dec!(100.10)));
    }
}
