pub mod adapter;
pub mod api;
pub mod circuit_breaker;
pub mod config;
pub mod correlation;
pub mod error;
pub mod events;
pub mod exit_management;
pub mod http;
pub mod metrics;
pub mod models;
pub mod shutdown;
pub mod state;
pub mod subscription;
pub mod telemetry;
pub mod trailing_stop;
pub mod websocket;

pub use adapter::{ProviderCapabilities, TradingAdapter};
pub use api::extractors::{OptionalValidatedJson, ValidatedJson};
pub use api::routes::create_gateway_router;
pub use api::state::GatewayState;
pub use config::GatewayConfig;
pub use error::{GatewayError, GatewayResult};
pub use metrics::MetricsHandle;
pub use models::TradingPlatform;
pub use telemetry::init_tracing;
pub use websocket::provider::{arc_secret, arc_secret_from_string};

/// Internal test helpers — only compiled during `cargo test` for this crate.
///
/// External consumers should use `tektii_gateway_test_support` instead.
#[cfg(test)]
pub(crate) mod test_helpers;
