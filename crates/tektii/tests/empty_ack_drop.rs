//! TEK-1312: the strategy-facing ACK path must DROP `EventAck` frames whose
//! `events_processed` is empty instead of forwarding them to the ACK bridge.
//!
//! The Python SDK emits an `event_ack` after every yielded event, with
//! `events_processed: []` for gateway-local events (connection / error /
//! rate-limit) that carry no engine id. Forwarding those popped the oldest
//! delivered ENGINE event off the FIFO, silently advancing the engine one
//! event ahead of the strategy — free-running sim time and producing silent
//! 0-trade backtests (TEK-1309). Legit backtest ACKs always carry the engine
//! `event_id`, so pacing is unaffected.

use std::sync::Arc;

use tektii_gateway_core::config::ReconnectionConfig;
use tektii_gateway_core::correlation::CorrelationStore;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::connection::WsConnectionManager;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_tektii::ack_bridge::TektiiAckBridge;
use tokio_util::sync::CancellationToken;

fn build_registry() -> Arc<ProviderRegistry> {
    Arc::new(ProviderRegistry::new(
        Arc::new(WsConnectionManager::new()),
        SubscriptionFilter::new(&[]),
        CancellationToken::new(),
        ReconnectionConfig::default(),
        Arc::new(CorrelationStore::new()),
    ))
}

#[tokio::test]
async fn empty_events_processed_ack_does_not_release_engine_event() {
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    let registry = build_registry();
    registry.set_tektii_ack_bridge(bridge.clone()).await;

    // Two engine events delivered to the strategy: registered pending and
    // marked sent (post `connection_manager.send_to`).
    bridge.register_pending("evt-0".to_string()).await;
    bridge.register_pending("evt-1".to_string()).await;
    bridge
        .mark_sent(vec!["evt-0".to_string(), "evt-1".to_string()])
        .await;

    // A gateway-local event (ConnectionEvent / ErrorEvent / rate-limit) yields
    // an empty-payload ACK. It must be dropped — the engine FIFO is untouched.
    registry.handle_strategy_ack(&[]).await;
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "empty events_processed ACK must not pop the engine FIFO"
    );

    // A real backtest ACK carries the engine event_id — releases exactly the
    // oldest delivered event; FIFO release semantics are unchanged.
    registry.handle_strategy_ack(&["evt-0".to_string()]).await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("non-empty ACK must release the oldest delivered event");
    assert_eq!(released, vec!["evt-0".to_string()]);
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "exactly one event released per non-empty ACK"
    );
}
