//! Circuit breaker for exit order placement failures.
//!
//! Provides risk management by halting new exit order placements after repeated
//! failures to prevent accumulation of unprotected positions.

use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::{error, info};

/// State of the exit order circuit breaker.
///
/// When closed, normal operation continues. When open, new exit order placements
/// are blocked to prevent further unprotected positions from being created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation - exit order placements allowed.
    Closed,
    /// Circuit tripped - new exit order placements blocked.
    /// Requires manual intervention to reset.
    Open,
}

/// Circuit breaker for exit order placement failures.
///
/// Trips after N failures within a time window to prevent accumulation of
/// unprotected positions when there's a systematic placement problem
/// (e.g., exchange connectivity issues, rate limiting, account issues).
///
/// # Risk Rationale
///
/// If exit order placement repeatedly fails, the system is creating unprotected
/// positions - a serious risk management failure. The circuit breaker halts
/// new placements to:
///
/// 1. Prevent further unprotected position accumulation
/// 2. Alert operators to investigate the systematic failure
/// 3. Require manual verification before resuming
///
/// # Manual Reset Required
///
/// The circuit breaker does NOT auto-reset. This is intentional:
/// - Systematic failures require investigation
/// - Auto-reset could mask underlying problems
/// - Risk officers need to verify exchange status before resuming
#[derive(Debug)]
pub struct ExitOrderCircuitBreaker {
    /// Number of failures required to trip the breaker.
    failure_threshold: usize,
    /// Time window for counting failures.
    failure_window: Duration,
    /// Recent failure timestamps within the window.
    recent_failures: VecDeque<Instant>,
    /// Current circuit state.
    state: CircuitState,
    /// When the breaker was last reset via API (cooldown enforcement).
    last_reset_at: Option<Instant>,
    /// Minimum time between resets (prevents strategies from defeating the breaker).
    reset_cooldown: Duration,
}

impl ExitOrderCircuitBreaker {
    /// Create a new circuit breaker with the given threshold and window.
    ///
    /// # Arguments
    ///
    /// * `failure_threshold` - Number of failures to trip the breaker
    /// * `failure_window` - Time window for counting failures
    #[must_use]
    pub fn new(failure_threshold: usize, failure_window: Duration) -> Self {
        Self {
            failure_threshold,
            failure_window,
            recent_failures: VecDeque::with_capacity(failure_threshold),
            state: CircuitState::Closed,
            last_reset_at: None,
            reset_cooldown: Duration::from_secs(60),
        }
    }

    /// Record a placement failure.
    ///
    /// Prunes old failures outside the window and checks if the threshold
    /// is reached. If so, trips the circuit breaker.
    pub fn record_failure(&mut self) {
        let now = Instant::now();

        // Prune old failures outside the window
        while let Some(ts) = self.recent_failures.front() {
            if now.duration_since(*ts) > self.failure_window {
                self.recent_failures.pop_front();
            } else {
                break;
            }
        }

        self.recent_failures.push_back(now);

        // Check if threshold reached
        if self.recent_failures.len() >= self.failure_threshold {
            self.state = CircuitState::Open;
            error!(
                failures = self.recent_failures.len(),
                threshold = self.failure_threshold,
                window_secs = self.failure_window.as_secs(),
                "Exit order circuit breaker TRIPPED - halting new placements. Manual reset required."
            );
        }
    }

    /// Check if the circuit breaker is open (blocking new placements).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state == CircuitState::Open
    }

    /// Reset the circuit breaker to closed state.
    ///
    /// Returns `Err` if the breaker re-tripped within the cooldown window,
    /// preventing strategies from defeating the breaker in a tight loop.
    ///
    /// Should only be called after manual investigation of the failure cause.
    pub fn reset(&mut self) -> Result<(), &'static str> {
        if let Some(last) = self.last_reset_at
            && last.elapsed() < self.reset_cooldown
        {
            return Err("Circuit breaker re-tripped within cooldown period. \
                 Investigate the underlying failure before resetting again.");
        }
        self.recent_failures.clear();
        self.state = CircuitState::Closed;
        self.last_reset_at = Some(Instant::now());
        info!("Exit order circuit breaker reset - placements resumed");
        Ok(())
    }

    /// Get the current number of recent failures within the window.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.recent_failures.len()
    }

    /// Get the current circuit state.
    #[must_use]
    pub const fn state(&self) -> CircuitState {
        self.state
    }
}

impl Default for ExitOrderCircuitBreaker {
    fn default() -> Self {
        // Default: 3 failures in 5 minutes trips the breaker
        Self::new(3, Duration::from_secs(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn breaker_starts_closed() {
        let breaker = ExitOrderCircuitBreaker::new(3, Duration::from_secs(300));
        assert!(!breaker.is_open());
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.failure_count(), 0);
    }

    #[test]
    fn breaker_trips_after_threshold() {
        let mut breaker = ExitOrderCircuitBreaker::new(3, Duration::from_secs(300));

        breaker.record_failure();
        assert!(!breaker.is_open());

        breaker.record_failure();
        assert!(!breaker.is_open());

        breaker.record_failure();
        assert!(breaker.is_open());
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn breaker_stays_open_after_more_failures() {
        let mut breaker = ExitOrderCircuitBreaker::new(2, Duration::from_secs(300));

        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.is_open());

        // Additional failures should not panic or change state
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.is_open());
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn breaker_reset() {
        let mut breaker = ExitOrderCircuitBreaker::new(3, Duration::from_secs(300));

        for _ in 0..3 {
            breaker.record_failure();
        }
        assert!(breaker.is_open());

        breaker.reset().expect("reset should succeed");
        assert!(!breaker.is_open());
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.failure_count(), 0);
    }

    #[test]
    fn breaker_can_retrip_after_reset() {
        let mut breaker = ExitOrderCircuitBreaker::new(2, Duration::from_secs(300));

        // Trip
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.is_open());

        // Reset
        breaker.reset().expect("reset should succeed");
        assert!(!breaker.is_open());

        // Trip again
        breaker.record_failure();
        assert!(!breaker.is_open());
        breaker.record_failure();
        assert!(breaker.is_open());
    }

    #[test]
    fn window_pruning_expires_old_failures() {
        let mut breaker = ExitOrderCircuitBreaker::new(3, Duration::from_millis(50));

        // Record 2 failures
        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.failure_count(), 2);

        // Wait for window to expire
        thread::sleep(Duration::from_millis(60));

        // This failure should prune the old ones first
        breaker.record_failure();
        assert!(!breaker.is_open(), "old failures should have been pruned");
        assert_eq!(breaker.failure_count(), 1);
    }

    #[test]
    fn default_trips_after_three_failures() {
        let mut breaker = ExitOrderCircuitBreaker::default();

        breaker.record_failure();
        breaker.record_failure();
        assert!(!breaker.is_open());

        breaker.record_failure();
        assert!(breaker.is_open());
    }
}
