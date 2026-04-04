//! Binance COIN-M Futures provider capabilities.

use tektii_gateway_core::adapter::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::models::{
    AssetClass, Capabilities, OrderRequest, OrderType, PositionMode, RateLimits,
};

#[derive(Debug, Default, Clone)]
pub struct BinanceCoinFuturesCapabilities;

impl BinanceCoinFuturesCapabilities {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ProviderCapabilities for BinanceCoinFuturesCapabilities {
    fn supports_bracket_orders(&self, _symbol: &str) -> bool {
        false
    }
    fn supports_oco_orders(&self, _symbol: &str) -> bool {
        true
    }
    fn supports_oto_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn bracket_strategy(&self, order: &OrderRequest) -> BracketStrategy {
        if order.stop_loss.is_none() && order.take_profit.is_none() {
            return BracketStrategy::None;
        }
        let has_both = order.stop_loss.is_some() && order.take_profit.is_some();
        if has_both && self.supports_oco_orders(&order.symbol) {
            return BracketStrategy::NativeOco;
        }
        BracketStrategy::PendingSlTp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![AssetClass::Futures],
            supported_order_types: vec![
                OrderType::Market,
                OrderType::Limit,
                OrderType::Stop,
                OrderType::StopLimit,
                OrderType::TrailingStop,
            ],
            position_mode: PositionMode::Hedging,
            features: vec![
                "oco".to_string(),
                "reduce_only".to_string(),
                "isolated_margin".to_string(),
                "cross_margin".to_string(),
                "post_only".to_string(),
                "get_quote".to_string(),
                "trailing_stop".to_string(),
                "leverage".to_string(),
                "short_selling".to_string(),
            ],
            max_leverage: Some(rust_decimal::Decimal::from(125)),
            rate_limits: Some(RateLimits {
                requests_per_minute: Some(2400),
                orders_per_second: Some(30),
                orders_per_day: Some(200_000),
            }),
        }
    }
}
