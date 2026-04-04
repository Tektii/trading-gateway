//! Integration tests for trailing stop price source.
//!
//! Tests `MockPriceSource` subscription, price delivery, and unsubscription.

use rust_decimal_macros::dec;

use tektii_gateway_core::trailing_stop::PriceSource;
use tektii_gateway_test_support::mock_price_source::MockPriceSource;

#[tokio::test]
async fn mock_price_source_subscribe_and_receive() {
    let source = MockPriceSource::new();

    let mut rx = source.subscribe("BTCUSD").await.unwrap();

    source.emit_price("BTCUSD", dec!(50000)).await;

    let update = rx.recv().await.expect("should receive price update");
    assert_eq!(update.symbol, "BTCUSD");
    assert_eq!(update.price, dec!(50000));
}

#[tokio::test]
async fn current_price_returns_latest() {
    let source = MockPriceSource::new();

    // No price set → error
    assert!(source.current_price("BTCUSD").await.is_err());

    // Set price → returns it
    source.set_price("BTCUSD", dec!(48000));
    let price = source.current_price("BTCUSD").await.unwrap();
    assert_eq!(price, dec!(48000));

    // Update price → returns new value
    source.set_price("BTCUSD", dec!(49000));
    let price = source.current_price("BTCUSD").await.unwrap();
    assert_eq!(price, dec!(49000));
}

#[tokio::test]
async fn unsubscribe_removes_sender() {
    let source = MockPriceSource::new();

    let _rx = source.subscribe("BTCUSD").await.unwrap();
    source.unsubscribe("BTCUSD").await.unwrap();

    // After unsubscribe, emitting a price should not panic
    // (sender was removed, so emit_price is a no-op)
    source.emit_price("BTCUSD", dec!(50000)).await;
}

#[tokio::test]
async fn multiple_symbols_independent() {
    let source = MockPriceSource::new();

    source.set_price("BTCUSD", dec!(50000));
    source.set_price("ETHUSD", dec!(3000));

    assert_eq!(source.current_price("BTCUSD").await.unwrap(), dec!(50000));
    assert_eq!(source.current_price("ETHUSD").await.unwrap(), dec!(3000));

    // Updating one doesn't affect the other
    source.set_price("BTCUSD", dec!(51000));
    assert_eq!(source.current_price("BTCUSD").await.unwrap(), dec!(51000));
    assert_eq!(source.current_price("ETHUSD").await.unwrap(), dec!(3000));
}
