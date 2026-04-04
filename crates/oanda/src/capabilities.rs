//! Oanda provider capability declarations.
//!
//! This module provides an implementation of `ProviderCapabilities`
//! for Oanda, declaring what order types are supported natively.

use tektii_gateway_core::adapter::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::models::{
    AssetClass, Capabilities, OrderRequest, OrderType, PositionMode, RateLimits,
};

// ============================================================================
// Oanda Provider Capabilities
// ============================================================================

/// Oanda provider capabilities for forex and CFD trading.
///
/// Oanda has the best native bracket order support of any integrated broker:
/// - Bracket orders: Supported for ALL instruments via `onFill` parameters
/// - OCO orders: Not supported as a standalone concept
/// - OTO orders: Effectively supported via `onFill` (SL/TP created on fill)
/// - Trailing stops: Supported natively (dependent orders on trades)
///
/// Position mode is detected from the account API (`hedgingEnabled` field).
/// Defaults to Netting when not yet detected.
#[derive(Debug, Clone, Default)]
pub struct OandaCapabilities {
    /// Whether the Oanda account uses hedging mode.
    hedging_enabled: bool,
}

impl OandaCapabilities {
    /// Create a new instance of Oanda capabilities (defaults to netting mode).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hedging_enabled: false,
        }
    }

    /// Create capabilities with explicit hedging mode.
    #[must_use]
    pub const fn new_with_hedging(hedging_enabled: bool) -> Self {
        Self { hedging_enabled }
    }
}

impl ProviderCapabilities for OandaCapabilities {
    fn supports_bracket_orders(&self, _symbol: &str) -> bool {
        // Oanda supports native onFill SL/TP for ALL instruments
        true
    }

    fn supports_oco_orders(&self, _symbol: &str) -> bool {
        // No standalone OCO concept -- bracket handles SL+TP
        false
    }

    fn supports_oto_orders(&self, _symbol: &str) -> bool {
        // onFill is essentially OTO (create exit orders when entry fills)
        true
    }

    fn bracket_strategy(&self, order: &OrderRequest) -> BracketStrategy {
        // No SL/TP requested - nothing to handle
        if order.stop_loss.is_none() && order.take_profit.is_none() {
            return BracketStrategy::None;
        }

        // Oanda supports native bracket for all instruments and any
        // combination of SL only, TP only, or both
        BracketStrategy::NativeBracket
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![AssetClass::Forex],
            supported_order_types: vec![
                OrderType::Market,
                OrderType::Limit,
                OrderType::Stop,
                OrderType::StopLimit,
                // TrailingStop excluded -- Oanda trailing stops are dependent
                // orders (must attach to trade), not standalone entry orders.
                // Returning UnsupportedOperation for standalone trailing stop.
            ],
            position_mode: if self.hedging_enabled {
                PositionMode::Hedging
            } else {
                PositionMode::Netting
            },
            features: vec![
                "bracket_orders".to_string(),
                "trailing_stop".to_string(), // Dependent trailing stops on trades
                "get_quote".to_string(),
                "short_selling".to_string(),
            ],
            max_leverage: Some(rust_decimal::Decimal::from(50)), // Varies by jurisdiction
            rate_limits: Some(RateLimits {
                requests_per_minute: Some(7200), // 120/sec * 60
                orders_per_second: None,
                orders_per_day: None,
            }),
        }
    }
}
