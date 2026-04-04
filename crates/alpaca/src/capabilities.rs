//! Alpaca provider capability declarations.
//!
//! This module provides an implementation of `ProviderCapabilities`
//! for Alpaca, declaring what order types are supported natively.

use tektii_gateway_core::adapter::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::models::{
    AssetClass, Capabilities, OrderRequest, OrderType, PositionMode, RateLimits,
};

// ============================================================================
// Symbol Classification Utilities
// ============================================================================

/// Check if a symbol represents a cryptocurrency.
///
/// Determines asset class from the symbol string by checking common crypto
/// quote currencies and separators used by Alpaca.
///
/// # Arguments
///
/// * `symbol` - The trading symbol
///
/// # Returns
///
/// `true` if the symbol appears to be a cryptocurrency, `false` otherwise.
///
/// # Examples
///
/// ```ignore
/// assert!(is_crypto_symbol("BTCUSDT"));
/// assert!(is_crypto_symbol("BTC/USD"));
/// assert!(!is_crypto_symbol("AAPL"));
/// ```
#[must_use]
pub fn is_crypto_symbol(symbol: &str) -> bool {
    let symbol_upper = symbol.to_uppercase();
    symbol_upper.ends_with("USD")
        || symbol_upper.ends_with("USDT")
        || symbol_upper.ends_with("USDC")
        || symbol_upper.ends_with("BTC")
        || symbol_upper.ends_with("ETH")
        || symbol_upper.contains("/USD")
        || symbol_upper.contains("/BTC")
        || symbol_upper.contains("/ETH")
}

// ============================================================================
// Alpaca Provider Capabilities
// ============================================================================

/// Alpaca provider capabilities.
///
/// Alpaca supports bracket orders for stocks but NOT for crypto.
/// - Stocks: Full bracket order support (primary + SL + TP)
/// - Crypto: No bracket orders, requires pending SL/TP system
#[derive(Debug, Default, Clone)]
pub struct AlpacaCapabilities;

impl AlpacaCapabilities {
    /// Create a new instance of Alpaca capabilities.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ProviderCapabilities for AlpacaCapabilities {
    fn supports_bracket_orders(&self, symbol: &str) -> bool {
        // Alpaca supports bracket orders ONLY for stocks, not crypto
        !is_crypto_symbol(symbol)
    }

    fn supports_oco_orders(&self, symbol: &str) -> bool {
        // Alpaca supports OCO for stocks only (same as bracket)
        !is_crypto_symbol(symbol)
    }

    fn supports_oto_orders(&self, symbol: &str) -> bool {
        // Alpaca supports OTO for stocks only (same as bracket)
        !is_crypto_symbol(symbol)
    }

    fn bracket_strategy(&self, order: &OrderRequest) -> BracketStrategy {
        // No SL/TP requested - nothing to handle
        if order.stop_loss.is_none() && order.take_profit.is_none() {
            return BracketStrategy::None;
        }

        // Priority 1: Native bracket orders (Alpaca stocks)
        // Works with SL only, TP only, or both
        if self.supports_bracket_orders(&order.symbol) {
            return BracketStrategy::NativeBracket;
        }

        // Priority 2: Native OCO (requires BOTH SL and TP)
        let has_both = order.stop_loss.is_some() && order.take_profit.is_some();
        if has_both && self.supports_oco_orders(&order.symbol) {
            return BracketStrategy::NativeOco;
        }

        // Priority 3: Pending SL/TP system (universal fallback)
        BracketStrategy::PendingSlTp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![AssetClass::Stock, AssetClass::Crypto],
            supported_order_types: vec![
                OrderType::Market,
                OrderType::Limit,
                OrderType::Stop,
                OrderType::StopLimit,
                OrderType::TrailingStop,
            ],
            position_mode: PositionMode::Netting,
            features: vec![
                "bracket_orders".to_string(), // For stocks only
                "oco".to_string(),            // For stocks only
                "trailing_stop".to_string(),
                "hidden_orders".to_string(),
                "iceberg_orders".to_string(),
                "post_only".to_string(),
                "get_quote".to_string(),
                "short_selling".to_string(),
            ],
            max_leverage: None,
            rate_limits: Some(RateLimits {
                requests_per_minute: Some(200),
                orders_per_second: Some(10),
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

    // is_crypto_symbol tests

    #[test]
    fn is_crypto_true_for_usdt_suffix() {
        assert!(is_crypto_symbol("BTCUSDT"));
        assert!(is_crypto_symbol("ETHUSDT"));
    }

    #[test]
    fn is_crypto_true_for_slash_usd() {
        assert!(is_crypto_symbol("BTC/USD"));
        assert!(is_crypto_symbol("ETH/USD"));
    }

    #[test]
    fn is_crypto_false_for_plain_stock_ticker() {
        assert!(!is_crypto_symbol("AAPL"));
        assert!(!is_crypto_symbol("MSFT"));
        assert!(!is_crypto_symbol("GOOG"));
    }

    // bracket_strategy tests

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
    fn alpaca_bracket_returns_native_bracket_for_stock_with_sl_tp() {
        let caps = AlpacaCapabilities::new();
        let order = order_request("AAPL", Some(dec!(140)), Some(dec!(160)));
        assert_eq!(
            caps.bracket_strategy(&order),
            BracketStrategy::NativeBracket
        );
    }

    #[test]
    fn alpaca_bracket_returns_pending_for_crypto_with_sl_only() {
        let caps = AlpacaCapabilities::new();
        let order = order_request("BTCUSDT", Some(dec!(45000)), None);
        assert_eq!(caps.bracket_strategy(&order), BracketStrategy::PendingSlTp);
    }

    // short_selling feature tests

    #[test]
    fn alpaca_supports_short_selling() {
        let caps = AlpacaCapabilities::new().capabilities();
        assert!(caps.supports_feature("short_selling"));
    }
}
