//! Engine-id ACK pacing: the strategy-facing ACK path releases exactly the
//! engine events the ACK names (strict-ID), and releases nothing for empty
//! or unknown payloads.
//!
//! The gateway echoes the engine `event_id` on outbound engine frames, so the
//! backtest SDK returns it in `events_processed`. Engine-paced events
//! therefore ACK with the id of the event they answer, and exactly that event
//! is released. Gateway-local events (connection / data-freshness / error /
//! rate-limit) carry no engine id, so the SDK ACKs them with an empty payload
//! — those release nothing, or they would advance the engine ahead of the
//! strategy and free-run sim time into silent 0-trade backtests.

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
async fn ack_releases_the_named_event_empty_ack_releases_none() {
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

    // A gateway-local event (connection / data-freshness / error / rate-limit)
    // yields an empty-payload ACK. It releases nothing — the engine FIFO is
    // untouched, so the engine cannot advance ahead of the strategy.
    registry.handle_strategy_ack(&[]).await;
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "empty events_processed ACK must not release any engine event"
    );

    // A real engine ACK carries the echoed `event_id` — exactly that event is
    // released.
    registry.handle_strategy_ack(&["evt-0".to_string()]).await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("ACK must release the event it names");
    assert_eq!(released, vec!["evt-0".to_string()]);
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "exactly the named event released per ACK"
    );

    registry.handle_strategy_ack(&["evt-1".to_string()]).await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("second ACK releases the event it names");
    assert_eq!(released, vec!["evt-1".to_string()]);
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "exactly the named event released per ACK"
    );
}

#[tokio::test]
async fn ack_with_unknown_id_releases_nothing() {
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    let registry = build_registry();
    registry.set_tektii_ack_bridge(bridge.clone()).await;

    bridge.register_pending("evt-0".to_string()).await;
    bridge.mark_sent(vec!["evt-0".to_string()]).await;

    // Under positional release any non-empty ACK popped the FIFO head,
    // whatever id it carried — a confused or stale client could advance the
    // sim. Strict-ID release: an id the bridge never registered releases
    // nothing.
    registry
        .handle_strategy_ack(&["some-unrelated-id".to_string()])
        .await;
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "an unknown id must not release the pending engine event"
    );

    // The event is still releasable by its own id.
    registry.handle_strategy_ack(&["evt-0".to_string()]).await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("the named event must still be releasable");
    assert_eq!(released, vec!["evt-0".to_string()]);
}
