//! Tektii engine provider capabilities.
//!
//! The engine supports a minimal subset of order types:
//! - Order types: Market, Limit, Stop
//! - No bracket/OCO/OTO orders (use gateway's pending SL/TP system)
//! - Hedging position mode (multiple positions per symbol)

use tektii_gateway_core::adapter::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::models::{
    AssetClass, Capabilities, OrderRequest, OrderType, PositionMode,
};

/// Tektii engine provider capabilities.
#[derive(Debug, Default, Clone)]
pub struct TektiiCapabilities;

impl TektiiCapabilities {
    /// Create a new instance of Tektii capabilities.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ProviderCapabilities for TektiiCapabilities {
    fn supports_bracket_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn supports_oco_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn supports_oto_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn bracket_strategy(&self, order: &OrderRequest) -> BracketStrategy {
        // No SL/TP requested - nothing to handle
        if order.stop_loss.is_none() && order.take_profit.is_none() {
            return BracketStrategy::None;
        }

        // Engine doesn't support any native bracket/OCO - always use pending system
        BracketStrategy::PendingSlTp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![AssetClass::Stock, AssetClass::Crypto],
            supported_order_types: vec![OrderType::Market, OrderType::Limit, OrderType::Stop],
            position_mode: PositionMode::Hedging,
            features: vec![
                "hedging".to_string(),
                "reduce_only".to_string(),
                "short_selling".to_string(),
                "leverage".to_string(),
            ],
            max_leverage: None,
            rate_limits: None,
        }
    }
}
