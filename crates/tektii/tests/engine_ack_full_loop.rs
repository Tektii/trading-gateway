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

/// Strict-ID release: the ACK names which event it acknowledges, and exactly
/// that event is released — not whatever happens to sit at the queue head.
#[tokio::test]
async fn ack_releases_the_exact_echoed_id_not_the_queue_head() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge.clone())
        .await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Two engine events delivered back-to-back.
    for id in ["evt-a", "evt-b"] {
        bridge.register_pending(id.to_string()).await;
        gw.event_tx
            .send(ProviderEvent::engine(
                WsMessage::quote(test_quote("AAPL")),
                id.to_string(),
            ))
            .expect("event stream open");
    }
    for _ in 0..2 {
        recv_quote_event_id(&mut client)
            .await
            .expect("strategy never received the quote");
    }

    // The strategy acks the SECOND event's id. Exactly evt-b must be
    // released — under positional release the head (evt-a) would leak out.
    client.send_json(&event_ack(&["evt-b"])).await;

    let released = tokio::time::timeout(TIMEOUT, engine_ack_rx.recv())
        .await
        .expect("engine never received the ACK")
        .expect("bridge engine ACK channel closed");
    assert_eq!(released, vec!["evt-b".to_string()]);
}

/// A lost delivery must not wedge the FIFO: an ACK for a *delivered* event
/// releases that event even when an older, never-delivered event still sits
/// at the queue head. Under positional release the ACK deferred forever
/// behind the wedged head and the engine starved to its halt threshold.
#[tokio::test]
async fn ack_for_a_delivered_event_releases_it_past_an_undelivered_head() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge.clone())
        .await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // evt-lost is registered pending but never delivered (dropped between the
    // engine WS read and the strategy connection — e.g. a send failure).
    bridge.register_pending("evt-lost".to_string()).await;

    // evt-next is delivered normally.
    bridge.register_pending("evt-next".to_string()).await;
    gw.event_tx
        .send(ProviderEvent::engine(
            WsMessage::quote(test_quote("AAPL")),
            "evt-next".to_string(),
        ))
        .expect("event stream open");
    let echoed = recv_quote_event_id(&mut client)
        .await
        .expect("strategy never received the quote")
        .expect("engine event_id was not echoed on the wire");
    assert_eq!(echoed, "evt-next");

    client.send_json(&event_ack(&[&echoed])).await;

    let released = tokio::time::timeout(TIMEOUT, engine_ack_rx.recv())
        .await
        .expect("ACK for a delivered event must not wedge behind an undelivered head")
        .expect("bridge engine ACK channel closed");
    assert_eq!(released, vec!["evt-next".to_string()]);
}

/// An ACK naming an id the bridge never registered must release nothing —
/// under positional release any non-empty ACK popped the queue head,
/// whatever id it carried.
#[tokio::test]
async fn ack_with_unknown_id_releases_nothing() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge.clone())
        .await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    bridge.register_pending("evt-real".to_string()).await;
    gw.event_tx
        .send(ProviderEvent::engine(
            WsMessage::quote(test_quote("AAPL")),
            "evt-real".to_string(),
        ))
        .expect("event stream open");
    recv_quote_event_id(&mut client)
        .await
        .expect("strategy never received the quote");

    client.send_json(&event_ack(&["evt-bogus"])).await;

    let result = tokio::time::timeout(Duration::from_millis(300), engine_ack_rx.recv()).await;
    assert!(
        result.is_err(),
        "an unknown-id ACK must not release the pending event, got {result:?}"
    );
}

/// One ACK frame naming several ids (a batch ACK over the real wire) must
/// release all of them — a regression that truncates `events_processed` to
/// its first element anywhere on the inbound path would fail this.
#[tokio::test]
async fn single_ack_frame_naming_multiple_ids_releases_all_of_them() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge.clone())
        .await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    for id in ["evt-b1", "evt-b2"] {
        bridge.register_pending(id.to_string()).await;
        gw.event_tx
            .send(ProviderEvent::engine(
                WsMessage::quote(test_quote("AAPL")),
                id.to_string(),
            ))
            .expect("event stream open");
    }
    for _ in 0..2 {
        recv_quote_event_id(&mut client)
            .await
            .expect("strategy never received the quote");
    }

    client.send_json(&event_ack(&["evt-b1", "evt-b2"])).await;

    let released = tokio::time::timeout(TIMEOUT, engine_ack_rx.recv())
        .await
        .expect("engine never received the batch ACK")
        .expect("bridge engine ACK channel closed");
    assert_eq!(released, vec!["evt-b1".to_string(), "evt-b2".to_string()]);
}

/// Duplicate ACKs for the same id are idempotent: the first releases the
/// event, the second finds nothing and releases nothing.
#[tokio::test]
async fn duplicate_ack_for_the_same_id_releases_once() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let (bridge, mut engine_ack_rx) = TektiiAckBridge::create();
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge.clone())
        .await;

    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    bridge.register_pending("evt-once".to_string()).await;
    gw.event_tx
        .send(ProviderEvent::engine(
            WsMessage::quote(test_quote("AAPL")),
            "evt-once".to_string(),
        ))
        .expect("event stream open");
    recv_quote_event_id(&mut client)
        .await
        .expect("strategy never received the quote");

    client.send_json(&event_ack(&["evt-once"])).await;
    client.send_json(&event_ack(&["evt-once"])).await;

    let released = tokio::time::timeout(TIMEOUT, engine_ack_rx.recv())
        .await
        .expect("first ACK must release the event")
        .expect("bridge engine ACK channel closed");
    assert_eq!(released, vec!["evt-once".to_string()]);

    let second = tokio::time::timeout(Duration::from_millis(300), engine_ack_rx.recv()).await;
    assert!(
        second.is_err(),
        "duplicate ACK must not release anything further, got {second:?}"
    );
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
