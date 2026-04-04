//! Binance Spot-specific request and response types.

use serde::{Deserialize, Serialize};

/// Binance production API base URL
pub const BINANCE_BASE_URL: &str = "https://api.binance.com";

/// Binance testnet API base URL for testing
pub const BINANCE_TESTNET_URL: &str = "https://testnet.binance.vision";

/// Binance account information response
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceAccount {
    #[allow(dead_code)]
    pub update_time: i64,
    #[allow(dead_code)]
    pub account_type: String,
    pub balances: Vec<BinanceBalance>,
    #[allow(dead_code)]
    pub permissions: Vec<String>,
}

/// Binance asset balance
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceBalance {
    pub asset: String,
    pub free: String,
    pub locked: String,
}

/// Binance trade (fill) information
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceTrade {
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
}

/// Binance order information
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceOrder {
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
    #[serde(default)]
    pub time: i64,
    #[serde(default)]
    pub update_time: i64,
    #[serde(default)]
    pub is_working: bool,
    #[serde(default)]
    pub stop_price: Option<String>,
}

/// Binance cancel-replace order response
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceCancelReplaceResponse {
    pub cancel_result: String,
    pub new_order_result: String,
    #[serde(default)]
    pub cancel_response: Option<BinanceOrder>,
    #[serde(default)]
    pub new_order_response: Option<BinanceOrder>,
}

/// Binance OCO order response
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceOcoResponse {
    pub order_list_id: i64,
    pub contingency_type: String,
    pub list_status_type: String,
    pub list_order_status: String,
    pub list_client_order_id: String,
    pub transaction_time: i64,
    pub symbol: String,
    pub orders: Vec<BinanceOcoOrderLeg>,
    pub order_reports: Vec<BinanceOcoOrderReport>,
}

/// Individual order leg within an OCO response
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceOcoOrderLeg {
    pub symbol: String,
    pub order_id: i64,
    pub client_order_id: String,
}

/// Detailed order report for each OCO leg
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceOcoOrderReport {
    pub symbol: String,
    pub order_id: i64,
    pub order_list_id: i64,
    pub client_order_id: String,
    pub transact_time: i64,
    pub price: String,
    pub orig_qty: String,
    pub executed_qty: String,
    pub cummulative_quote_qty: String,
    pub status: String,
    pub time_in_force: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub side: String,
    #[serde(default)]
    pub stop_price: Option<String>,
}

/// Binance book ticker (quote) response
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceQuote {
    pub symbol: String,
    pub bid_price: String,
    pub bid_qty: String,
    pub ask_price: String,
    pub ask_qty: String,
}

/// Binance kline (candlestick) data
#[derive(Debug)]
pub struct BinanceKline {
    pub open_time: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    #[allow(dead_code)]
    pub close_time: i64,
    #[allow(dead_code)]
    pub quote_asset_volume: String,
    #[allow(dead_code)]
    pub number_of_trades: i64,
    #[allow(dead_code)]
    pub taker_buy_base_asset_volume: String,
    #[allow(dead_code)]
    pub taker_buy_quote_asset_volume: String,
}

/// Custom deserializer for Binance kline array format
impl<'de> Deserialize<'de> for BinanceKline {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let arr: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;

        if arr.len() < 12 {
            return Err(serde::de::Error::custom("Kline array too short"));
        }

        Ok(Self {
            open_time: arr[0]
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom("Invalid open_time"))?,
            open: arr[1]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid open"))?
                .to_string(),
            high: arr[2]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid high"))?
                .to_string(),
            low: arr[3]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid low"))?
                .to_string(),
            close: arr[4]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid close"))?
                .to_string(),
            volume: arr[5]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid volume"))?
                .to_string(),
            close_time: arr[6]
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom("Invalid close_time"))?,
            quote_asset_volume: arr[7]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid quote_asset_volume"))?
                .to_string(),
            number_of_trades: arr[8]
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom("Invalid number_of_trades"))?,
            taker_buy_base_asset_volume: arr[9]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid taker_buy_base_asset_volume"))?
                .to_string(),
            taker_buy_quote_asset_volume: arr[10]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid taker_buy_quote_asset_volume"))?
                .to_string(),
        })
    }
}
