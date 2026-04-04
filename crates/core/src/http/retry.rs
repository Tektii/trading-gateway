//! Retry logic with exponential backoff for transient failures.

use std::future::Future;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use crate::error::GatewayError;

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (not including the initial attempt).
    pub max_retries: u32,
    /// Initial delay between retries.
    pub initial_delay: Duration,
    /// Maximum delay between retries.
    pub max_delay: Duration,
    /// Multiplier for exponential backoff.
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Create a new retry configuration.
    #[must_use]
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            ..Default::default()
        }
    }

    /// Create configuration optimized for tests (faster).
    #[must_use]
    pub const fn for_tests() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
            backoff_multiplier: 2.0,
        }
    }

    /// Calculate delay for the given retry attempt.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base_delay = self.initial_delay.as_millis() as f64;
        let multiplier = self.backoff_multiplier.powi(attempt as i32);
        let delay_ms = (base_delay * multiplier) as u64;
        let delay = Duration::from_millis(delay_ms);

        std::cmp::min(delay, self.max_delay)
    }
}

/// Execute an async operation with retry on transient failures.
///
/// Retries with exponential backoff when the operation returns a retryable error
/// (503, 502, rate limit, timeouts). Respects `retry_after_seconds` from rate
/// limit responses.
pub async fn retry_with_backoff<F, Fut, T>(
    config: &RetryConfig,
    operation: F,
) -> Result<T, GatewayError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, GatewayError>>,
{
    let mut last_error = None;

    for attempt in 0..=config.max_retries {
        match operation().await {
            Ok(result) => {
                if attempt > 0 {
                    debug!(
                        attempt = attempt + 1,
                        total_attempts = config.max_retries + 1,
                        "Request succeeded after retry"
                    );
                }
                return Ok(result);
            }
            Err(err) => {
                let is_retryable = err.is_retryable();
                let is_last_attempt = attempt >= config.max_retries;

                if is_retryable && !is_last_attempt {
                    let delay = if let GatewayError::RateLimited {
                        retry_after_seconds: Some(seconds),
                        ..
                    } = &err
                    {
                        let suggested = Duration::from_secs(*seconds);
                        std::cmp::min(suggested, config.max_delay)
                    } else {
                        config.delay_for_attempt(attempt)
                    };

                    warn!(
                        attempt = attempt + 1,
                        max_attempts = config.max_retries + 1,
                        delay_ms = delay.as_millis(),
                        error = %err,
                        "Transient error, retrying"
                    );

                    sleep(delay).await;
                    last_error = Some(err);
                } else {
                    if is_retryable {
                        warn!(
                            attempts = attempt + 1,
                            error = %err,
                            "All retries exhausted"
                        );
                    } else {
                        debug!(error = %err, "Non-retryable error, not retrying");
                    }
                    return Err(err);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| GatewayError::Internal {
        message: "Retry logic error".to_string(),
        source: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn succeeds_on_first_attempt() {
        let config = RetryConfig::for_tests();
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok::<_, GatewayError>(42)
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn succeeds_after_transient_error() {
        let config = RetryConfig::for_tests();
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(GatewayError::ProviderUnavailable {
                        message: "Temporary".to_string(),
                    })
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exhausts_attempts_on_persistent_error() {
        let config = RetryConfig::for_tests();
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(GatewayError::ProviderUnavailable {
                    message: "Down".to_string(),
                })
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), config.max_retries + 1);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_error() {
        let config = RetryConfig::for_tests();
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(GatewayError::OrderNotFound {
                    id: "xyz".to_string(),
                })
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn delay_increases_exponentially() {
        let config = RetryConfig {
            max_retries: 5,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 2.0,
        };

        assert_eq!(config.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(config.delay_for_attempt(1), Duration::from_millis(200));
        assert_eq!(config.delay_for_attempt(2), Duration::from_millis(400));
        assert_eq!(config.delay_for_attempt(3), Duration::from_millis(800));
    }

    #[test]
    fn delay_capped_at_max() {
        let config = RetryConfig {
            max_retries: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            backoff_multiplier: 2.0,
        };

        assert_eq!(config.delay_for_attempt(3), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn rate_limited_is_retried_and_succeeds() {
        let config = RetryConfig::for_tests();
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(GatewayError::RateLimited {
                        retry_after_seconds: Some(0),
                        reset_at: None,
                    })
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rate_limited_without_retry_after_uses_backoff() {
        let config = RetryConfig::for_tests();
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(GatewayError::RateLimited {
                        retry_after_seconds: None,
                        reset_at: None,
                    })
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rate_limited_exhausts_all_retries() {
        let config = RetryConfig::for_tests();
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(GatewayError::RateLimited {
                    retry_after_seconds: Some(0),
                    reset_at: None,
                })
            }
        })
        .await;

        assert!(matches!(result, Err(GatewayError::RateLimited { .. })));
        assert_eq!(call_count.load(Ordering::SeqCst), config.max_retries + 1);
    }

    #[tokio::test]
    async fn rate_limited_retry_after_capped_by_max_delay() {
        let config = RetryConfig {
            max_retries: 1,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            backoff_multiplier: 2.0,
        };
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&call_count);

        let start = tokio::time::Instant::now();
        let result = retry_with_backoff(&config, || {
            let count = Arc::clone(&cc);
            async move {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(GatewayError::RateLimited {
                        retry_after_seconds: Some(999),
                        reset_at: None,
                    })
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        let elapsed = start.elapsed();
        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        // 999s was capped to 50ms max_delay — must complete in well under 2s
        assert!(elapsed < Duration::from_secs(2), "elapsed: {elapsed:?}");
    }
}
