//! Integration tests for trailing stop price calculation functions.
//!
//! Tests `calculate_stop_price`, `is_peak_improvement`, and `should_adjust_stop`
//! from the types module — verifying the calculation logic that drives
//! the executor integration.

use rust_decimal_macros::dec;

use tektii_gateway_core::models::{Side, TrailingType};
use tektii_gateway_core::trailing_stop::types::{
    calculate_stop_price, is_peak_improvement, should_adjust_stop,
};

#[test]
fn stop_price_sell_absolute() {
    // SELL trailing stop: protect long position — stop = peak - distance
    let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Absolute, Side::Sell);
    assert_eq!(stop, dec!(95));
}

#[test]
fn stop_price_buy_absolute() {
    // BUY trailing stop: protect short position — stop = peak + distance
    let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Absolute, Side::Buy);
    assert_eq!(stop, dec!(105));
}

#[test]
fn stop_price_sell_percentage() {
    // SELL trailing stop: 5% → stop = 100 * (1 - 0.05) = 95
    let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Percent, Side::Sell);
    assert_eq!(stop, dec!(95));
}

#[test]
fn stop_price_buy_percentage() {
    // BUY trailing stop: 5% → stop = 100 * (1 + 0.05) = 105
    let stop = calculate_stop_price(dec!(100), dec!(5), TrailingType::Percent, Side::Buy);
    assert_eq!(stop, dec!(105));
}

#[test]
fn peak_improvement_sell_higher_is_better() {
    assert!(is_peak_improvement(dec!(100), dec!(101), Side::Sell));
    assert!(!is_peak_improvement(dec!(100), dec!(99), Side::Sell));
    assert!(!is_peak_improvement(dec!(100), dec!(100), Side::Sell));
}

#[test]
fn peak_improvement_buy_lower_is_better() {
    assert!(is_peak_improvement(dec!(100), dec!(99), Side::Buy));
    assert!(!is_peak_improvement(dec!(100), dec!(101), Side::Buy));
    assert!(!is_peak_improvement(dec!(100), dec!(100), Side::Buy));
}

#[test]
fn should_adjust_first_placement_always_true() {
    assert!(should_adjust_stop(dec!(0), dec!(95)));
}

#[test]
fn should_adjust_significant_improvement() {
    // MIN_PRICE_IMPROVEMENT_PCT is 0.1%
    // 95.0 → 95.1 = 0.105% change → should adjust
    assert!(should_adjust_stop(dec!(95), dec!(95.1)));
}

#[test]
fn should_not_adjust_tiny_improvement() {
    // 95.0 → 95.005 = 0.0052% change → below threshold → no adjust
    assert!(!should_adjust_stop(dec!(95), dec!(95.005)));
}
