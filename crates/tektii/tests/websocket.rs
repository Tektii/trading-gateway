//! WebSocket provider integration tests for the Tektii adapter.
//!
//! Tests the `WebSocketProvider::connect()` implementation against a real
//! `MockWsServer`, covering event routing, ACK flows, graceful shutdown,
//! and edge cases.

mod helpers;

use std::time::Duration;

use tektii_gateway_core::websocket::messages::{EventAckMessage, WsMessage};
use tektii_gateway_core::websocket::provider::WebSocketProvider;
use tektii_gateway_test_support::websocket::MockWsServer;
use tektii_protocol::rest::OrderEventType;
use tektii_protocol::websocket::{ClientMessage, ServerMessage};
use tokio::time::timeout;

use helpers::{
    create_ws_provider, make_engine_account, make_engine_order, make_engine_position,
    make_engine_trade, minimal_provider_config, parse_client_message,
};

/// Helper: send a ServerMessage as JSON text through the mock server.
fn send_server_message(tx: &tokio::sync::mpsc::UnboundedSender<String>, msg: &ServerMessage) {
    let json = serde_json::to_string(msg).expect("Failed to serialize ServerMessage");
    tx.send(json).expect("Failed to send to MockWsServer");
}

// =========================================================================
// Event Routing Tests
// =========================================================================

#[tokio::test]
async fn order_event_routed_to_event_stream() {
    let (server, tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    let msg = ServerMessage::order("evt-1", OrderEventType::Created, make_engine_order());
    send_server_message(&tx, &msg);

    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");

    match ws_msg {
        WsMessage::Order { event, order, .. } => {
            assert_eq!(
                event,
                tektii_gateway_core::websocket::messages::OrderEventType::OrderCreated
            );
            assert_eq!(order.symbol, "AAPL");
        }
        other => panic!("Expected WsMessage::Order, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn trade_event_routed_to_event_stream() {
    let (server, tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    let msg = ServerMessage::trade("evt-2", make_engine_trade());
    send_server_message(&tx, &msg);

    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");

    match ws_msg {
        WsMessage::Trade { trade, .. } => {
            assert_eq!(trade.symbol, "AAPL");
        }
        other => panic!("Expected WsMessage::Trade, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn position_event_routed_to_event_stream() {
    let (server, tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    let msg = ServerMessage::position("evt-3", make_engine_position());
    send_server_message(&tx, &msg);

    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");

    match ws_msg {
        WsMessage::Position { position, .. } => {
            assert_eq!(position.symbol, "AAPL");
        }
        other => panic!("Expected WsMessage::Position, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn account_event_routed_to_event_stream() {
    let (server, tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    let msg = ServerMessage::account_balance_updated("evt-4", make_engine_account());
    send_server_message(&tx, &msg);

    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");

    match ws_msg {
        WsMessage::Account { event, account, .. } => {
            assert_eq!(
                event,
                tektii_gateway_core::websocket::messages::AccountEventType::BalanceUpdated
            );
            assert_eq!(account.currency, "USD");
        }
        other => panic!("Expected WsMessage::Account, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn candle_event_routed_to_event_stream() {
    let (server, tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    use rust_decimal_macros::dec;
    let msg = ServerMessage::candle(
        "evt-5",
        "AAPL",
        "1m",
        1_704_067_200_000,
        dec!(150),
        dec!(152),
        dec!(149),
        dec!(151),
        10000.0,
    );
    send_server_message(&tx, &msg);

    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");

    match ws_msg {
        WsMessage::Candle { bar, .. } => {
            assert_eq!(bar.symbol, "AAPL");
            assert_eq!(
                bar.timeframe,
                tektii_gateway_core::models::Timeframe::OneMinute
            );
        }
        other => panic!("Expected WsMessage::Candle, got {other:?}"),
    }

    server.shutdown().await;
}

// =========================================================================
// Non-Broadcast Event Tests
// =========================================================================

#[tokio::test]
async fn error_event_produces_no_ws_message() {
    let (server, tx, mut rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    // Send error event (should not appear on EventStream)
    let error_msg = ServerMessage::error("evt-err", "E001", "something broke");
    send_server_message(&tx, &error_msg);

    // Send a valid order event after (should appear)
    let order_msg = ServerMessage::order("evt-ok", OrderEventType::Created, make_engine_order());
    send_server_message(&tx, &order_msg);

    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");

    // First message on EventStream should be the order, not the error
    assert!(matches!(ws_msg, WsMessage::Order { .. }));

    // Verify ACK was sent for the error's event_id (immediate ack since Error → None → immediate)
    let ack_raw = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for ACK")
        .expect("channel closed");

    let client_msg = parse_client_message(&ack_raw);
    match client_msg {
        ClientMessage::EventAck {
            events_processed, ..
        } => {
            assert!(events_processed.contains(&"evt-err".to_string()));
        }
        other => panic!("Expected EventAck, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn pong_produces_no_ws_message_and_no_ack() {
    let (server, tx, mut rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    // Send Pong (no event_id → no ACK, no WsMessage)
    let pong_msg = ServerMessage::pong();
    send_server_message(&tx, &pong_msg);

    // Send a valid order after
    let order_msg = ServerMessage::order("evt-ok", OrderEventType::Created, make_engine_order());
    send_server_message(&tx, &order_msg);

    // First message on EventStream should be the order
    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");
    assert!(matches!(ws_msg, WsMessage::Order { .. }));

    // Drain ACKs — we should only see ACK for "evt-ok", not for pong
    // Give tasks time to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut ack_event_ids = Vec::new();
    while let Ok(Some(raw)) = timeout(Duration::from_millis(200), rx.recv()).await {
        let client_msg = parse_client_message(&raw);
        if let ClientMessage::EventAck {
            events_processed, ..
        } = client_msg
        {
            ack_event_ids.extend(events_processed);
        }
    }

    // No ACK for pong (it has no event_id). evt-ok may or may not be ACK'd
    // depending on subscription filter — with empty filter it's pending.
    // The key assertion: no "pong" event_id in any ACK.
    assert!(
        !ack_event_ids.iter().any(|id| id.contains("pong")),
        "Pong should not generate any ACK"
    );

    server.shutdown().await;
}

// =========================================================================
// ACK Flow Tests
// =========================================================================

#[tokio::test]
async fn subscribed_event_acked_only_after_strategy_ack() {
    let (server, tx, mut rx) = MockWsServer::start().await;
    // Empty filter → all events subscribed → pending until strategy ACK
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    let msg = ServerMessage::order("evt-1", OrderEventType::Created, make_engine_order());
    send_server_message(&tx, &msg);

    // Wait for event to arrive on EventStream (proves it was processed)
    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");
    assert!(matches!(ws_msg, WsMessage::Order { .. }));

    // Short timeout: no ACK should have been sent yet (event is pending)
    let no_ack = timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        no_ack.is_err(),
        "ACK should NOT be sent before strategy ACK"
    );

    // Now send strategy ACK
    provider
        .handle_ack(EventAckMessage {
            correlation_id: "test-corr".to_string(),
            events_processed: vec!["evt-1".to_string()],
            timestamp: 0,
        })
        .await
        .expect("handle_ack failed");

    // Now the engine ACK should appear
    let ack_raw = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for engine ACK")
        .expect("channel closed");

    let client_msg = parse_client_message(&ack_raw);
    match client_msg {
        ClientMessage::EventAck {
            events_processed, ..
        } => {
            assert!(
                events_processed.contains(&"evt-1".to_string()),
                "ACK should contain evt-1, got: {events_processed:?}"
            );
        }
        other => panic!("Expected EventAck, got {other:?}"),
    }

    server.shutdown().await;
}

#[tokio::test]
async fn multiple_pending_events_batch_drained() {
    let (server, tx, mut rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    // Send 3 events
    for i in 1..=3 {
        let msg = ServerMessage::order(
            format!("evt-{i}"),
            OrderEventType::Created,
            make_engine_order(),
        );
        send_server_message(&tx, &msg);
    }

    // Wait for all 3 to arrive on EventStream
    for _ in 0..3 {
        timeout(Duration::from_secs(5), event_stream.recv())
            .await
            .expect("timeout")
            .expect("stream closed");
    }

    // No ACKs yet (all pending)
    let no_ack = timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        no_ack.is_err(),
        "No ACKs should be sent before strategy ACK"
    );

    // Single strategy ACK drains all pending
    provider
        .handle_ack(EventAckMessage {
            correlation_id: "batch".to_string(),
            events_processed: vec!["any".to_string()],
            timestamp: 0,
        })
        .await
        .expect("handle_ack failed");

    // Collect all ACK event_ids from the engine
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut all_acked_ids = Vec::new();
    while let Ok(Some(raw)) = timeout(Duration::from_millis(500), rx.recv()).await {
        let client_msg = parse_client_message(&raw);
        if let ClientMessage::EventAck {
            events_processed, ..
        } = client_msg
        {
            all_acked_ids.extend(events_processed);
        }
    }

    assert_eq!(
        all_acked_ids.len(),
        3,
        "All 3 events should be ACK'd, got: {all_acked_ids:?}"
    );
    assert!(all_acked_ids.contains(&"evt-1".to_string()));
    assert!(all_acked_ids.contains(&"evt-2".to_string()));
    assert!(all_acked_ids.contains(&"evt-3".to_string()));

    server.shutdown().await;
}

// =========================================================================
// Graceful Shutdown Tests
// =========================================================================

#[tokio::test]
async fn disconnect_closes_event_stream() {
    let (server, tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    // Verify connection works
    let msg = ServerMessage::order("evt-1", OrderEventType::Created, make_engine_order());
    send_server_message(&tx, &msg);

    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout")
        .expect("stream closed");
    assert!(matches!(ws_msg, WsMessage::Order { .. }));

    // Disconnect
    provider.disconnect().await.expect("disconnect failed");

    // EventStream should close (recv returns None)
    let result = timeout(Duration::from_secs(2), event_stream.recv())
        .await
        .expect("timeout — event stream should close promptly");
    assert!(
        result.is_none(),
        "EventStream should return None after disconnect"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn handle_ack_before_connect_does_not_panic() {
    let (server, _tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    // Call handle_ack before connect() — ACK bridge is None.
    // Should not panic; just logs a warning.
    let result = provider
        .handle_ack(EventAckMessage {
            correlation_id: "early".to_string(),
            events_processed: vec!["evt-1".to_string()],
            timestamp: 0,
        })
        .await;

    assert!(result.is_ok(), "handle_ack before connect should not fail");

    server.shutdown().await;
}

// =========================================================================
// Edge Case Tests
// =========================================================================

#[tokio::test]
async fn malformed_json_skipped_without_crash() {
    let (server, tx, _rx) = MockWsServer::start().await;
    let (provider, _broadcast_rx) = create_ws_provider(&server.url());

    let mut event_stream = provider
        .connect(minimal_provider_config())
        .await
        .expect("connect failed");

    // Send malformed JSON
    tx.send("this is not valid json {".to_string())
        .expect("send failed");

    // Send valid event after
    let msg = ServerMessage::order("evt-ok", OrderEventType::Created, make_engine_order());
    send_server_message(&tx, &msg);

    // Valid event should still arrive — provider didn't crash
    let ws_msg = timeout(Duration::from_secs(5), event_stream.recv())
        .await
        .expect("timeout — provider may have crashed on malformed JSON")
        .expect("stream closed");

    assert!(
        matches!(ws_msg, WsMessage::Order { .. }),
        "Valid event should arrive after malformed JSON"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn non_connection_refused_error_fails_immediately() {
    // Use an invalid host that won't trigger "Connection refused"
    // (DNS resolution failure or similar)
    let (provider, _broadcast_rx) = create_ws_provider("ws://invalid.host.that.does.not.exist:1");

    let result = timeout(
        Duration::from_secs(10),
        provider.connect(minimal_provider_config()),
    )
    .await
    .expect("timeout — non-connection-refused should fail fast, not retry 60 times");

    assert!(
        result.is_err(),
        "Should fail with ConnectionFailed for non-connection-refused error"
    );
}
