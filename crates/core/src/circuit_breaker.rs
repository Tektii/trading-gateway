//! Adapter circuit breaker for detecting provider outages.
//!
//! Provides circuit breaker functionality to detect systematic provider outages
//! and fail fast, preventing strategies from wasting time on requests that will
//! likely fail.
//!
//! # Error Classification
//!
//! Only provider outage errors count toward the circuit breaker:
//! - 503 Service Unavailable (provider maintenance)
//! - 502 Bad Gateway (upstream issues)
//! - Connection timeouts
//!
//! The following do NOT count:
//! - 429 Rate Limited (handled by retry with backoff)
//! - 422 Order Rejected (client/business error)
//! - 404 Not Found (resource doesn't exist)
//! - 401 Unauthorized (credential issue)

use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::{error, info};

use crate::error::GatewayError;

/// State of the adapter circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests allowed.
    Closed,
    /// Circuit tripped — requests blocked. Requires manual reset.
    Open,
}

/// Adapter circuit breaker for detecting provider outages.
///
/// Trips after N failures within a time window to prevent strategies from
/// wasting time on requests to an unavailable provider.
#[derive(Debug)]
pub struct AdapterCircuitBreaker {
    /// Number of failures required to trip the breaker.
    failure_threshold: usize,
    /// Time window for counting failures.
    failure_window: Duration,
    /// Recent failure timestamps within the window.
    recent_failures: VecDeque<Instant>,
    /// Current circuit state.
    state: CircuitState,
    /// Provider name for logging.
    provider: String,
    /// When the breaker was last reset via API (cooldown enforcement).
    last_reset_at: Option<Instant>,
    /// Minimum time between resets (prevents strategies from defeating the breaker).
    reset_cooldown: Duration,
}

impl AdapterCircuitBreaker {
    /// Create a new circuit breaker with the given threshold and window.
    #[must_use]
    pub fn new(failure_threshold: usize, failure_window: Duration, provider: &str) -> Self {
        Self {
            failure_threshold,
            failure_window,
            recent_failures: VecDeque::with_capacity(failure_threshold),
            state: CircuitState::Closed,
            provider: provider.to_string(),
            last_reset_at: None,
            reset_cooldown: Duration::from_secs(60),
        }
    }

    /// Record a provider outage failure.
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

        if self.recent_failures.len() >= self.failure_threshold {
            self.state = CircuitState::Open;
            error!(
                provider = %self.provider,
                failures = self.recent_failures.len(),
                threshold = self.failure_threshold,
                window_secs = self.failure_window.as_secs(),
                "Adapter circuit breaker TRIPPED — failing fast on requests. Manual reset required."
            );
        }
    }

    /// Check if the circuit breaker is open (blocking requests).
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
        info!(provider = %self.provider, "Adapter circuit breaker reset — requests resumed");
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

    /// Create the error to return when circuit breaker is open.
    #[must_use]
    pub fn open_error(&self) -> GatewayError {
        GatewayError::ProviderUnavailable {
            message: format!(
                "Adapter circuit breaker open — {} provider outage detected. Manual reset required.",
                self.provider
            ),
        }
    }
}

impl Default for AdapterCircuitBreaker {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(300), "unknown")
    }
}

/// Point-in-time snapshot of a circuit breaker's state.
///
/// Used by the REST API to report status without holding locks.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct CircuitBreakerSnapshot {
    /// Current state: `"open"` or `"closed"`.
    pub state: String,
    /// Number of recent failures within the time window.
    pub failure_count: usize,
}

impl CircuitBreakerSnapshot {
    /// Create a snapshot from an `AdapterCircuitBreaker`.
    #[must_use]
    pub fn from_adapter(breaker: &AdapterCircuitBreaker) -> Self {
        Self {
            state: match breaker.state() {
                CircuitState::Closed => "closed".to_string(),
                CircuitState::Open => "open".to_string(),
            },
            failure_count: breaker.failure_count(),
        }
    }

    /// Create a default "closed" snapshot (for adapters without a circuit breaker).
    #[must_use]
    pub fn closed() -> Self {
        Self {
            state: "closed".to_string(),
            failure_count: 0,
        }
    }
}

/// Check if an error represents a provider outage that should count toward the circuit breaker.
#[must_use]
pub const fn is_outage_error(error: &GatewayError) -> bool {
    matches!(
        error,
        GatewayError::ProviderUnavailable { .. } | GatewayError::ProviderError { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_starts_closed() {
        let breaker = AdapterCircuitBreaker::new(3, Duration::from_secs(300), "test");
        assert!(!breaker.is_open());
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn breaker_trips_after_threshold() {
        let mut breaker = AdapterCircuitBreaker::new(3, Duration::from_secs(300), "test");

        breaker.record_failure();
        assert!(!breaker.is_open());

        breaker.record_failure();
        assert!(!breaker.is_open());

        breaker.record_failure();
        assert!(breaker.is_open());
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn breaker_reset() {
        let mut breaker = AdapterCircuitBreaker::new(3, Duration::from_secs(300), "test");

        for _ in 0..3 {
            breaker.record_failure();
        }
        assert!(breaker.is_open());

        breaker.reset().expect("reset should succeed");
        assert!(!breaker.is_open());
        assert_eq!(breaker.failure_count(), 0);
    }

    #[test]
    fn outage_error_classification() {
        assert!(is_outage_error(&GatewayError::ProviderUnavailable {
            message: "test".to_string()
        }));

        assert!(is_outage_error(&GatewayError::ProviderError {
            message: "test".to_string(),
            provider: Some("test".to_string()),
            source: None
        }));

        assert!(!is_outage_error(&GatewayError::RateLimited {
            retry_after_seconds: Some(30),
            reset_at: None
        }));

        assert!(!is_outage_error(&GatewayError::OrderRejected {
            reason: "test".to_string(),
            reject_code: None,
            details: None
        }));

        assert!(!is_outage_error(&GatewayError::OrderNotFound {
            id: "test".to_string()
        }));
    }

    #[test]
    fn open_error_message() {
        let breaker = AdapterCircuitBreaker::new(3, Duration::from_secs(300), "alpaca");
        let error = breaker.open_error();

        match error {
            GatewayError::ProviderUnavailable { message } => {
                assert!(message.contains("circuit breaker"));
                assert!(message.contains("alpaca"));
            }
            _ => panic!("Expected ProviderUnavailable error"),
        }
    }

    #[test]
    fn reset_cooldown_rejects_rapid_reset() {
        let mut breaker = AdapterCircuitBreaker::new(2, Duration::from_secs(300), "test");

        // Trip and reset
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.is_open());
        breaker.reset().expect("first reset should succeed");

        // Trip again immediately
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.is_open());

        // Second reset within cooldown should fail
        let result = breaker.reset();
        assert!(result.is_err());
        assert!(breaker.is_open(), "breaker should still be open");
    }
}
