//! Single-adapter registry for the gateway.
//!
//! Each gateway instance serves exactly one trading provider.

use std::sync::Arc;

use crate::adapter::TradingAdapter;
use crate::models::TradingPlatform;

/// Registry holding the single trading adapter for this gateway instance.
///
/// Constructed at startup with exactly one adapter. Access is infallible —
/// the adapter is guaranteed to exist after construction.
#[derive(Clone)]
pub struct AdapterRegistry {
    adapter: Arc<dyn TradingAdapter>,
    platform: TradingPlatform,
}

impl AdapterRegistry {
    /// Create a new registry with the given adapter.
    #[must_use]
    pub fn new(adapter: Arc<dyn TradingAdapter>, platform: TradingPlatform) -> Self {
        Self { adapter, platform }
    }

    /// Get the trading adapter.
    #[must_use]
    pub fn adapter(&self) -> &Arc<dyn TradingAdapter> {
        &self.adapter
    }

    /// Get the configured trading platform.
    #[must_use]
    pub fn platform(&self) -> TradingPlatform {
        self.platform
    }
}

impl std::fmt::Debug for AdapterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterRegistry")
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}
