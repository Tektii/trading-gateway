//! Shared test helpers for Tektii adapter integration tests.
#![allow(dead_code)]

use std::sync::Arc;

use rust_decimal_macros::dec;
use serde_json::{Value, json};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::ExitHandler;
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::messages::{InstrumentSubscription, WsMessage};
use tektii_gateway_core::websocket::provider::ProviderConfig;
use tektii_gateway_tektii::{TektiiAdapter, TektiiCredentials, TektiiWebSocketProvider};
use tektii_gateway_test_support::wiremock_helpers::merge_json;
use tektii_protocol::websocket::ClientMessage;
use tokio::sync::broadcast;

/// Create a `TektiiAdapter` pointed at a wiremock server.
pub fn test_adapter(base_url: &str) -> TektiiAdapter {
    let credentials = TektiiCredentials::new(base_url, "ws://unused:9999");
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    TektiiAdapter::new(&credentials, broadcaster).expect("Failed to build HTTP client")
}

// =========================================================================
// Engine JSON response builders
// =========================================================================

/// Engine `OrderHandle` response with optional overrides.
pub fn engine_order_handle_json(overrides: &Value) -> Value {
    let mut base = json!({
        "id": "order-abc-123",
        "client_order_id": "client-001",
        "status": "open"
    });
    merge_json(&mut base, overrides);
    base
}

/// Engine `Order` response with optional overrides.
pub fn engine_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "id": "order-abc-123",
        "client_order_id": "client-001",
        "symbol": "AAPL",
        "side": "buy",
        "order_type": "market",
        "quantity": "10",
        "filled_quantity": "0",
        "price": "0",
        "status": "open",
        "created_at": 1_704_067_200_000u64
    });
    merge_json(&mut base, overrides);
    base
}

/// Engine `Account` response.
pub fn engine_account_json() -> Value {
    json!({
        "balance": "100000",
        "equity": "105000",
        "unrealized_pnl": "5000",
        "realized_pnl": "2000",
        "total_commission": "100",
        "margin_used": "10000",
        "margin_available": "90000"
    })
}

/// Engine `Position` response with optional overrides.
pub fn engine_position_json(overrides: &Value) -> Value {
    let mut base = json!({
        "id": "pos-001",
        "symbol": "AAPL",
        "side": "long",
        "quantity": "10",
        "avg_entry_price": "150",
        "current_price": "155",
        "unrealized_pnl": "50"
    });
    merge_json(&mut base, overrides);
    base
}

/// Engine `Quote` response with optional overrides.
pub fn engine_quote_json(overrides: &Value) -> Value {
    let mut base = json!({
        "symbol": "AAPL",
        "bid": "150.00",
        "ask": "150.50",
        "timestamp": 1_704_067_200_000u64
    });
    merge_json(&mut base, overrides);
    base
}

/// Engine `Bar` response with optional overrides.
pub fn engine_bar_json(overrides: &Value) -> Value {
    let mut base = json!({
        "timestamp": 1_704_067_200_000u64,
        "open": "150.00",
        "high": "152.00",
        "low": "149.50",
        "close": "151.00",
        "volume": 10000.0
    });
    merge_json(&mut base, overrides);
    base
}

/// Engine `Trade` response with optional overrides.
pub fn engine_trade_json(overrides: &Value) -> Value {
    let mut base = json!({
        "id": "trade-001",
        "order_id": "order-abc-123",
        "symbol": "AAPL",
        "side": "buy",
        "quantity": "10",
        "price": "150.50",
        "commission": "1.50",
        "timestamp": 1_704_067_200_000u64,
        "liquidation": false
    });
    merge_json(&mut base, overrides);
    base
}

// =========================================================================
// WebSocket provider helpers
// =========================================================================

/// Create a `TektiiWebSocketProvider` with an empty subscription filter
/// (matches ALL events — subscribed path, events wait for strategy ACK).
pub fn create_ws_provider(
    ws_url: &str,
) -> (TektiiWebSocketProvider, broadcast::Receiver<WsMessage>) {
    create_ws_provider_with_filter(ws_url, SubscriptionFilter::new(&[]))
}

/// Create a `TektiiWebSocketProvider` with a restrictive filter that matches
/// nothing (forces non-subscribed / immediate-ack path for all events).
pub fn create_ws_provider_no_subscriptions(
    ws_url: &str,
) -> (TektiiWebSocketProvider, broadcast::Receiver<WsMessage>) {
    // A non-empty filter that only matches "NONEXISTENT" instrument — real
    // events for AAPL etc. will not match, triggering the immediate-ack path.
    let filter = SubscriptionFilter::new(&[InstrumentSubscription::new(
        TradingPlatform::Tektii,
        "NONEXISTENT".to_string(),
        vec!["*".to_string()],
    )]);
    create_ws_provider_with_filter(ws_url, filter)
}

fn create_ws_provider_with_filter(
    ws_url: &str,
    filter: SubscriptionFilter,
) -> (TektiiWebSocketProvider, broadcast::Receiver<WsMessage>) {
    let state_manager = Arc::new(StateManager::new());
    let exit_handler: Arc<dyn tektii_gateway_core::exit_management::ExitHandling> = Arc::new(
        ExitHandler::with_defaults(Arc::clone(&state_manager), TradingPlatform::Tektii),
    );
    let (tx, rx) = broadcast::channel::<WsMessage>(64);
    let event_router = Arc::new(EventRouter::new(
        state_manager,
        exit_handler,
        tx,
        TradingPlatform::Tektii,
    ));

    let provider = TektiiWebSocketProvider::new(
        ws_url.to_string(),
        event_router,
        TradingPlatform::Tektii,
        Arc::new(filter),
    );

    (provider, rx)
}

/// Minimal `ProviderConfig` for calling `connect()`.
pub fn minimal_provider_config() -> ProviderConfig {
    ProviderConfig {
        platform: TradingPlatform::Tektii,
        symbols: vec![],
        event_types: vec![],
        credentials: None,
        tektii_params: None,
    }
}

// =========================================================================
// Engine protocol type builders (for constructing ServerMessage)
// =========================================================================

/// Minimal engine `Order` struct for WebSocket event testing.
pub fn make_engine_order() -> tektii_protocol::rest::Order {
    tektii_protocol::rest::Order {
        id: "order-abc-123".to_string(),
        client_order_id: Some("client-001".to_string()),
        symbol: "AAPL".to_string(),
        side: tektii_protocol::rest::Side::Buy,
        order_type: tektii_protocol::rest::OrderType::Market,
        quantity: dec!(10),
        filled_quantity: dec!(0),
        price: dec!(0),
        limit_price: None,
        status: tektii_protocol::rest::OrderStatus::Open,
        position_id: None,
        created_at: 1_704_067_200_000,
        executed_at: None,
        cancelled_at: None,
        reject_reason: None,
        rejected_at: None,
    }
}

/// Minimal engine `Trade` struct for WebSocket event testing.
pub fn make_engine_trade() -> tektii_protocol::rest::Trade {
    tektii_protocol::rest::Trade {
        id: "trade-001".to_string(),
        order_id: "order-abc-123".to_string(),
        position_id: None,
        symbol: "AAPL".to_string(),
        side: tektii_protocol::rest::Side::Buy,
        quantity: dec!(10),
        price: dec!(150.50),
        base_price: None,
        slippage_bps: None,
        commission: dec!(1.50),
        realized_pnl: None,
        timestamp: 1_704_067_200_000,
        liquidation: false,
    }
}

/// Minimal engine `Position` struct for WebSocket event testing.
pub fn make_engine_position() -> tektii_protocol::rest::Position {
    tektii_protocol::rest::Position {
        id: "pos-001".to_string(),
        symbol: "AAPL".to_string(),
        side: tektii_protocol::rest::PositionSide::Long,
        quantity: dec!(10),
        avg_entry_price: dec!(150),
        current_price: dec!(155),
        unrealized_pnl: dec!(50),
    }
}

/// Minimal engine `Account` struct for WebSocket event testing.
pub fn make_engine_account() -> tektii_protocol::rest::Account {
    tektii_protocol::rest::Account {
        balance: dec!(100000),
        equity: dec!(105000),
        unrealized_pnl: dec!(5000),
        realized_pnl: dec!(2000),
        total_commission: dec!(100),
        margin_used: dec!(10000),
        margin_available: dec!(90000),
    }
}

/// Parse a JSON string from the MockWsServer into a `ClientMessage`.
/// Panics with context on parse failure.
pub fn parse_client_message(raw: &str) -> ClientMessage {
    serde_json::from_str(raw)
        .unwrap_or_else(|e| panic!("Failed to parse ClientMessage: {e}\nRaw: {raw}"))
}
