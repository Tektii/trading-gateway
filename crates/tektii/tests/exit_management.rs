//! TEK-271: ACK round-trip for engine-placed SL/TP fills under the Tektii
//! backtest provider.
//!
//! Under the TEK-268 simplification, every engine event that produces a
//! downstream `WsMessage` registers pending in `TektiiAckBridge` and waits for
//! a strategy ACK. This includes Order(`Filled`) events for SL/TP orders the
//! strategy never directly placed (the engine fills them as part of bracket
//! orders). Auto-correlation drains all pending entries on any strategy ACK.
//!
//! Without this path, any backtest using SL/TP would silently stall when the
//! engine waits forever for an ACK the strategy can't generate. These tests
//! cover the round-trip and prove the failure mode is detectable.

mod helpers;

use std::sync::Arc;
use std::time::Duration;

use tektii_gateway_core::config::ReconnectionConfig;
use tektii_gateway_core::correlation::CorrelationStore;
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::connection::{WsConnection, WsConnectionManager};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_core::websocket::provider::WebSocketProvider;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_test_support::websocket::MockWsServer;
use tektii_protocol::rest::{OrderEventType as EngineOrderEventType, OrderStatus};
use tektii_protocol::websocket::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use helpers::{
    create_ws_provider, make_engine_order, minimal_provider_config, parse_client_message,
};

const SL_ORDER_ID: &str = "engine-sl-001";
const SL_FILL_EVENT_ID: &str = "evt-sl-fill-1";

fn send_server_message(tx: &mpsc::UnboundedSender<String>, msg: &ServerMessage) {
    let json = serde_json::to_string(msg).expect("serialize ServerMessage");
    tx.send(json).expect("send to MockWsServer");
}

/// Build an engine `Order` representing a filled SL/TP that the engine placed
/// itself (not originated by the strategy).
fn engine_sl_fill_order() -> tektii_protocol::rest::Order {
    let mut order = make_engine_order();
    order.id = SL_ORDER_ID.to_string();
    order.client_order_id = None;
    order.status = OrderStatus::Filled;
    order.filled_quantity = order.quantity;
    order
}

/// Set up the full provider → registry → strategy pipeline against a
/// `MockWsServer` engine.
///
/// Returns the live mock server, the engine→gateway sender, the gateway→engine
/// receiver (for asserting `EventAck` payloads), the registered registry, and
/// the strategy connection's outbound receiver.
async fn setup_pipeline() -> (
    MockWsServer,
    mpsc::UnboundedSender<String>,
    mpsc::UnboundedReceiver<String>,
    Arc<ProviderRegistry>,
    mpsc::Receiver<WsMessage>,
) {
    let (server, engine_tx, engine_rx) = MockWsServer::start().await;

    let (provider, _broadcast_rx) = create_ws_provider(&server.url());
    let stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("provider.connect should succeed");

    // Capture the bridge before `register_provider` consumes the provider —
    // mirrors the wiring in `src/main.rs::connect_tektii_provider`.
    let ack_bridge = provider
        .ack_bridge()
        .await
        .expect("ack bridge available after connect");

    let ws_manager = Arc::new(WsConnectionManager::new());
    let cancel = CancellationToken::new();
    let registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]),
        cancel,
        ReconnectionConfig::default(),
        Arc::new(CorrelationStore::new()),
    ));

    registry.set_tektii_ack_bridge(ack_bridge).await;

    registry
        .register_provider(
            TradingPlatform::Tektii,
            Box::new(provider),
            stream,
            vec![],
            vec![],
        )
        .await
        .expect("register_provider should succeed");

    let (out_tx, out_rx) = mpsc::channel::<WsMessage>(1024);
    let conn = WsConnection::new(out_tx, "127.0.0.1:1".parse().unwrap());
    let conn_id = conn.id;
    ws_manager.add_connection(conn).await;
    registry.register_strategy_connection(conn_id).await;

    (server, engine_tx, engine_rx, registry, out_rx)
}

/// TEK-271: an engine-placed SL/TP fill reaches the strategy, the strategy
/// ACK drains the auto-tracked pending entry, and the engine receives a
/// `ClientMessage::EventAck` carrying the original `event_id`.
///
/// A regression that skips `register_pending` for engine-placed fills (or
/// fails to wire the bridge into the registry) would break the
/// `events_processed` assertion below.
#[tokio::test]
async fn engine_placed_sl_fill_acks_round_trip() {
    let (server, engine_tx, mut engine_rx, registry, mut out_rx) = setup_pipeline().await;

    let fill = ServerMessage::order(
        SL_FILL_EVENT_ID,
        EngineOrderEventType::Filled,
        engine_sl_fill_order(),
    );
    send_server_message(&engine_tx, &fill);

    // Strategy receives the SL/TP fill.
    let received = timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("strategy never received SL/TP fill (TEK-271)")
        .expect("strategy channel closed before fill arrived");

    match received {
        WsMessage::Order { event, order, .. } => {
            assert_eq!(order.id, SL_ORDER_ID);
            assert_eq!(event, OrderEventType::OrderFilled);
        }
        other => panic!("expected Order, got {other:?}"),
    }

    // Strategy ACK auto-drains all pending events to the engine.
    registry.handle_strategy_ack().await;

    let raw = timeout(Duration::from_secs(2), engine_rx.recv())
        .await
        .expect("engine never received EventAck — backtest would stall (TEK-271)")
        .expect("engine channel closed before EventAck arrived");

    match parse_client_message(&raw) {
        ClientMessage::EventAck {
            events_processed, ..
        } => {
            assert_eq!(
                events_processed,
                vec![SL_FILL_EVENT_ID.to_string()],
                "engine must receive ACK for the original SL/TP fill event_id",
            );
        }
        other => panic!("expected EventAck, got {other:?}"),
    }

    server.shutdown().await;
}

/// TEK-271 failure-mode witness: if the strategy never ACKs (the symptom a
/// regression in the auto-drain path would produce), the engine never
/// receives an `EventAck` and the backtest stalls.
///
/// Pairs with the round-trip test above to demonstrate the assertion is
/// load-bearing — without the ACK, `engine_rx` stays empty.
#[tokio::test]
async fn missing_strategy_ack_stalls_engine() {
    let (server, engine_tx, mut engine_rx, _registry, mut out_rx) = setup_pipeline().await;

    let fill = ServerMessage::order(
        SL_FILL_EVENT_ID,
        EngineOrderEventType::Filled,
        engine_sl_fill_order(),
    );
    send_server_message(&engine_tx, &fill);

    // Strategy still receives the event — the upstream path is unaffected.
    let received = timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("strategy never received SL/TP fill")
        .expect("strategy channel closed before fill arrived");
    assert!(matches!(received, WsMessage::Order { .. }));

    // No strategy ACK → engine sees nothing within a generous wait window.
    let result = timeout(Duration::from_millis(500), engine_rx.recv()).await;
    assert!(
        result.is_err(),
        "engine must NOT receive EventAck without strategy ACK; got {result:?}",
    );

    server.shutdown().await;
}
