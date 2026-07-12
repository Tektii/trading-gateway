//! Engine-id ACK pacing: the strategy-facing ACK path paces the engine FIFO
//! only on ACKs that carry an engine `event_id`, and drops empty ones.
//!
//! The gateway echoes the engine `event_id` on outbound engine frames, so the
//! backtest SDK returns it in `events_processed`. Engine-paced events therefore
//! ACK with a non-empty payload and release the oldest delivered event (FIFO).
//! Gateway-local events (connection / data-freshness / error / rate-limit) carry
//! no engine id, so the SDK ACKs them with an empty payload — those must NOT pop
//! the FIFO, or they advance the engine ahead of the strategy and free-run sim
//! time into silent 0-trade backtests.

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
async fn non_empty_ack_releases_one_fifo_event_empty_ack_releases_none() {
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
    // yields an empty-payload ACK. It must be dropped — the engine FIFO is
    // untouched, so the engine cannot advance ahead of the strategy.
    registry.handle_strategy_ack(&[]).await;
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "empty events_processed ACK must not pop the engine FIFO"
    );

    // A real engine ACK carries the echoed `event_id` — non-empty. It releases
    // exactly the oldest delivered event; FIFO release order comes from delivery
    // position, not the acked id.
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

    // The next non-empty ACK releases the next event, in delivery order.
    registry.handle_strategy_ack(&["evt-1".to_string()]).await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("second non-empty ACK releases the next delivered event");
    assert_eq!(released, vec!["evt-1".to_string()]);
    assert!(
        engine_ack_rx.try_recv().is_err(),
        "exactly one event released per non-empty ACK"
    );
}

#[tokio::test]
async fn non_empty_ack_releases_fifo_head_regardless_of_the_acked_id() {
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    let registry = build_registry();
    registry.set_tektii_ack_bridge(bridge.clone()).await;

    bridge.register_pending("evt-0".to_string()).await;
    bridge.mark_sent(vec!["evt-0".to_string()]).await;

    // The payload only marks the ACK as engine-paced; release order comes from
    // delivery position, not the acked id. An unrelated/stale id in a non-empty
    // ACK must still release the true FIFO head — this guards against anyone
    // "fixing" the drop-guard into real id-correlation (the reverted #109).
    registry
        .handle_strategy_ack(&["some-unrelated-id".to_string()])
        .await;
    let released = engine_ack_rx
        .recv()
        .await
        .expect("non-empty ACK must release the FIFO head even with a stale id");
    assert_eq!(released, vec!["evt-0".to_string()]);
}
