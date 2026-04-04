//! Binance Margin-specific request and response types.

use serde::{Deserialize, Serialize};

#[allow(dead_code)]
pub const BINANCE_MARGIN_BASE_URL: &str = "https://api.binance.com";
#[allow(dead_code)]
pub const BINANCE_MARGIN_TESTNET_URL: &str = "https://testnet.binance.vision";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceMarginAccount {
    #[allow(dead_code)]
    pub borrow_enabled: bool,
    #[allow(dead_code)]
    pub margin_level: String,
    #[allow(dead_code)]
    pub total_asset_of_btc: String,
    #[allow(dead_code)]
    pub total_liability_of_btc: String,
    pub total_net_asset_of_btc: String,
    #[allow(dead_code)]
    pub trade_enabled: bool,
    #[allow(dead_code)]
    pub transfer_enabled: bool,
    pub user_assets: Vec<BinanceMarginAsset>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceMarginAsset {
    pub asset: String,
    #[allow(dead_code)]
    pub borrowed: String,
    pub free: String,
    #[allow(dead_code)]
    pub interest: String,
    pub locked: String,
    #[allow(dead_code)]
    pub net_asset: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceMarginOrder {
    pub order_id: i64,
    pub symbol: String,
    pub status: String,
    pub client_order_id: String,
    pub price: String,
    pub orig_qty: String,
    pub executed_qty: String,
    pub cummulative_quote_qty: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub side: String,
    pub time_in_force: String,
    pub time: i64,
    pub update_time: i64,
    pub is_working: bool,
    #[serde(default)]
    pub is_isolated: bool,
    #[serde(default)]
    pub stop_price: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)] // Mirrors Binance API response shape
pub struct BinanceMarginTrade {
    pub id: i64,
    pub order_id: i64,
    pub symbol: String,
    pub price: String,
    pub qty: String,
    #[allow(dead_code)]
    pub quote_qty: String,
    pub commission: String,
    #[allow(dead_code)]
    pub commission_asset: String,
    pub time: i64,
    pub is_buyer: bool,
    #[allow(dead_code)]
    pub is_maker: bool,
    #[allow(dead_code)]
    pub is_best_match: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub is_isolated: bool,
}
