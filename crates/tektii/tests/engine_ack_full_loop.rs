//! End-to-end seam test: the engine `event_id` echoed on the outbound wire
//! frame, sent back by the strategy in `events_processed`, must release exactly
//! that event from the engine ACK FIFO — and an empty strategy ACK must not.
//!
//! The per-half tests (`engine_id_ack_pacing.rs`, `e2e_engine_event_id_echo.rs`)
//! cover the registry drop-guard and the outbound serialization independently.
//! This test glues them through the real axum server + `ProviderRegistry` +
//! `TektiiAckBridge`, so a field-name mismatch between what the gateway writes
//! (`event_id`) and what it reads back off an inbound `EventAck`
//! (`events_processed`) is caught — the exact class of regression behind
//! TEK-1309 / TEK-1312 / TEK-1331.

use std::time::Duration;

use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_core::websocket::provider::ProviderEvent;
use tektii_gateway_tektii::ack_bridge::TektiiAckBridge;
use tektii_gateway_test_support::harness::{StrategyClient, spawn_test_gateway};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::test_quote;

const TIMEOUT: Duration = Duration::from_secs(2);

/// Read raw frames until a `quote`, returning its echoed `event_id` (if any).
async fn recv_quote_event_id(client: &mut StrategyClient) -> Option<Option<String>> {
    for _ in 0..8 {
        let text = client.recv_raw(TIMEOUT).await?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        if value.get("type").and_then(serde_json::Value::as_str) == Some("quote") {
            return Some(
                value
                    .get("event_id")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from),
            );
        }
    }
    None
}

fn event_ack(events_processed: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "event_ack",
        "correlation_id": "test-corr",
        "events_processed": events_processed,
        "timestamp": 0,
    })
}

#[tokio::test]
async fn echoed_id_acked_by_strategy_releases_the_engine_fifo() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge.clone())
        .await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The tektii provider registers the engine event pending before broadcast;
    // the registry marks it sent once delivered to the strategy.
    bridge.register_pending("evt-loop-1".to_string()).await;
    gw.event_tx
        .send(ProviderEvent::engine(
            WsMessage::quote(test_quote("AAPL")),
            "evt-loop-1".to_string(),
        ))
        .expect("event stream open");

    // The strategy reads the echoed id off the wire and ACKs with it — exactly
    // what the backtest SDK does via `BaseEvent.event_id`.
    let echoed = recv_quote_event_id(&mut client)
        .await
        .expect("strategy never received the quote")
        .expect("engine event_id was not echoed on the wire");
    assert_eq!(echoed, "evt-loop-1");

    client.send_json(&event_ack(&[&echoed])).await;

    let released = tokio::time::timeout(TIMEOUT, engine_ack_rx.recv())
        .await
        .expect("engine never received the ACK — the wire/pacing loop is broken")
        .expect("bridge engine ACK channel closed");
    assert_eq!(released, vec!["evt-loop-1".to_string()]);
}

#[tokio::test]
async fn empty_strategy_ack_does_not_release_the_engine_fifo() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge.clone())
        .await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    bridge.register_pending("evt-loop-1".to_string()).await;
    gw.event_tx
        .send(ProviderEvent::engine(
            WsMessage::quote(test_quote("AAPL")),
            "evt-loop-1".to_string(),
        ))
        .expect("event stream open");

    // Drain the delivered frame so the event is marked sent (releasable).
    recv_quote_event_id(&mut client)
        .await
        .expect("strategy never received the quote");

    // A gateway-local event yields an empty-payload ACK. Even with a sent event
    // sitting in the FIFO, it must be dropped and release nothing.
    client.send_json(&event_ack(&[])).await;

    let result = tokio::time::timeout(Duration::from_millis(300), engine_ack_rx.recv()).await;
    assert!(
        result.is_err(),
        "empty strategy ACK must not release the engine FIFO, got {result:?}"
    );
}
