use std::sync::Arc;

use axum::extract::State;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

/// Handle for accessing Prometheus metrics output.
///
/// Wrap in `Arc` or clone freely — the inner handle is already reference-counted.
#[derive(Clone)]
pub struct MetricsHandle {
    handle: PrometheusHandle,
}

impl MetricsHandle {
    /// Default buckets for HTTP request duration histogram (1 ms → 10 s).
    const HTTP_DURATION_BUCKETS: &'static [f64] = &[
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    /// Buckets for order submission round-trip latency (1 ms → 10 s).
    const ORDER_SUBMIT_DURATION_BUCKETS: &'static [f64] = &[
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    /// Initializes the Prometheus metrics recorder.
    ///
    /// # Panics
    ///
    /// Panics if a recorder is already installed.
    #[must_use]
    pub fn init() -> Self {
        let builder = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Prefix("http_request_duration".to_string()),
                Self::HTTP_DURATION_BUCKETS,
            )
            .expect("Failed to set buckets for http_request_duration")
            .set_buckets_for_metric(
                Matcher::Prefix("gateway_order_submit_duration".to_string()),
                Self::ORDER_SUBMIT_DURATION_BUCKETS,
            )
            .expect("Failed to set buckets for gateway_order_submit_duration");

        let handle = builder
            .install_recorder()
            .expect("Failed to install Prometheus metrics recorder");

        Self { handle }
    }

    /// Non-panicking variant — returns `None` if a recorder is already installed.
    #[must_use]
    pub fn try_init() -> Option<Self> {
        let builder = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Prefix("http_request_duration".to_string()),
                Self::HTTP_DURATION_BUCKETS,
            )
            .ok()?
            .set_buckets_for_metric(
                Matcher::Prefix("gateway_order_submit_duration".to_string()),
                Self::ORDER_SUBMIT_DURATION_BUCKETS,
            )
            .ok()?;

        builder
            .install_recorder()
            .ok()
            .map(|handle| Self { handle })
    }

    /// Renders current metrics in Prometheus text exposition format.
    #[must_use]
    pub fn render(&self) -> String {
        self.handle.render()
    }
}

/// Axum handler that renders metrics for a `/metrics` endpoint.
#[allow(clippy::unused_async)] // must be async for axum handler signature
pub async fn metrics_handler(State(handle): State<Arc<MetricsHandle>>) -> String {
    handle.render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_handle_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<MetricsHandle>();
    }

    #[test]
    fn metrics_render_contains_recorded_metrics() {
        // try_init returns None if another test already installed a recorder
        let Some(handle) = MetricsHandle::try_init() else {
            return;
        };

        metrics::counter!(
            "gateway_orders_submitted_total",
            "platform" => "test",
        )
        .increment(1);

        metrics::gauge!("gateway_ws_connections_active").increment(1.0);

        metrics::histogram!(
            "gateway_order_submit_duration_seconds",
            "platform" => "test",
        )
        .record(0.042);

        metrics::counter!(
            "gateway_order_events_total",
            "platform" => "test",
            "event_type" => "filled",
        )
        .increment(1);

        let output = handle.render();
        assert!(
            output.contains("gateway_orders_submitted_total"),
            "Missing gateway_orders_submitted_total in:\n{output}"
        );
        assert!(
            output.contains("gateway_ws_connections_active"),
            "Missing gateway_ws_connections_active in:\n{output}"
        );
        assert!(
            output.contains("gateway_order_submit_duration_seconds"),
            "Missing gateway_order_submit_duration_seconds in:\n{output}"
        );
        assert!(
            output.contains("gateway_order_events_total"),
            "Missing gateway_order_events_total in:\n{output}"
        );
    }
}
