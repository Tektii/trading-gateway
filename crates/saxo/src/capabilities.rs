//! Saxo Bank provider capability declarations.
//!
//! This module provides an implementation of `ProviderCapabilities`
//! for Saxo, declaring what order types are supported natively.

use tektii_gateway_core::adapter::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::models::{
    AssetClass, Capabilities, OrderRequest, OrderType, PositionMode, RateLimits,
};

/// Saxo Bank provider capabilities for forex and CFD trading.
///
/// Saxo has strong native bracket order support via related orders:
/// - Bracket orders: Supported for ALL instruments via entry + SL + TP related orders
/// - OCO orders: Supported (two entry orders, one cancels other)
/// - OTO orders: Supported via `IfDoneMaster`/Slave related orders
/// - Trailing stops: Supported natively
///
/// Position mode is Netting (one position per instrument).
#[derive(Debug, Default, Clone)]
pub struct SaxoCapabilities;

impl SaxoCapabilities {
    /// Create a new instance of Saxo capabilities.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ProviderCapabilities for SaxoCapabilities {
    fn supports_bracket_orders(&self, _symbol: &str) -> bool {
        // Saxo supports native related orders (entry + SL + TP) for ALL instruments
        true
    }

    fn supports_oco_orders(&self, _symbol: &str) -> bool {
        // Saxo supports OCO (two entry orders, one cancels other)
        true
    }

    fn supports_oto_orders(&self, _symbol: &str) -> bool {
        // Saxo supports OTO via IfDoneMaster/Slave related orders
        true
    }

    fn bracket_strategy(&self, order: &OrderRequest) -> BracketStrategy {
        // No SL/TP requested - nothing to handle
        if order.stop_loss.is_none() && order.take_profit.is_none() {
            return BracketStrategy::None;
        }

        // Saxo supports native bracket for all instruments and any
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
                OrderType::TrailingStop,
            ],
            position_mode: PositionMode::Netting,
            features: vec![
                "bracket_orders".to_string(),
                "oco".to_string(),
                "trailing_stop".to_string(),
                "get_quote".to_string(),
                "short_selling".to_string(),
            ],
            max_leverage: Some(rust_decimal::Decimal::from(30)), // ESMA retail default
            rate_limits: Some(RateLimits {
                requests_per_minute: Some(120),
                orders_per_second: Some(1),
                orders_per_day: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use tektii_gateway_core::models::{OrderType, Side, TimeInForce};

    fn order_request(
        symbol: &str,
        sl: Option<rust_decimal::Decimal>,
        tp: Option<rust_decimal::Decimal>,
    ) -> OrderRequest {
        OrderRequest {
            symbol: symbol.to_string(),
            side: Side::Buy,
            quantity: dec!(1),
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Gtc,
            limit_price: None,
            stop_price: None,
            stop_loss: sl,
            take_profit: tp,
            trailing_distance: None,
            trailing_type: None,
            client_order_id: None,
            position_id: None,
            display_quantity: None,
            margin_mode: None,
            leverage: None,
            oco_group_id: None,
            hidden: false,
            post_only: false,
            reduce_only: false,
        }
    }

    #[test]
    fn saxo_bracket_returns_native_bracket_for_any_symbol_with_sl_tp() {
        let caps = SaxoCapabilities::new();
        let order = order_request("EURUSD:FxSpot", Some(dec!(1.08)), Some(dec!(1.10)));
        assert_eq!(
            caps.bracket_strategy(&order),
            BracketStrategy::NativeBracket
        );
    }

    #[test]
    fn saxo_bracket_returns_none_without_sl_tp() {
        let caps = SaxoCapabilities::new();
        let order = order_request("EURUSD:FxSpot", None, None);
        assert_eq!(caps.bracket_strategy(&order), BracketStrategy::None);
    }

    #[test]
    fn saxo_supports_short_selling() {
        let caps = SaxoCapabilities::new().capabilities();
        assert!(caps.supports_feature("short_selling"));
    }

    #[test]
    fn saxo_supports_bracket_orders_all_symbols() {
        let caps = SaxoCapabilities::new();
        assert!(caps.supports_bracket_orders("EURUSD:FxSpot"));
        assert!(caps.supports_bracket_orders("US500:CfdOnIndex"));
        assert!(caps.supports_bracket_orders("AAPL:CfdOnStock"));
    }
}
