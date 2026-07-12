//! Auto-correlation: the strategy-facing ACK path forwards EVERY `EventAck` to
//! the ACK bridge, including ones whose `events_processed` is empty.
//!
//! The backtest SDK strips engine `event_id`s from the wire and uses
//! auto-correlation — one ACK releases the oldest delivered event by delivery
//! position, so the SDK's per-event ACKs legitimately carry an empty payload.
//! Dropping empty ACKs (the reverted #109) starved the FIFO and halted the
//! engine on consecutive candle-ACK timeouts. This test pins the restored
//! behaviour: an empty-payload ACK releases exactly the oldest delivered event.

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
async fn empty_events_processed_ack_releases_oldest_delivered_event() {
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

    // The SDK ACKs each delivered event with an empty payload. Under
    // auto-correlation that ACK must release the oldest delivered event — it
    // must NOT be dropped.
    registry.handle_strategy_ack().await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("empty-payload ACK must release the oldest delivered event");
    assert_eq!(released, vec!["evt-0".to_string()]);

    // The next ACK releases the next event — exactly one release per ACK.
    registry.handle_strategy_ack().await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("second ACK releases the next delivered event");
    assert_eq!(released, vec!["evt-1".to_string()]);

    assert!(
        engine_ack_rx.try_recv().is_err(),
        "exactly one event released per ACK"
    );
}
