//! End-to-end market data flow tests (Story 6.5.3).
//!
//! Tests market data flowing through the full gateway stack:
//! bars via REST, quote/candle events via WebSocket, and round-trip flows.

use std::time::Duration;

use rust_decimal_macros::dec;
use tektii_gateway_core::models::{Quote, TradingPlatform};
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_test_support::harness::{StrategyClient, spawn_test_gateway};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::{test_bar, test_quote};

const TIMEOUT: Duration = Duration::from_secs(2);

fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

// ---------------------------------------------------------------------------
// REST bars
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rest_get_bars_returns_configured_bars() {
    let bar = test_bar("AAPL");
    let adapter =
        MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_bars("AAPL", vec![bar]);
    let gw = spawn_test_gateway(adapter).await;

    let resp = http_client()
        .get(format!("{}/bars/AAPL?timeframe=1m", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["symbol"], "AAPL");
    assert_eq!(body[0]["open"], "100");
    assert_eq!(body[0]["high"], "100");
    assert_eq!(body[0]["low"], "100");
    assert_eq!(body[0]["close"], "100");
    assert_eq!(body[0]["volume"], "1000");
}

#[tokio::test]
async fn rest_get_bars_unknown_symbol_returns_empty() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = http_client()
        .get(format!("{}/bars/NOPE?timeframe=1m", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(body.is_empty(), "no bars configured — empty list expected");
}

#[tokio::test]
async fn rest_get_bars_missing_timeframe_returns_400() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let resp = http_client()
        .get(format!("{}/bars/AAPL", gw.base_url()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

// ---------------------------------------------------------------------------
// WebSocket streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quote_event_reaches_strategy_via_websocket() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let quote = test_quote("AAPL");
    gw.inject_event(WsMessage::quote(quote));

    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "expected to receive quote event via WS");

    if let Some(WsMessage::QuoteData { quote, .. }) = msg {
        assert_eq!(quote.symbol, "AAPL");
        assert_eq!(quote.bid, dec!(99));
        assert_eq!(quote.ask, dec!(101));
        assert_eq!(quote.last, dec!(100));
    } else {
        panic!("expected WsMessage::QuoteData, got: {msg:?}");
    }
}

#[tokio::test]
async fn candle_event_reaches_strategy_via_websocket() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let bar = test_bar("AAPL");
    gw.inject_event(WsMessage::candle(bar));

    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "expected to receive candle event via WS");

    if let Some(WsMessage::Candle { bar, .. }) = msg {
        assert_eq!(bar.symbol, "AAPL");
        assert_eq!(bar.open, dec!(100));
        assert_eq!(bar.high, dec!(100));
        assert_eq!(bar.low, dec!(100));
        assert_eq!(bar.close, dec!(100));
        assert_eq!(bar.volume, dec!(1000));
    } else {
        panic!("expected WsMessage::Candle, got: {msg:?}");
    }
}

#[tokio::test]
async fn multiple_symbols_stream_independently() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    gw.inject_event(WsMessage::quote(test_quote("AAPL")));
    gw.inject_event(WsMessage::quote(test_quote("BTCUSD")));

    let msg1 = client.recv_message(TIMEOUT).await;
    let msg2 = client.recv_message(TIMEOUT).await;

    assert!(msg1.is_some(), "expected first quote event");
    assert!(msg2.is_some(), "expected second quote event");

    if let Some(WsMessage::QuoteData { quote, .. }) = msg1 {
        assert_eq!(quote.symbol, "AAPL");
    } else {
        panic!("expected WsMessage::QuoteData for AAPL, got: {msg1:?}");
    }

    if let Some(WsMessage::QuoteData { quote, .. }) = msg2 {
        assert_eq!(quote.symbol, "BTCUSD");
    } else {
        panic!("expected WsMessage::QuoteData for BTCUSD, got: {msg2:?}");
    }
}

// ---------------------------------------------------------------------------
// Full round-trip: REST query → streaming update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rest_query_then_streaming_update() {
    let quote = test_quote("AAPL");
    let adapter = MockTradingAdapter::new(TradingPlatform::AlpacaPaper).with_quote(quote);
    let gw = spawn_test_gateway(adapter).await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Step 1: Query current quote via REST
    let resp = http_client()
        .get(format!("{}/quotes/AAPL", gw.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["symbol"], "AAPL");
    assert_eq!(body["last"], "100");

    // Step 2: Market moves — new quote arrives via WebSocket
    let updated_quote = Quote {
        last: dec!(105),
        bid: dec!(104),
        ask: dec!(106),
        ..test_quote("AAPL")
    };
    gw.inject_event(WsMessage::quote(updated_quote));

    let msg = client.recv_message(TIMEOUT).await;
    assert!(msg.is_some(), "expected updated quote via WS");

    if let Some(WsMessage::QuoteData { quote, .. }) = msg {
        assert_eq!(quote.symbol, "AAPL");
        assert_eq!(quote.last, dec!(105));
        assert_eq!(quote.bid, dec!(104));
        assert_eq!(quote.ask, dec!(106));
    } else {
        panic!("expected WsMessage::QuoteData, got: {msg:?}");
    }
}
