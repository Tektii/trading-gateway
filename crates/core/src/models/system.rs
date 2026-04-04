//! System models for connection status, health checks, and provider capabilities.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{AssetClass, OrderType, PositionMode};

/// Health check response for liveness probes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct HealthStatus {
    /// Health status (always "ok" if responding)
    pub status: String,
}

impl HealthStatus {
    /// Create a healthy status response.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            status: "ok".to_string(),
        }
    }
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self::ok()
    }
}

/// Readiness check response for readiness probes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct ReadyStatus {
    /// Whether the service is ready to accept requests
    pub ready: bool,

    /// List of registered provider names
    pub providers: Vec<String>,

    /// Number of registered providers
    pub provider_count: usize,

    /// Optional message explaining non-ready state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ReadyStatus {
    /// Create a ready status with the given providers.
    #[must_use]
    pub const fn ready(providers: Vec<String>) -> Self {
        let provider_count = providers.len();
        Self {
            ready: true,
            providers,
            provider_count,
            message: None,
        }
    }

    /// Create a not-ready status with the given reason.
    #[must_use]
    pub fn not_ready(reason: impl Into<String>) -> Self {
        Self {
            ready: false,
            providers: vec![],
            provider_count: 0,
            message: Some(reason.into()),
        }
    }
}

/// Connection status to the trading provider.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct ConnectionStatus {
    /// Whether the connection to the provider is active
    pub connected: bool,

    /// Round-trip latency in milliseconds
    pub latency_ms: u32,

    /// Timestamp of last successful heartbeat/ping
    pub last_heartbeat: DateTime<Utc>,
}

/// Rate limit information for a trading provider.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct RateLimits {
    /// Maximum API requests per minute
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,

    /// Maximum order submissions per second
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orders_per_second: Option<u32>,

    /// Maximum orders per day
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orders_per_day: Option<u32>,
}

/// Provider capabilities describing supported features.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Capabilities {
    /// Asset classes supported by this provider
    pub supported_asset_classes: Vec<AssetClass>,

    /// Order types supported by this provider
    pub supported_order_types: Vec<OrderType>,

    /// Position mode (netting vs hedging)
    pub position_mode: PositionMode,

    /// List of supported optional features.
    pub features: Vec<String>,

    /// Maximum leverage multiplier (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_leverage: Option<Decimal>,

    /// Provider rate limits (None means unlimited, e.g., Tektii engine)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<RateLimits>,
}

/// Overall gateway health status for detailed health endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverallStatus {
    /// All providers connected with fresh data
    Connected,
    /// One or more providers have stale data (e.g., during reconnection)
    Degraded,
}

/// Per-provider health detail.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct ProviderHealth {
    /// Platform identifier
    pub platform: String,
    /// Whether all instrument data is fresh
    pub connected: bool,
    /// Instruments with stale data (empty when healthy)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_instruments: Vec<String>,
}

/// Detailed health response for the `/health` endpoint.
///
/// Always returned with 200 status — degraded is informational,
/// not an error. This prevents k8s from restarting the pod on a
/// transient broker blip.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct DetailedHealthStatus {
    /// Overall status: "connected" or "degraded"
    pub status: OverallStatus,
    /// Per-provider health details
    pub providers: Vec<ProviderHealth>,
}

impl Capabilities {
    /// Check if a specific feature is supported (case-insensitive).
    #[must_use]
    pub fn supports_feature(&self, feature: &str) -> bool {
        let feature_lower = feature.to_lowercase();
        self.features
            .iter()
            .any(|f| f.to_lowercase() == feature_lower)
    }

    /// Check if a specific order type is supported.
    #[must_use]
    pub fn supports_order_type(&self, order_type: OrderType) -> bool {
        self.supported_order_types.contains(&order_type)
    }

    /// Check if a specific asset class is supported.
    #[must_use]
    pub fn supports_asset_class(&self, asset_class: AssetClass) -> bool {
        self.supported_asset_classes.contains(&asset_class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detailed_health_serializes_connected() {
        let health = DetailedHealthStatus {
            status: OverallStatus::Connected,
            providers: vec![ProviderHealth {
                platform: "alpaca-paper".to_string(),
                connected: true,
                stale_instruments: vec![],
            }],
        };

        let json = serde_json::to_value(&health).unwrap();
        assert_eq!(json["status"], "connected");
        assert_eq!(json["providers"][0]["platform"], "alpaca-paper");
        assert_eq!(json["providers"][0]["connected"], true);
        // stale_instruments should be skipped when empty
        assert!(json["providers"][0].get("stale_instruments").is_none());
    }

    #[test]
    fn detailed_health_serializes_degraded() {
        let health = DetailedHealthStatus {
            status: OverallStatus::Degraded,
            providers: vec![ProviderHealth {
                platform: "alpaca-paper".to_string(),
                connected: false,
                stale_instruments: vec!["AAPL".to_string(), "MSFT".to_string()],
            }],
        };

        let json = serde_json::to_value(&health).unwrap();
        assert_eq!(json["status"], "degraded");
        assert_eq!(json["providers"][0]["connected"], false);
        assert_eq!(json["providers"][0]["stale_instruments"][0], "AAPL");
        assert_eq!(json["providers"][0]["stale_instruments"][1], "MSFT");
    }
}
