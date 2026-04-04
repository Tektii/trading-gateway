//! Shared Binance API types.
//!
//! Types that are common across Binance products (Spot, Futures, Margin, etc.)
//! with identical or nearly identical schemas.

use serde::Deserialize;

/// Asset balance information.
///
/// Used in account responses across all Binance products.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceBalance {
    /// Asset name (e.g., "BTC", "USDT")
    pub asset: String,
    /// Free (available) balance as string
    pub free: String,
    /// Locked (in orders) balance as string
    pub locked: String,
}

/// Book ticker (quote) response.
///
/// Used by both Spot and Futures with identical schema.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceQuote {
    /// Symbol (e.g., "BTCUSDT")
    pub symbol: String,
    /// Best bid price
    pub bid_price: String,
    /// Best bid quantity
    pub bid_qty: String,
    /// Best ask price
    pub ask_price: String,
    /// Best ask quantity
    pub ask_qty: String,
    /// Update time (only present in Futures response)
    #[serde(default)]
    pub time: Option<i64>,
}

/// Kline (candlestick) data.
///
/// Both Spot and Futures return klines as arrays in the same format.
#[derive(Debug)]
#[allow(dead_code)]
pub struct BinanceKline {
    pub open_time: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    pub close_time: i64,
    pub quote_asset_volume: String,
    pub number_of_trades: i64,
    pub taker_buy_base_asset_volume: String,
    pub taker_buy_quote_asset_volume: String,
}

/// Custom deserializer for Binance kline array format.
impl<'de> Deserialize<'de> for BinanceKline {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let arr: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;

        if arr.len() < 11 {
            return Err(serde::de::Error::custom(format!(
                "Kline array too short: expected at least 11 elements, got {}",
                arr.len()
            )));
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

/// Trade (fill) information - common fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BinanceTradeCommon {
    pub id: i64,
    pub order_id: i64,
    pub symbol: String,
    pub price: String,
    pub qty: String,
    pub quote_qty: String,
    pub commission: String,
    pub commission_asset: String,
    pub time: i64,
    pub is_maker: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_kline_from_array() {
        let json = r#"[
            1499040000000,
            "0.01634000",
            "0.80000000",
            "0.01575800",
            "0.01577100",
            "148976.11427815",
            1499644799999,
            "2434.19055334",
            308,
            "1756.87402397",
            "28.46694368",
            "17928899.62484339"
        ]"#;

        let kline: BinanceKline = serde_json::from_str(json).unwrap();

        assert_eq!(kline.open_time, 1_499_040_000_000);
        assert_eq!(kline.open, "0.01634000");
        assert_eq!(kline.high, "0.80000000");
        assert_eq!(kline.close, "0.01577100");
        assert_eq!(kline.number_of_trades, 308);
    }

    #[test]
    fn deserialize_kline_fails_on_short_array() {
        let json = r#"[1499040000000, "0.01634000"]"#;
        let result: Result<BinanceKline, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_quote() {
        let json = r#"{
            "symbol": "BTCUSDT",
            "bidPrice": "50000.00",
            "bidQty": "1.5",
            "askPrice": "50001.00",
            "askQty": "2.0"
        }"#;

        let quote: BinanceQuote = serde_json::from_str(json).unwrap();
        assert_eq!(quote.symbol, "BTCUSDT");
        assert_eq!(quote.bid_price, "50000.00");
        assert!(quote.time.is_none());
    }
}
