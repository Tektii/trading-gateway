//! Reproduction for TEK-268: gateway tektii adapter doesn't forward provider
//! events to subscribed strategies in backtest mode.
//!
//! Sets up the full provider→registry→strategy pipeline using
//! `MockWsServer` as the engine, registers a strategy with the
//! `ProviderRegistry`, sends a candle from the engine, and asserts the
//! candle reaches the strategy connection's outbound channel.

mod helpers;

use std::sync::Arc;
use std::time::Duration;

use rust_decimal_macros::dec;
use tektii_gateway_core::config::ReconnectionConfig;
use tektii_gateway_core::correlation::CorrelationStore;
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::connection::{WsConnection, WsConnectionManager};
use tektii_gateway_core::websocket::messages::{
    EventAckMessage, InstrumentSubscription, WsMessage,
};
use tektii_gateway_core::websocket::provider::WebSocketProvider;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_test_support::websocket::MockWsServer;
use tektii_protocol::websocket::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use helpers::{create_ws_provider, minimal_provider_config};

fn send_server_message(tx: &tokio::sync::mpsc::UnboundedSender<String>, msg: &ServerMessage) {
    let json = serde_json::to_string(msg).expect("serialize ServerMessage");
    tx.send(json).expect("send to MockWsServer");
}

/// TEK-268: candle events from the engine reach a strategy registered with
/// the `ProviderRegistry`. Baseline transparency assertion — Tektii declares
/// `filters_events_upstream() = true`, registry forwards every event.
#[tokio::test]
async fn candle_from_engine_reaches_registered_strategy() {
    let (server, engine_tx, _engine_rx) = MockWsServer::start().await;

    // Build the Tektii provider and connect it to the mock engine.
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());
    let stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("provider.connect should succeed");

    // Build the gateway-side ProviderRegistry exactly the way main.rs does.
    let ws_manager = Arc::new(WsConnectionManager::new());
    let cancel = CancellationToken::new();
    let registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]),
        cancel.clone(),
        ReconnectionConfig::default(),
        Arc::new(CorrelationStore::new()),
    ));

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

    // Register a strategy connection — emulates what server.rs does on a
    // successful `/v1/ws` upgrade.
    let (out_tx, mut out_rx) = mpsc::channel::<WsMessage>(1024);
    let conn = WsConnection::new(out_tx, "127.0.0.1:1".parse().unwrap());
    let conn_id = conn.id;
    ws_manager.add_connection(conn).await;
    registry.register_strategy_connection(conn_id).await;

    // Engine sends a candle, just like the backtest scenario does.
    let candle = ServerMessage::candle(
        "evt-candle-1",
        "EUR/USD",
        "1m",
        1_775_001_720_000,
        dec!(1.08),
        dec!(1.081),
        dec!(1.079),
        dec!(1.0805),
        100.0,
    );
    send_server_message(&engine_tx, &candle);

    // The strategy connection should receive the candle.
    let msg = timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("strategy never received candle (TEK-268)")
        .expect("channel closed before candle arrived");

    match msg {
        WsMessage::Candle { bar, .. } => {
            assert_eq!(bar.symbol, "EUR/USD");
        }
        other => panic!("expected Candle, got {other:?}"),
    }

    server.shutdown().await;
}

/// TEK-268 reproduction with EXACT production config: subscriptions for
/// platform=tektii, instrument="F:EURUSD", events=["candle_1m"], engine emits
/// Candle with symbol="F:EURUSD" timeframe="1m".
#[tokio::test]
async fn candle_reaches_strategy_with_production_subscriptions() {
    let (server, engine_tx, _engine_rx) = MockWsServer::start().await;

    let (provider, _broadcast_rx) = create_ws_provider(&server.url());
    let stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("provider.connect should succeed");

    let subs = vec![InstrumentSubscription::new(
        TradingPlatform::Tektii,
        "F:EURUSD".into(),
        vec!["candle_1m".into()],
    )];
    let registry_filter = SubscriptionFilter::new(&subs);

    let ws_manager = Arc::new(WsConnectionManager::new());
    let cancel = CancellationToken::new();
    let registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        registry_filter,
        cancel.clone(),
        ReconnectionConfig::default(),
        Arc::new(CorrelationStore::new()),
    ));

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

    let (out_tx, mut out_rx) = mpsc::channel::<WsMessage>(1024);
    let conn = WsConnection::new(out_tx, "127.0.0.1:1".parse().unwrap());
    let conn_id = conn.id;
    ws_manager.add_connection(conn).await;
    registry.register_strategy_connection(conn_id).await;

    let candle = ServerMessage::candle(
        "evt-candle-1",
        "F:EURUSD",
        "1m",
        1_775_001_720_000,
        dec!(1.08),
        dec!(1.081),
        dec!(1.079),
        dec!(1.0805),
        100.0,
    );
    send_server_message(&engine_tx, &candle);

    let msg = timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("strategy never received candle (TEK-268)")
        .expect("channel closed before candle arrived");

    match msg {
        WsMessage::Candle { bar, .. } => {
            assert_eq!(bar.symbol, "F:EURUSD");
        }
        other => panic!("expected Candle, got {other:?}"),
    }

    server.shutdown().await;
}

/// TEK-268: even when `SUBSCRIPTIONS` would otherwise gate the candle's
/// platform, the registry must forward Tektii events because the provider
/// declares it filters upstream. Pre-fix, the registry's filter silently
/// dropped these — exact production symptom: provider connected, strategy
/// registered with `providers = [Tektii]`, but no events ever reached the
/// strategy.
#[tokio::test]
async fn candle_passes_through_when_subscriptions_target_other_platform() {
    let (server, engine_tx, _engine_rx) = MockWsServer::start().await;

    let (provider, _broadcast_rx) = create_ws_provider(&server.url());
    let stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("provider.connect should succeed");

    // Realistic gateway config: SUBSCRIPTIONS is non-empty but only references
    // a non-Tektii platform (Alpaca). The registry's filter therefore is
    // non-empty, but has no entry for `Tektii` — and `matches_candle` returns
    // `false` for any Tektii event.
    let subs = vec![InstrumentSubscription::new(
        TradingPlatform::AlpacaPaper,
        "AAPL".into(),
        vec!["candle_1m".into()],
    )];
    let registry_filter = SubscriptionFilter::new(&subs);

    let ws_manager = Arc::new(WsConnectionManager::new());
    let cancel = CancellationToken::new();
    let registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        registry_filter,
        cancel.clone(),
        ReconnectionConfig::default(),
        Arc::new(CorrelationStore::new()),
    ));

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

    let (out_tx, mut out_rx) = mpsc::channel::<WsMessage>(1024);
    let conn = WsConnection::new(out_tx, "127.0.0.1:1".parse().unwrap());
    let conn_id = conn.id;
    ws_manager.add_connection(conn).await;
    registry.register_strategy_connection(conn_id).await;

    let candle = ServerMessage::candle(
        "evt-candle-1",
        "EUR/USD",
        "1m",
        1_775_001_720_000,
        dec!(1.08),
        dec!(1.081),
        dec!(1.079),
        dec!(1.0805),
        100.0,
    );
    send_server_message(&engine_tx, &candle);

    let msg = timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("strategy never received candle (TEK-268)")
        .expect("channel closed before candle arrived");

    match msg {
        WsMessage::Candle { bar, .. } => {
            assert_eq!(bar.symbol, "EUR/USD");
        }
        other => panic!("expected Candle, got {other:?}"),
    }

    server.shutdown().await;
}

/// TEK-270 (registry path): once an event has flowed through the registry's
/// broadcast loop and `connection_manager.send_to` has returned Ok, the
/// registry must call `AckBridge::mark_sent`. The strategy's subsequent ACK
/// then drains the event and the engine receives an `EventAck` over the
/// engine WebSocket.
///
/// Pre-fix, the registry didn't call `mark_sent` at all (the field didn't
/// exist) — events would stay `sent = false` forever and `handle_strategy_ack`
/// would drain nothing, so this test would time out waiting for the engine
/// EventAck.
#[tokio::test]
async fn engine_receives_ack_after_strategy_acks_event_delivered_through_registry() {
    let (server, engine_tx, mut engine_rx) = MockWsServer::start().await;

    let (provider, _broadcast_rx) = create_ws_provider(&server.url());
    let stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("provider.connect should succeed");

    // Capture the bridge BEFORE moving the provider into the registry.
    let bridge = provider
        .ack_bridge()
        .await
        .expect("ack_bridge available after connect");

    let ws_manager = Arc::new(WsConnectionManager::new());
    let cancel = CancellationToken::new();
    let registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]),
        cancel.clone(),
        ReconnectionConfig::default(),
        Arc::new(CorrelationStore::new()),
    ));

    // Wire the bridge to the registry the way `main.rs` does. Without this,
    // the registry's broadcast loop has no bridge to call `mark_sent` on.
    registry.set_tektii_ack_bridge(bridge.clone()).await;

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

    let (out_tx, mut out_rx) = mpsc::channel::<WsMessage>(1024);
    let conn = WsConnection::new(out_tx, "127.0.0.1:1".parse().unwrap());
    let conn_id = conn.id;
    ws_manager.add_connection(conn).await;
    registry.register_strategy_connection(conn_id).await;

    let candle = ServerMessage::candle(
        "evt-candle-1",
        "EUR/USD",
        "1m",
        1_775_001_720_000,
        dec!(1.08),
        dec!(1.081),
        dec!(1.079),
        dec!(1.0805),
        100.0,
    );
    send_server_message(&engine_tx, &candle);

    // Strategy receives the candle through the registry pipeline.
    let strategy_msg = timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("strategy never received candle")
        .expect("channel closed before candle arrived");
    assert!(matches!(strategy_msg, WsMessage::Candle { .. }));

    // Strategy ACKs. The registry should already have called `mark_sent`
    // post `send_to`, so this drains and forwards to the engine.
    bridge.handle_strategy_ack().await;

    // Engine receives the EventAck on its WebSocket — proves the registry
    // correctly called `mark_sent`.
    let ack_raw = timeout(Duration::from_secs(2), engine_rx.recv())
        .await
        .expect("engine never received EventAck (registry didn't call mark_sent?)")
        .expect("engine_rx closed");
    let ack_msg: ClientMessage = serde_json::from_str(&ack_raw).expect("parse ClientMessage");
    match ack_msg {
        ClientMessage::EventAck {
            events_processed, ..
        } => {
            assert!(
                events_processed.contains(&"evt-candle-1".to_string()),
                "EventAck should contain evt-candle-1, got: {events_processed:?}"
            );
        }
        other => panic!("expected EventAck, got {other:?}"),
    }

    server.shutdown().await;
}

/// TEK-270 (race fix, end-to-end): events that have NOT yet completed registry
/// broadcast must remain pending across a strategy ACK. Validates that
/// `provider.handle_ack` (the path actually wired up to incoming strategy
/// EventAck frames) correctly drains only `sent = true` entries — events
/// still in flight inside the registry pipeline are left alone.
#[tokio::test]
async fn strategy_ack_does_not_drain_in_flight_events() {
    let (server, _engine_tx, mut engine_rx) = MockWsServer::start().await;

    let (provider, _broadcast_rx) = create_ws_provider(&server.url());
    let _stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("provider.connect should succeed");

    let bridge = provider
        .ack_bridge()
        .await
        .expect("ack_bridge available after connect");

    // Engine emitted three events. All registered pending at WS-read time
    // (sent = false).
    bridge.register_pending("evt-1".to_string()).await;
    bridge.register_pending("evt-2".to_string()).await;
    bridge.register_pending("evt-3".to_string()).await;

    // Registry's broadcast loop has only completed `send_to` for evt-1.
    // evt-2 and evt-3 are still in flight inside the registry pipeline.
    bridge.mark_sent(vec!["evt-1".to_string()]).await;

    // Strategy ACKs. Pre-fix, this would drain ALL three (race fires →
    // engine ACK includes evt-2 and evt-3 even though strategy never saw
    // them). Post-fix, only evt-1 is drained.
    provider
        .handle_ack(EventAckMessage {
            correlation_id: "race-test".to_string(),
            events_processed: vec!["evt-1".to_string()],
            timestamp: 0,
        })
        .await
        .expect("handle_ack should succeed");

    let ack_raw = timeout(Duration::from_secs(2), engine_rx.recv())
        .await
        .expect("engine should receive ACK for evt-1")
        .expect("engine_rx closed");
    let ack_msg: ClientMessage = serde_json::from_str(&ack_raw).expect("parse ClientMessage");
    match ack_msg {
        ClientMessage::EventAck {
            events_processed, ..
        } => {
            assert_eq!(
                events_processed,
                vec!["evt-1".to_string()],
                "Engine should receive ACK for ONLY evt-1, not in-flight events. Got: {events_processed:?}"
            );
        }
        other => panic!("expected EventAck, got {other:?}"),
    }

    // No more ACKs for the in-flight events — they remain pending.
    let extra = timeout(Duration::from_millis(200), engine_rx.recv()).await;
    assert!(
        extra.is_err(),
        "Engine should NOT receive ACK for events still in flight to the WS"
    );

    server.shutdown().await;
}
