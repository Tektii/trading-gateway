//! Binance Margin provider capabilities (Cross/Isolated Margin).

use tektii_gateway_core::adapter::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::models::{
    AssetClass, Capabilities, OrderRequest, OrderType, PositionMode, RateLimits,
};

#[derive(Debug, Default, Clone)]
pub struct BinanceMarginCapabilities;

impl BinanceMarginCapabilities {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ProviderCapabilities for BinanceMarginCapabilities {
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
        if order.stop_loss.is_none() && order.take_profit.is_none() {
            return BracketStrategy::None;
        }
        BracketStrategy::PendingSlTp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![AssetClass::Crypto],
            supported_order_types: vec![
                OrderType::Market,
                OrderType::Limit,
                OrderType::Stop,
                OrderType::StopLimit,
                OrderType::TrailingStop,
            ],
            position_mode: PositionMode::Netting,
            features: vec![
                "isolated_margin".to_string(),
                "cross_margin".to_string(),
                "post_only".to_string(),
                "get_quote".to_string(),
                "borrow".to_string(),
                "trailing_stop".to_string(),
                "short_selling".to_string(),
            ],
            max_leverage: Some(rust_decimal::Decimal::from(10)),
            rate_limits: Some(RateLimits {
                requests_per_minute: Some(6000),
                orders_per_second: Some(10),
                orders_per_day: Some(200_000),
            }),
        }
    }
}
