//! Unit tests for Binance WebSocket message parsing.
//!
//! Tests the pure conversion functions that map Binance JSON events
//! to gateway `WsMessage` types.

use tektii_gateway_binance::websocket::{process_market_message, process_user_data_message};
use tektii_gateway_core::models::{
    OrderStatus, OrderType, Side, TimeInForce, Timeframe, TradingPlatform,
};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};

const SPOT: TradingPlatform = TradingPlatform::BinanceSpotLive;
const FUTURES: TradingPlatform = TradingPlatform::BinanceFuturesLive;

// ============================================================================
// Market Data — Book Ticker
// ============================================================================

#[test]
fn test_book_ticker_parses_to_quote() {
    let json = r#"{"stream":"btcusdt@bookTicker","data":{"u":12345,"s":"BTCUSDT","b":"50000.50","B":"1.5","a":"50001.00","A":"2.0"}}"#;

    let events = process_market_message(json, SPOT);
    assert_eq!(events.len(), 1);

    match &events[0] {
        WsMessage::QuoteData { quote, .. } => {
            assert_eq!(quote.symbol, "BTCUSDT");
            assert_eq!(quote.provider, "binance-spot");
            assert_eq!(quote.bid.to_string(), "50000.50");
            assert_eq!(quote.ask.to_string(), "50001.00");
            assert!(quote.bid_size.is_some());
            assert!(quote.ask_size.is_some());
        }
        other => panic!("Expected QuoteData, got {other:?}"),
    }
}

// ============================================================================
// Market Data — Kline
// ============================================================================

#[test]
fn test_kline_parses_to_candle() {
    let json = r#"{"stream":"ethusdt@kline_1m","data":{"e":"kline","E":1700000000000,"s":"ETHUSDT","k":{"t":1700000000000,"T":1700000059999,"i":"1m","o":"2000.00","h":"2005.00","l":"1998.00","c":"2003.50","v":"150.5","x":false}}}"#;

    let events = process_market_message(json, SPOT);
    assert_eq!(events.len(), 1);

    match &events[0] {
        WsMessage::Candle { bar, .. } => {
            assert_eq!(bar.symbol, "ETHUSDT");
            assert_eq!(bar.provider, "binance-spot");
            assert_eq!(bar.timeframe, Timeframe::OneMinute);
            assert_eq!(bar.open.to_string(), "2000.00");
            assert_eq!(bar.high.to_string(), "2005.00");
            assert_eq!(bar.low.to_string(), "1998.00");
            assert_eq!(bar.close.to_string(), "2003.50");
            assert_eq!(bar.volume.to_string(), "150.5");
        }
        other => panic!("Expected Candle, got {other:?}"),
    }
}

#[test]
fn test_kline_1h_parses_timeframe() {
    let json = r#"{"stream":"btcusdt@kline_1h","data":{"e":"kline","E":1700000000000,"s":"BTCUSDT","k":{"t":1700000000000,"T":1700003599999,"i":"1h","o":"50000","h":"50100","l":"49900","c":"50050","v":"500","x":true}}}"#;

    let events = process_market_message(json, SPOT);
    assert_eq!(events.len(), 1);

    if let WsMessage::Candle { bar, .. } = &events[0] {
        assert_eq!(bar.timeframe, Timeframe::OneHour);
    } else {
        panic!("Expected Candle");
    }
}

// ============================================================================
// Market Data — Unknown / Subscription Responses
// ============================================================================

#[test]
fn test_subscription_response_ignored() {
    let json = r#"{"result":null,"id":1}"#;
    let events = process_market_message(json, SPOT);
    assert!(events.is_empty());
}

#[test]
fn test_unknown_stream_ignored() {
    let json = r#"{"stream":"btcusdt@unknownStream","data":{"some":"data"}}"#;
    let events = process_market_message(json, SPOT);
    assert!(events.is_empty());
}

#[test]
fn test_malformed_json_returns_empty() {
    let json = r"not valid json at all";
    let events = process_market_message(json, SPOT);
    assert!(events.is_empty());
}

// ============================================================================
// Spot — executionReport
// ============================================================================

fn spot_execution_report(execution_type: &str, order_status: &str) -> String {
    format!(
        r#"{{"e":"executionReport","E":1700000000000,"s":"BTCUSDT","c":"client123","S":"BUY","o":"LIMIT","f":"GTC","q":"0.5","p":"50000.00","P":"0","x":"{execution_type}","X":"{order_status}","r":"NONE","i":12345,"l":"0.25","z":"0.25","L":"49999.00","n":"0.001","N":"BNB","T":1700000000000,"C":"original123"}}"#
    )
}

#[test]
fn test_spot_new_order() {
    let json = spot_execution_report("NEW", "NEW");
    let events = process_user_data_message(&json, SPOT, false);
    assert_eq!(events.len(), 1);

    match &events[0] {
        WsMessage::Order { event, order, .. } => {
            assert_eq!(*event, OrderEventType::OrderCreated);
            assert_eq!(order.symbol, "BTCUSDT");
            assert_eq!(order.id, "12345");
            assert_eq!(order.status, OrderStatus::Open);
            assert_eq!(order.side, Side::Buy);
            assert_eq!(order.order_type, OrderType::Limit);
            assert_eq!(order.time_in_force, TimeInForce::Gtc);
        }
        other => panic!("Expected Order, got {other:?}"),
    }
}

#[test]
fn test_spot_partial_fill() {
    let json = spot_execution_report("TRADE", "PARTIALLY_FILLED");
    let events = process_user_data_message(&json, SPOT, false);
    assert_eq!(events.len(), 1);

    if let WsMessage::Order { event, order, .. } = &events[0] {
        assert_eq!(*event, OrderEventType::OrderPartiallyFilled);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
        assert_eq!(order.filled_quantity.to_string(), "0.25");
    } else {
        panic!("Expected Order");
    }
}

#[test]
fn test_spot_full_fill() {
    let json = spot_execution_report("TRADE", "FILLED");
    let events = process_user_data_message(&json, SPOT, false);
    assert_eq!(events.len(), 1);

    if let WsMessage::Order { event, order, .. } = &events[0] {
        assert_eq!(*event, OrderEventType::OrderFilled);
        assert_eq!(order.status, OrderStatus::Filled);
    } else {
        panic!("Expected Order");
    }
}

#[test]
fn test_spot_cancel_uses_original_client_order_id() {
    let json = spot_execution_report("CANCELED", "CANCELED");
    let events = process_user_data_message(&json, SPOT, false);
    assert_eq!(events.len(), 1);

    if let WsMessage::Order { event, order, .. } = &events[0] {
        assert_eq!(*event, OrderEventType::OrderCancelled);
        assert_eq!(order.status, OrderStatus::Cancelled);
        // On cancel, should use the original client order ID
        assert_eq!(order.client_order_id.as_deref(), Some("original123"));
    } else {
        panic!("Expected Order");
    }
}

#[test]
fn test_spot_rejected() {
    let json = spot_execution_report("REJECTED", "REJECTED");
    let events = process_user_data_message(&json, SPOT, false);
    assert_eq!(events.len(), 1);

    if let WsMessage::Order { event, order, .. } = &events[0] {
        assert_eq!(*event, OrderEventType::OrderRejected);
        assert_eq!(order.status, OrderStatus::Rejected);
    } else {
        panic!("Expected Order");
    }
}

#[test]
fn test_spot_expired() {
    let json = spot_execution_report("EXPIRED", "EXPIRED");
    let events = process_user_data_message(&json, SPOT, false);
    assert_eq!(events.len(), 1);

    if let WsMessage::Order { event, order, .. } = &events[0] {
        assert_eq!(*event, OrderEventType::OrderExpired);
        assert_eq!(order.status, OrderStatus::Expired);
    } else {
        panic!("Expected Order");
    }
}

// ============================================================================
// Spot — outboundAccountPosition
// ============================================================================

#[test]
fn test_spot_account_update() {
    let json = r#"{"e":"outboundAccountPosition","E":1700000000000,"B":[{"a":"USDT","f":"10000.50","l":"500.00"},{"a":"BTC","f":"0.5","l":"0.0"}]}"#;
    let events = process_user_data_message(json, SPOT, false);
    assert_eq!(events.len(), 1);

    match &events[0] {
        WsMessage::Account { account, .. } => {
            // Sum of free balances
            assert!(account.balance > rust_decimal::Decimal::ZERO);
        }
        other => panic!("Expected Account, got {other:?}"),
    }
}

// ============================================================================
// Futures — ORDER_TRADE_UPDATE
// ============================================================================

#[test]
fn test_futures_order_fill() {
    let json = r#"{"e":"ORDER_TRADE_UPDATE","E":1700000000000,"T":1700000000000,"o":{"s":"BTCUSDT","c":"fut_client1","S":"SELL","o":"LIMIT","f":"GTC","q":"0.1","p":"50000","sp":"0","ap":"49999.50","x":"TRADE","X":"FILLED","i":99999,"l":"0.1","z":"0.1","L":"49999.50","n":"0.01","N":"USDT","rp":"5.50","ps":"SHORT","R":false,"cr":null,"AP":null}}"#;
    let events = process_user_data_message(json, FUTURES, true);
    assert_eq!(events.len(), 1);

    match &events[0] {
        WsMessage::Order { event, order, .. } => {
            assert_eq!(*event, OrderEventType::OrderFilled);
            assert_eq!(order.symbol, "BTCUSDT");
            assert_eq!(order.side, Side::Sell);
            assert_eq!(order.status, OrderStatus::Filled);
            assert_eq!(order.average_fill_price.unwrap().to_string(), "49999.50");
        }
        other => panic!("Expected Order, got {other:?}"),
    }
}

#[test]
fn test_futures_trailing_stop_order() {
    let json = r#"{"e":"ORDER_TRADE_UPDATE","E":1700000000000,"T":1700000000000,"o":{"s":"ETHUSDT","c":"trail1","S":"SELL","o":"TRAILING_STOP_MARKET","f":"GTC","q":"1.0","p":"0","sp":"0","ap":"0","x":"NEW","X":"NEW","i":88888,"l":"0","z":"0","L":"0","n":"0","N":null,"rp":"0","ps":"BOTH","R":true,"cr":"1.5","AP":"3000.00"}}"#;
    let events = process_user_data_message(json, FUTURES, true);
    assert_eq!(events.len(), 1);

    match &events[0] {
        WsMessage::Order { event, order, .. } => {
            assert_eq!(*event, OrderEventType::OrderCreated);
            assert_eq!(order.order_type, OrderType::TrailingStop);
            assert_eq!(order.trailing_distance.unwrap().to_string(), "1.5");
            assert_eq!(
                order.trailing_type,
                Some(tektii_gateway_core::models::TrailingType::Percent)
            );
            assert_eq!(order.reduce_only, Some(true));
        }
        other => panic!("Expected Order, got {other:?}"),
    }
}

// ============================================================================
// Futures — ACCOUNT_UPDATE
// ============================================================================

#[test]
fn test_futures_account_update() {
    let json = r#"{"e":"ACCOUNT_UPDATE","E":1700000000000,"a":{"m":"ORDER","B":[{"a":"USDT","wb":"15000.50","cw":"14500.00"}],"P":[{"s":"BTCUSDT","pa":"0.1","ep":"50000","up":"50.00","mt":"cross","ps":"LONG"}]}}"#;
    let events = process_user_data_message(json, FUTURES, true);
    assert_eq!(events.len(), 1);

    match &events[0] {
        WsMessage::Account { account, .. } => {
            assert_eq!(account.balance.to_string(), "15000.50");
            assert_eq!(account.currency, "USDT");
        }
        other => panic!("Expected Account, got {other:?}"),
    }
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_unknown_user_data_event_ignored() {
    let json = r#"{"e":"STRATEGY_UPDATE","E":1700000000000,"data":"something"}"#;
    let events = process_user_data_message(json, SPOT, false);
    assert!(events.is_empty());
}

#[test]
fn test_listen_key_expired_ignored() {
    let json = r#"{"e":"listenKeyExpired","E":1700000000000}"#;
    let events = process_user_data_message(json, SPOT, false);
    assert!(events.is_empty());
}

#[test]
fn test_malformed_user_data_returns_empty() {
    let json = r"totally not json";
    let events = process_user_data_message(json, SPOT, false);
    assert!(events.is_empty());
}

#[test]
fn test_spot_event_on_futures_ignored() {
    // executionReport sent to a futures parser should be ignored
    let json = spot_execution_report("NEW", "NEW");
    let events = process_user_data_message(&json, FUTURES, true);
    assert!(events.is_empty());
}

#[test]
fn test_futures_event_on_spot_ignored() {
    let json = r#"{"e":"ORDER_TRADE_UPDATE","E":1700000000000,"T":1700000000000,"o":{"s":"BTCUSDT","c":"c1","S":"BUY","o":"MARKET","f":"GTC","q":"0.1","p":"0","sp":"0","ap":"50000","x":"NEW","X":"NEW","i":1,"l":"0","z":"0","L":"0","n":"0","N":null,"rp":"0","ps":"BOTH","R":false,"cr":null,"AP":null}}"#;
    let events = process_user_data_message(json, SPOT, false);
    assert!(events.is_empty());
}
