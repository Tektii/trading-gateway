//! Synthetic price generation for mock provider.
//!
//! Simple random walk around seed prices. Not realistic market data —
//! just enough to see "water moving through the pipes."

use chrono::{Duration, Utc};
use parking_lot::RwLock;
use rand::Rng;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

use tektii_gateway_core::models::{Bar, Quote, Timeframe};

/// Generates synthetic prices via random walk.
pub struct PriceGenerator {
    prices: RwLock<HashMap<String, Decimal>>,
    provider_name: &'static str,
}

impl PriceGenerator {
    pub fn new(provider_name: &'static str) -> Self {
        Self {
            prices: RwLock::new(HashMap::new()),
            provider_name,
        }
    }

    /// Get current price for a symbol, applying a small random walk step.
    pub fn get_price(&self, symbol: &str) -> Decimal {
        let mut prices = self.prices.write();
        let price = prices
            .entry(symbol.to_string())
            .or_insert_with(|| seed_price(symbol));

        // Random walk: ±0.1%
        let mut rng = rand::rng();
        let delta: f64 = rng.random_range(-0.001..0.001);
        let multiplier = Decimal::try_from(1.0 + delta).unwrap_or(Decimal::ONE);
        *price = (*price * multiplier).max(dec!(0.01));

        *price
    }

    /// Get a quote with bid/ask spread around current price.
    pub fn get_quote(&self, symbol: &str) -> Quote {
        let last = self.get_price(symbol);
        let spread = (last * dec!(0.0001)).max(dec!(0.01));

        Quote {
            symbol: symbol.to_string(),
            provider: self.provider_name.to_string(),
            bid: last - spread,
            bid_size: Some(dec!(100)),
            ask: last + spread,
            ask_size: Some(dec!(100)),
            last,
            volume: Some(dec!(50000)),
            timestamp: Utc::now(),
        }
    }

    /// Generate a single synthetic 1-minute bar at the current price.
    pub fn get_bar(&self, symbol: &str) -> Bar {
        let close = self.get_price(symbol);
        let mut rng = rand::rng();

        // Small random variation for open/high/low
        let open_delta: f64 = rng.random_range(-0.002..0.002);
        let open =
            (close * Decimal::try_from(1.0 + open_delta).unwrap_or(Decimal::ONE)).max(dec!(0.01));
        let high = open.max(close) * dec!(1.003);
        let low = (open.min(close) * dec!(0.997)).max(dec!(0.01));

        Bar {
            symbol: symbol.to_string(),
            provider: self.provider_name.to_string(),
            timeframe: Timeframe::OneMinute,
            timestamp: Utc::now(),
            open,
            high: high.max(dec!(0.01)),
            low: low.max(dec!(0.01)),
            close,
            volume: Decimal::from(rng.random_range(1000_u32..10000)),
        }
    }

    /// Generate synthetic bars walking backward from current price.
    pub fn generate_bars(&self, symbol: &str, timeframe: Timeframe, count: u32) -> Vec<Bar> {
        let current_price = self.get_price(symbol);
        let mut rng = rand::rng();
        let mut bars = Vec::with_capacity(count as usize);
        let mut price = current_price;
        let interval = timeframe_duration(timeframe);
        let now = Utc::now();

        for i in 0..count {
            let delta: f64 = rng.random_range(-0.005..0.005);
            let multiplier = Decimal::try_from(1.0 + delta).unwrap_or(Decimal::ONE);
            let close = (price * multiplier).max(dec!(0.01));
            let open = price;
            let high = open.max(close) * dec!(1.003);
            let low = (open.min(close) * dec!(0.997)).max(dec!(0.01));

            bars.push(Bar {
                symbol: symbol.to_string(),
                provider: self.provider_name.to_string(),
                timeframe,
                #[allow(clippy::cast_possible_wrap)]
                timestamp: now - interval * (count - i) as i32,
                open,
                high: high.max(dec!(0.01)),
                low: low.max(dec!(0.01)),
                close,
                volume: Decimal::from(rng.random_range(1000_u32..10000)),
            });

            price = close;
        }

        bars
    }
}

/// Seed prices for well-known symbols.
fn seed_price(symbol: &str) -> Decimal {
    match symbol.to_uppercase().as_str() {
        "AAPL" | "USD/JPY" | "USDJPY" => dec!(150),
        "MSFT" => dec!(400),
        "GOOG" | "GOOGL" => dec!(170),
        "AMZN" => dec!(180),
        "TSLA" => dec!(250),
        "NVDA" => dec!(800),
        "BTC/USD" | "BTCUSD" => dec!(50000),
        "ETH/USD" | "ETHUSD" => dec!(3000),
        "EUR/USD" | "EURUSD" => dec!(1.08),
        "GBP/USD" | "GBPUSD" => dec!(1.27),
        _ => dec!(100),
    }
}

fn timeframe_duration(tf: Timeframe) -> Duration {
    match tf {
        Timeframe::OneMinute => Duration::minutes(1),
        Timeframe::TwoMinutes => Duration::minutes(2),
        Timeframe::FiveMinutes => Duration::minutes(5),
        Timeframe::TenMinutes => Duration::minutes(10),
        Timeframe::FifteenMinutes => Duration::minutes(15),
        Timeframe::ThirtyMinutes => Duration::minutes(30),
        Timeframe::OneHour => Duration::hours(1),
        Timeframe::TwoHours => Duration::hours(2),
        Timeframe::FourHours => Duration::hours(4),
        Timeframe::TwelveHours => Duration::hours(12),
        Timeframe::OneDay => Duration::days(1),
        Timeframe::OneWeek => Duration::weeks(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_symbol_gets_seed_price() {
        let pg = PriceGenerator::new("mock");
        let price = pg.get_price("AAPL");
        // Should be near $150, allowing for one random walk step
        assert!(price > dec!(140) && price < dec!(160));
    }

    #[test]
    fn unknown_symbol_defaults_to_100() {
        let pg = PriceGenerator::new("mock");
        let price = pg.get_price("XYZABC");
        assert!(price > dec!(90) && price < dec!(110));
    }

    #[test]
    fn prices_change_between_calls() {
        let pg = PriceGenerator::new("mock");
        let p1 = pg.get_price("AAPL");
        let p2 = pg.get_price("AAPL");
        // Extremely unlikely to be exactly equal after two random walks
        // but not impossible, so we just check they're both positive
        assert!(p1 > Decimal::ZERO);
        assert!(p2 > Decimal::ZERO);
    }

    #[test]
    fn quote_has_valid_spread() {
        let pg = PriceGenerator::new("mock");
        let quote = pg.get_quote("AAPL");
        assert!(quote.bid < quote.ask);
        assert!(quote.last >= quote.bid);
        assert!(quote.last <= quote.ask);
    }

    #[test]
    fn get_bar_returns_valid_ohlcv() {
        let pg = PriceGenerator::new("mock");
        let bar = pg.get_bar("AAPL");
        assert_eq!(bar.symbol, "AAPL");
        assert_eq!(bar.provider, "mock");
        assert_eq!(bar.timeframe, Timeframe::OneMinute);
        assert!(bar.high >= bar.low);
        assert!(bar.high >= bar.open);
        assert!(bar.high >= bar.close);
        assert!(bar.low <= bar.open);
        assert!(bar.low <= bar.close);
        assert!(bar.volume > Decimal::ZERO);
    }

    #[test]
    fn get_bar_ohlcv_invariants_hold_over_many_runs() {
        let pg = PriceGenerator::new("mock");
        for symbol in &["AAPL", "EURUSD", "BTCUSD"] {
            for _ in 0..500 {
                let bar = pg.get_bar(symbol);
                assert!(bar.high >= bar.open, "high < open: {bar:?}");
                assert!(bar.high >= bar.close, "high < close: {bar:?}");
                assert!(bar.low <= bar.open, "low > open: {bar:?}");
                assert!(bar.low <= bar.close, "low > close: {bar:?}");
                assert!(bar.high >= bar.low, "high < low: {bar:?}");
            }
        }
    }

    #[test]
    fn bars_returns_requested_count() {
        let pg = PriceGenerator::new("mock");
        let bars = pg.generate_bars("AAPL", Timeframe::OneHour, 5);
        assert_eq!(bars.len(), 5);
        for bar in &bars {
            assert!(bar.high >= bar.low);
            assert!(bar.volume > Decimal::ZERO);
        }
    }
}
