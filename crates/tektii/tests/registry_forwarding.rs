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
use tektii_gateway_core::websocket::messages::{InstrumentSubscription, WsMessage};
use tektii_gateway_core::websocket::provider::WebSocketProvider;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_test_support::websocket::MockWsServer;
use tektii_protocol::websocket::ServerMessage;
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
