//! Integration tests for broker WebSocket disconnect and reconnection.
//!
//! Tests the full orchestration in `ProviderRegistry::spawn_provider_event_task()`:
//! disconnect detection → event emission → staleness marking → exponential
//! backoff → provider reconnect → reconciliation → resume streaming.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use tektii_gateway_core::config::ReconnectionConfig;
use tektii_gateway_core::correlation::CorrelationStore;
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::connection::{WsConnection, WsConnectionManager};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{ConnectionEventType, WsMessage};
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_test_support::mock_provider::MockWebSocketProvider;

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;

/// Tiny backoff values for fast tests.
fn fast_reconnection_config() -> ReconnectionConfig {
    ReconnectionConfig {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(10),
        max_retry_duration: Duration::from_millis(200),
    }
}

/// Create shared infrastructure for reconnection tests.
fn setup(
    config: ReconnectionConfig,
) -> (
    Arc<WsConnectionManager>,
    Arc<ProviderRegistry>,
    CancellationToken,
) {
    let ws_manager = Arc::new(WsConnectionManager::new());
    let cancel = CancellationToken::new();
    let correlation_store = Arc::new(CorrelationStore::new());
    let registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]),
        cancel.clone(),
        config,
        correlation_store,
    ));
    (ws_manager, registry, cancel)
}

/// Add a strategy connection and return the receiver for inspecting messages.
///
/// Registers with both `WsConnectionManager` (for broadcast) and
/// `ProviderRegistry` (for per-strategy `send_to` in the inner event loop).
async fn strategy_connection(
    manager: &WsConnectionManager,
    registry: &ProviderRegistry,
) -> mpsc::Receiver<WsMessage> {
    let (tx, rx) = mpsc::channel(1024);
    let addr = "127.0.0.1:9999".parse().unwrap();
    let conn = WsConnection::new(tx, addr);
    let id = conn.id;
    manager.add_connection(conn).await;
    registry.register_strategy_connection(id).await;
    rx
}

/// Receive the next connection event from the strategy receiver, with timeout.
async fn recv_connection_event(
    rx: &mut mpsc::Receiver<WsMessage>,
) -> (ConnectionEventType, Option<String>, Option<u64>) {
    let msg = timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for message")
        .expect("channel closed");

    match msg {
        WsMessage::Connection {
            event,
            broker,
            gap_duration_ms,
            ..
        } => (event, broker, gap_duration_ms),
        other => panic!("expected Connection event, got {other:?}"),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn disconnect_emits_broker_disconnected_to_strategies() {
    let (ws_manager, registry, _cancel) = setup(fast_reconnection_config());
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    let provider = MockWebSocketProvider::new(true);

    // Push a successful reconnect so the task doesn't spin forever
    let (new_tx, new_stream) = MockWebSocketProvider::make_event_stream();
    provider.push_reconnect_result(Ok(new_stream));

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    // Drop sender to simulate broker disconnect
    drop(tx);

    let (event, broker, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);
    assert_eq!(broker.as_deref(), Some("alpaca-paper"));

    // Clean up: drop new_tx to let the task finish
    drop(new_tx);
}

#[tokio::test]
async fn successful_reconnect_emits_broker_reconnected_with_gap() {
    let (ws_manager, registry, _cancel) = setup(fast_reconnection_config());
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    let provider = MockWebSocketProvider::new(true);

    let (new_tx, new_stream) = MockWebSocketProvider::make_event_stream();
    provider.push_reconnect_result(Ok(new_stream));

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    drop(tx);

    // First event: BrokerDisconnected
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);

    // Second event: BrokerReconnected with gap duration
    let (event, broker, gap_ms) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerReconnected);
    assert_eq!(broker.as_deref(), Some("alpaca-paper"));
    assert!(
        gap_ms.is_some(),
        "BrokerReconnected should include gap_duration_ms"
    );

    drop(new_tx);
}

#[tokio::test]
async fn reconnect_resumes_event_streaming() {
    let (ws_manager, registry, _cancel) = setup(fast_reconnection_config());
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    let provider = MockWebSocketProvider::new(true);

    let (new_tx, new_stream) = MockWebSocketProvider::make_event_stream();
    provider.push_reconnect_result(Ok(new_stream));

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    // Send an event on the original stream, verify it arrives
    let quote = tektii_gateway_test_support::models::test_quote("AAPL");
    tx.send(WsMessage::quote(quote)).unwrap();

    let msg = timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout")
        .expect("closed");
    assert!(
        matches!(msg, WsMessage::QuoteData { .. }),
        "expected QuoteData, got {msg:?}"
    );

    // Disconnect
    drop(tx);

    // Drain disconnect + reconnect events
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerReconnected);

    // Send event on the NEW stream — should arrive at strategy
    let quote2 = tektii_gateway_test_support::models::test_quote("BTCUSD");
    new_tx.send(WsMessage::quote(quote2)).unwrap();

    let msg = timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout")
        .expect("closed");
    assert!(
        matches!(msg, WsMessage::QuoteData { .. }),
        "expected QuoteData after reconnect, got {msg:?}"
    );

    drop(new_tx);
}

#[tokio::test]
async fn max_retry_exceeded_emits_broker_connection_failed() {
    let config = ReconnectionConfig {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(1),
        max_retry_duration: Duration::from_millis(20),
    };
    let (ws_manager, registry, _cancel) = setup(config);
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    // Provider with reconnection support but no successful reconnect results
    let provider = MockWebSocketProvider::new(true);

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    drop(tx);

    // BrokerDisconnected
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);

    // BrokerConnectionFailed (after max_retry_duration exceeded)
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerConnectionFailed);

    // No BrokerReconnected should follow
    let result = timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(result.is_err(), "should not receive any more events");
}

#[tokio::test]
async fn permanent_auth_error_stops_retries_immediately() {
    let (ws_manager, registry, _cancel) = setup(fast_reconnection_config());
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    let provider = MockWebSocketProvider::new(true);
    let handle = provider.handle();
    provider.push_reconnect_result(Err(WebSocketError::PermanentAuthError(
        "invalid API key".into(),
    )));

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    drop(tx);

    // BrokerDisconnected
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);

    // BrokerConnectionFailed (immediate, no further retries)
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerConnectionFailed);

    // Only 1 reconnect attempt
    assert_eq!(handle.reconnect_call_count(), 1);
}

#[tokio::test]
async fn no_reconnection_support_emits_disconnected_and_exits() {
    let (ws_manager, registry, _cancel) = setup(fast_reconnection_config());
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    let provider = MockWebSocketProvider::new(false);
    let handle = provider.handle();

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    drop(tx);

    // BrokerDisconnected (and task exits)
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);

    // No reconnection attempted
    assert_eq!(handle.reconnect_call_count(), 0);

    // No further events
    let result = timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(
        result.is_err(),
        "should not receive any more events after disconnect without reconnection support"
    );
}

#[tokio::test]
async fn instruments_marked_stale_on_disconnect_cleared_on_fresh_tick() {
    let (ws_manager, registry, _cancel) = setup(fast_reconnection_config());
    let _rx = strategy_connection(&ws_manager, &registry).await;

    let provider = MockWebSocketProvider::new(true);

    let symbols = vec!["AAPL".to_string(), "BTCUSD".to_string()];

    let (new_tx, new_stream) = MockWebSocketProvider::make_event_stream();
    provider.push_reconnect_result(Ok(new_stream));

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, symbols, vec![])
        .await
        .unwrap();

    // Disconnect
    drop(tx);

    // Wait for reconnect to complete (BrokerReconnected means staleness is marked and reconnect done)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Both instruments should be stale after disconnect
    let stale = registry.get_staleness(PLATFORM).await.unwrap();
    assert!(
        stale.contains(&"AAPL".to_string()),
        "AAPL should be stale before tick"
    );
    assert!(
        stale.contains(&"BTCUSD".to_string()),
        "BTCUSD should be stale before tick"
    );

    // Send a quote for AAPL — should clear its staleness
    let quote = tektii_gateway_test_support::models::test_quote("AAPL");
    new_tx.send(WsMessage::quote(quote)).unwrap();

    // Allow the event to be processed
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stale = registry.get_staleness(PLATFORM).await.unwrap();
    assert!(
        !stale.contains(&"AAPL".to_string()),
        "AAPL should be fresh after quote tick"
    );
    assert!(
        stale.contains(&"BTCUSD".to_string()),
        "BTCUSD should still be stale (no tick yet)"
    );

    drop(new_tx);
}

#[tokio::test]
async fn shutdown_during_reconnection_backoff_exits_cleanly() {
    let config = ReconnectionConfig {
        initial_backoff: Duration::from_secs(60), // Long backoff — shutdown should interrupt
        max_backoff: Duration::from_secs(60),
        max_retry_duration: Duration::from_secs(300),
    };
    let (ws_manager, registry, cancel) = setup(config);
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    let provider = MockWebSocketProvider::new(true);
    // No reconnect results — it will try to backoff with a 60s sleep

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    drop(tx);

    // Receive BrokerDisconnected
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);

    // Cancel during backoff sleep
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancel.cancel();

    // Should NOT receive BrokerConnectionFailed (shutdown takes priority)
    // The channel may close when the task exits
    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    match result {
        Ok(Some(msg)) => {
            // If we receive a message, it should NOT be BrokerConnectionFailed
            if let WsMessage::Connection { event, .. } = &msg {
                assert_ne!(
                    *event,
                    ConnectionEventType::BrokerConnectionFailed,
                    "should not emit BrokerConnectionFailed on shutdown"
                );
            }
        }
        Ok(None) => {} // Channel closed — task exited cleanly
        Err(_) => {}   // Timeout — task exited without sending more messages
    }
}

#[tokio::test]
async fn reconnect_triggers_reconciliation() {
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use tektii_gateway_core::events::router::EventRouter;
    use tektii_gateway_core::exit_management::ExitHandler;
    use tektii_gateway_core::models::OrderStatus;
    use tektii_gateway_core::state::StateManager;
    use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
    use tektii_gateway_test_support::models::test_order;

    let (ws_manager, registry, _cancel) = setup(fast_reconnection_config());
    let mut rx = strategy_connection(&ws_manager, &registry).await;

    // Set up EventRouter with MockTradingAdapter
    let state_manager = Arc::new(StateManager::new());
    let exit_handler: Arc<dyn tektii_gateway_core::exit_management::ExitHandling> = Arc::new(
        ExitHandler::with_defaults(Arc::clone(&state_manager), PLATFORM),
    );
    let (broadcast_tx, _broadcast_rx) = tokio::sync::broadcast::channel(64);
    let router = Arc::new(EventRouter::new(
        Arc::clone(&state_manager),
        exit_handler,
        broadcast_tx,
        PLATFORM,
    ));

    // Pre-populate StateManager with an Open order
    let open_order = tektii_gateway_core::models::Order {
        id: "order-1".into(),
        status: OrderStatus::Open,
        filled_quantity: Decimal::ZERO,
        ..test_order()
    };
    state_manager.upsert_order(&open_order);

    // MockTradingAdapter returns the same order as Filled
    let filled_order = tektii_gateway_core::models::Order {
        id: "order-1".into(),
        status: OrderStatus::Filled,
        filled_quantity: dec!(1),
        remaining_quantity: Decimal::ZERO,
        average_fill_price: Some(dec!(50000)),
        ..test_order()
    };
    let mock_adapter = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(filled_order));

    // Register event router (must happen before provider registration so it's
    // available when the spawned task runs reconciliation)
    registry
        .register_event_router(
            PLATFORM,
            router,
            mock_adapter as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>,
        )
        .await
        .unwrap();

    // Register provider with reconnect result
    let provider = MockWebSocketProvider::new(true);
    let (new_tx, new_stream) = MockWebSocketProvider::make_event_stream();
    provider.push_reconnect_result(Ok(new_stream));

    let (tx, stream) = MockWebSocketProvider::make_event_stream();
    registry
        .register_provider(PLATFORM, Box::new(provider), stream, vec![], vec![])
        .await
        .unwrap();

    // Disconnect
    drop(tx);

    // Collect events: expect BrokerDisconnected, then BrokerReconnected
    // Reconciliation events go through the broadcast channel, not the strategy WS.
    // But BrokerReconnected after reconciliation confirms reconciliation ran.
    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerDisconnected);

    let (event, _, _) = recv_connection_event(&mut rx).await;
    assert_eq!(event, ConnectionEventType::BrokerReconnected);

    // Verify reconciliation actually ran: the order should have been detected
    // as filled and removed from open orders in state manager (sync_from_provider
    // only keeps Open/PartiallyFilled/Pending).
    // Give a moment for async reconciliation to complete
    tokio::time::sleep(Duration::from_millis(50)).await;

    // After reconciliation + sync, the filled order should not be in state manager
    // (sync_from_provider replaces with only open orders from provider)
    let cached_order = state_manager.get_order("order-1");
    assert!(
        cached_order.is_none(),
        "Filled order should be removed from StateManager after reconciliation"
    );

    drop(new_tx);
}
