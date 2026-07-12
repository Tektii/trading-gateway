//! The gateway echoes the engine `event_id` on outbound engine frames so the
//! backtest SDK returns it in `events_processed` — letting the gateway tell
//! engine-paced ACKs (non-empty) from gateway-local ACKs (empty). Gateway-local
//! and live-provider events carry no id and must not gain an `event_id` field.

use std::time::Duration;

use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_core::websocket::provider::ProviderEvent;
use tektii_gateway_test_support::harness::{StrategyClient, spawn_test_gateway};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::models::test_quote;

const TIMEOUT: Duration = Duration::from_secs(2);

/// Read raw frames until one whose `type` matches `msg_type`, or `None`.
async fn recv_frame_of_type(
    client: &mut StrategyClient,
    msg_type: &str,
) -> Option<serde_json::Value> {
    for _ in 0..8 {
        let text = client.recv_raw(TIMEOUT).await?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        if value.get("type").and_then(serde_json::Value::as_str) == Some(msg_type) {
            return Some(value);
        }
    }
    None
}

#[tokio::test]
async fn engine_event_echoes_event_id_to_strategy() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // An engine-paced event carries the engine event_id on its envelope.
    let quote = WsMessage::quote(test_quote("AAPL"));
    gw.event_tx
        .send(ProviderEvent::engine(quote, "evt-echo-1".to_string()))
        .expect("event stream open");

    let frame = recv_frame_of_type(&mut client, "quote")
        .await
        .expect("strategy should receive the quote frame");

    assert_eq!(
        frame.get("event_id").and_then(serde_json::Value::as_str),
        Some("evt-echo-1"),
        "engine event_id must be echoed at the top level of the frame"
    );
}

#[tokio::test]
async fn gateway_local_event_has_no_event_id() {
    let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
    let mut client = StrategyClient::connect(&gw).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A gateway-local / live-provider event has no engine id on its envelope.
    gw.inject_event(WsMessage::quote(test_quote("AAPL")));

    let frame = recv_frame_of_type(&mut client, "quote")
        .await
        .expect("strategy should receive the quote frame");

    assert!(
        frame.get("event_id").is_none(),
        "gateway-local event must not carry an event_id field, got: {frame}"
    );
}
