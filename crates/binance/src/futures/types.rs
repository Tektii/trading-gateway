//! Binance Futures-specific request and response types (USDS-M).

use serde::{Deserialize, Serialize};

pub const BINANCE_FUTURES_BASE_URL: &str = "https://fapi.binance.com";
pub const BINANCE_FUTURES_TESTNET_URL: &str = "https://testnet.binancefuture.com";

/// Binance Futures account information response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesAccount {
    pub total_wallet_balance: String,
    pub total_unrealized_profit: String,
    pub total_margin_balance: String,
    pub available_balance: String,
    #[allow(dead_code)]
    pub max_withdraw_amount: String,
    #[allow(dead_code)]
    pub can_trade: bool,
    #[allow(dead_code)]
    pub can_deposit: bool,
    #[allow(dead_code)]
    pub can_withdraw: bool,
    #[allow(dead_code)]
    pub update_time: i64,
    #[allow(dead_code)]
    pub assets: Vec<BinanceFuturesAsset>,
    #[allow(dead_code)]
    pub positions: Vec<BinanceFuturesAccountPosition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceFuturesAsset {
    pub asset: String,
    pub wallet_balance: String,
    pub unrealized_profit: String,
    pub margin_balance: String,
    pub available_balance: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceFuturesAccountPosition {
    pub symbol: String,
    pub position_side: String,
    pub position_amt: String,
    pub entry_price: String,
    pub unrealized_profit: String,
}

/// Binance Futures position risk response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesPosition {
    pub symbol: String,
    pub position_amt: String,
    pub entry_price: String,
    pub mark_price: String,
    pub un_realized_profit: String,
    pub liquidation_price: String,
    pub leverage: String,
    #[allow(dead_code)]
    pub position_side: String,
    pub margin_type: String,
    #[allow(dead_code)]
    pub isolated_margin: String,
    #[allow(dead_code)]
    pub notional: String,
    #[allow(dead_code)]
    pub update_time: i64,
}

/// Binance Futures order information.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesOrder {
    pub order_id: i64,
    pub symbol: String,
    pub status: String,
    pub client_order_id: String,
    pub price: String,
    pub avg_price: String,
    pub orig_qty: String,
    pub executed_qty: String,
    pub cum_quote: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub side: String,
    pub time_in_force: String,
    pub position_side: String,
    pub reduce_only: bool,
    #[serde(default)]
    pub stop_price: Option<String>,
    pub time: i64,
    pub update_time: i64,
}

/// Binance Futures trade (fill) information.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesTrade {
    pub id: i64,
    pub order_id: i64,
    pub symbol: String,
    pub price: String,
    pub qty: String,
    #[allow(dead_code)]
    pub quote_qty: String,
    pub commission: String,
    pub commission_asset: String,
    pub time: i64,
    pub side: String,
    #[allow(dead_code)]
    pub position_side: String,
    pub maker: bool,
    #[allow(dead_code)]
    pub buyer: bool,
}

/// Binance Futures book ticker (quote) response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinanceFuturesQuote {
    pub symbol: String,
    pub bid_price: String,
    pub bid_qty: String,
    pub ask_price: String,
    pub ask_qty: String,
    #[allow(dead_code)]
    pub time: i64,
}

/// Binance Futures kline (candlestick) data.
#[derive(Debug)]
pub struct BinanceFuturesKline {
    pub open_time: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    #[allow(dead_code)]
    pub close_time: i64,
    #[allow(dead_code)]
    pub quote_volume: String,
    #[allow(dead_code)]
    pub trades: i64,
}

impl<'de> Deserialize<'de> for BinanceFuturesKline {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let arr: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;
        if arr.len() < 11 {
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
            quote_volume: arr[7]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("Invalid quote_volume"))?
                .to_string(),
            trades: arr[8]
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom("Invalid trades"))?,
        })
    }
}
