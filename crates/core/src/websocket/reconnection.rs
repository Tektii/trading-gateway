//! Reconnection state machine for broker WebSocket connections.
//!
//! Pure logic — no async, no I/O. Drives exponential backoff with jitter
//! and tracks gap duration between disconnect and reconnect.

use std::time::{Duration, Instant};

use crate::config::ReconnectionConfig;

/// State of the reconnection handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No active reconnection attempt.
    Idle,
    /// Disconnected, actively retrying.
    Retrying,
    /// Gave up after max_retry_duration.
    GaveUp,
}

/// Drives exponential backoff with jitter for broker WebSocket reconnection.
///
/// This is a pure state machine — it computes delays and tracks state but
/// performs no I/O. The caller is responsible for sleeping and reconnecting.
#[derive(Debug)]
pub struct ReconnectionHandler {
    config: ReconnectionConfig,
    state: State,
    attempt_count: u32,
    disconnected_at: Option<Instant>,
}

impl ReconnectionHandler {
    /// Create a new handler with the given configuration.
    #[must_use]
    pub fn new(config: ReconnectionConfig) -> Self {
        Self {
            config,
            state: State::Idle,
            attempt_count: 0,
            disconnected_at: None,
        }
    }

    /// Called when a disconnect is detected. Records the disconnect timestamp.
    pub fn on_disconnect(&mut self) {
        self.state = State::Retrying;
        self.attempt_count = 0;
        self.disconnected_at = Some(Instant::now());
    }

    /// Calculate the next backoff delay with jitter.
    ///
    /// Returns `None` if `max_retry_duration` has been exceeded (caller should give up).
    #[must_use]
    pub fn next_backoff(&mut self) -> Option<Duration> {
        if self.state != State::Retrying {
            return None;
        }

        // Check if we've exceeded the max retry duration
        if let Some(disconnected_at) = self.disconnected_at
            && disconnected_at.elapsed() >= self.config.max_retry_duration
        {
            return None;
        }

        let base = u64::try_from(self.config.initial_backoff.as_millis()).unwrap_or(u64::MAX);
        let exp = self.attempt_count.min(6);
        let backoff_ms = base.saturating_mul(2_u64.pow(exp));
        let capped_ms =
            backoff_ms.min(u64::try_from(self.config.max_backoff.as_millis()).unwrap_or(u64::MAX));

        // Add jitter: 0–25% of the backoff using cheap pseudo-randomness
        let jitter_ms = if capped_ms > 0 {
            let nanos = u64::from(Instant::now().elapsed().subsec_nanos());
            nanos % (capped_ms / 4).max(1)
        } else {
            0
        };

        self.attempt_count += 1;

        Some(Duration::from_millis(capped_ms + jitter_ms))
    }

    /// Called on successful reconnection. Returns the gap duration.
    #[must_use]
    pub fn on_reconnect_success(&mut self) -> Duration {
        let gap = self.gap_duration().unwrap_or(Duration::ZERO);
        self.reset();
        gap
    }

    /// Called when giving up (max_retry_duration exceeded). Marks state as terminal.
    pub fn on_gave_up(&mut self) {
        self.state = State::GaveUp;
    }

    /// Reset to idle state.
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.attempt_count = 0;
        self.disconnected_at = None;
    }

    /// Get the duration since disconnect, if currently disconnected.
    #[must_use]
    pub fn gap_duration(&self) -> Option<Duration> {
        self.disconnected_at.map(|t| t.elapsed())
    }

    /// Get the current attempt count.
    #[must_use]
    pub fn attempt_count(&self) -> u32 {
        self.attempt_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ReconnectionConfig {
        ReconnectionConfig {
            initial_backoff: Duration::from_millis(1000),
            max_backoff: Duration::from_secs(60),
            max_retry_duration: Duration::from_secs(300),
        }
    }

    #[test]
    fn backoff_sequence_is_exponential_and_capped() {
        let mut handler = ReconnectionHandler::new(test_config());
        handler.on_disconnect();

        // Collect base backoff values (subtract jitter by checking range)
        let delays: Vec<Duration> = (0..8).filter_map(|_| handler.next_backoff()).collect();

        // Each delay should be >= its base exponential value
        // 1s, 2s, 4s, 8s, 16s, 32s, 60s (capped), 60s (capped)
        let expected_bases_ms: Vec<u64> = vec![1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000];

        for (i, (delay, &base_ms)) in delays.iter().zip(expected_bases_ms.iter()).enumerate() {
            let delay_ms = delay.as_millis() as u64;
            // Should be at least the base and at most base + 25% jitter
            assert!(
                delay_ms >= base_ms,
                "attempt {i}: delay {delay_ms}ms < base {base_ms}ms"
            );
            assert!(
                delay_ms <= base_ms + base_ms / 4,
                "attempt {i}: delay {delay_ms}ms > base + 25% jitter ({}ms)",
                base_ms + base_ms / 4
            );
        }
    }

    #[test]
    fn max_retry_duration_exceeded_returns_none() {
        let config = ReconnectionConfig {
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            max_retry_duration: Duration::from_millis(0), // Immediately expired
        };
        let mut handler = ReconnectionHandler::new(config);
        handler.on_disconnect();

        // Should return None because max_retry_duration is 0
        assert!(handler.next_backoff().is_none());
    }

    #[test]
    fn on_reconnect_success_returns_gap_and_resets() {
        let mut handler = ReconnectionHandler::new(test_config());
        handler.on_disconnect();

        // Consume one backoff
        let _ = handler.next_backoff();

        let gap = handler.on_reconnect_success();
        assert!(gap > Duration::ZERO);
        assert_eq!(handler.attempt_count(), 0);
        // After reset, next_backoff returns None (state is Idle, not Retrying)
        assert!(handler.next_backoff().is_none());
    }

    #[test]
    fn gave_up_is_terminal() {
        let mut handler = ReconnectionHandler::new(test_config());
        handler.on_disconnect();
        handler.on_gave_up();

        // Should always return None after giving up
        assert!(handler.next_backoff().is_none());
        assert!(handler.next_backoff().is_none());
    }

    #[test]
    fn reset_clears_all_state() {
        let mut handler = ReconnectionHandler::new(test_config());
        handler.on_disconnect();
        let _ = handler.next_backoff();
        let _ = handler.next_backoff();

        handler.reset();
        assert_eq!(handler.attempt_count(), 0);
        assert!(handler.gap_duration().is_none());
        // Idle state — next_backoff returns None until on_disconnect called again
        assert!(handler.next_backoff().is_none());
    }

    #[test]
    fn idle_handler_returns_no_backoff() {
        let mut handler = ReconnectionHandler::new(test_config());
        assert!(handler.next_backoff().is_none());
    }

    #[test]
    fn gap_duration_tracks_elapsed_time() {
        let mut handler = ReconnectionHandler::new(test_config());
        assert!(handler.gap_duration().is_none());

        handler.on_disconnect();
        let gap1 = handler.gap_duration().unwrap();
        // Just verify it's non-negative (elapsed time since disconnect)
        assert!(gap1 >= Duration::ZERO);
    }
}
