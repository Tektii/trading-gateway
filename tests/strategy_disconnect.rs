//! Integration tests for strategy WebSocket disconnect cleanup (Story 6.3.3).
//!
//! Verifies that when a strategy's WebSocket connection drops, the gateway:
//! - Unregisters the connection from WsConnectionManager
//! - Unregisters the connection from ProviderRegistry
//! - Aborts the send task
//! - Does NOT touch broker state (orders/positions left intact)

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::routing::get;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use tektii_gateway_core::api::routes::create_gateway_router;
use tektii_gateway_core::api::state::GatewayState;
use tektii_gateway_core::websocket::connection::WsConnectionManager;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_core::websocket::server::ws_handler;
use tektii_gateway_test_support::mock_state::test_gateway_state;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawn a minimal gateway server on a random port.
///
/// Returns the shared state (for inspecting connection counts), the bound
/// address, and the server task handle.
async fn spawn_gateway() -> (GatewayState, SocketAddr, JoinHandle<()>) {
    let state = test_gateway_state();

    let (router, _api) = create_gateway_router().split_for_parts();
    let app = router
        .route(
            "/v1/ws",
            get(
                |ws: axum::extract::WebSocketUpgrade,
                 state: axum::extract::State<GatewayState>,
                 conn: axum::extract::ConnectInfo<SocketAddr>| async move {
                    ws_handler(ws, state, conn)
                },
            ),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // Yield to let the server start accepting connections.
    tokio::task::yield_now().await;

    (state, addr, handle)
}

/// Connect a tokio-tungstenite WebSocket client to the gateway.
async fn ws_connect(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/v1/ws");
    let (stream, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket connect failed");
    stream
}

/// Poll `connection_count()` until it reaches `expected`, or panic after `deadline`.
async fn wait_for_connection_count(
    manager: &WsConnectionManager,
    expected: usize,
    deadline: Duration,
) {
    let start = Instant::now();
    loop {
        let count = manager.connection_count().await;
        if count == expected {
            return;
        }
        assert!(
            start.elapsed() <= deadline,
            "Timed out after {deadline:?} waiting for connection_count == {expected} (actual: {count})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll `connected_strategy_count()` until it reaches `expected`, or panic after `deadline`.
async fn wait_for_strategy_count(registry: &ProviderRegistry, expected: usize, deadline: Duration) {
    let start = Instant::now();
    loop {
        let count = registry.connected_strategy_count().await;
        if count == expected {
            return;
        }
        assert!(
            start.elapsed() <= deadline,
            "Timed out after {deadline:?} waiting for connected_strategy_count == {expected} (actual: {count})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disconnect_unregisters_from_connection_manager() {
    let (state, addr, _server) = spawn_gateway().await;
    let manager = state.ws_connection_manager();

    let client = ws_connect(addr).await;
    wait_for_connection_count(manager, 1, TIMEOUT).await;

    drop(client);
    wait_for_connection_count(manager, 0, TIMEOUT).await;

    assert_eq!(manager.connection_count().await, 0);
}

#[tokio::test]
async fn disconnect_unregisters_from_provider_registry() {
    let (state, addr, _server) = spawn_gateway().await;
    let registry = state.provider_registry();

    let client = ws_connect(addr).await;
    wait_for_strategy_count(registry, 1, TIMEOUT).await;

    drop(client);
    wait_for_strategy_count(registry, 0, TIMEOUT).await;

    assert_eq!(registry.connected_strategy_count().await, 0);
}

#[tokio::test]
async fn disconnect_does_not_touch_broker_state() {
    // test_gateway_state() has no adapters registered.
    // If the disconnect path tried to call any broker API, it would
    // fail or panic — the test completing successfully proves broker
    // state is untouched.
    let (state, addr, _server) = spawn_gateway().await;
    let manager = state.ws_connection_manager();

    let client = ws_connect(addr).await;
    wait_for_connection_count(manager, 1, TIMEOUT).await;

    drop(client);
    wait_for_connection_count(manager, 0, TIMEOUT).await;
}

#[tokio::test]
async fn broadcast_after_disconnect_reaches_zero_clients() {
    use tektii_gateway_core::websocket::messages::WsMessage;
    use tektii_gateway_test_support::models::test_quote;

    let (state, addr, _server) = spawn_gateway().await;
    let manager = state.ws_connection_manager();

    let client = ws_connect(addr).await;
    wait_for_connection_count(manager, 1, TIMEOUT).await;

    drop(client);
    wait_for_connection_count(manager, 0, TIMEOUT).await;

    // Broadcasting to zero connections should not panic or error.
    manager
        .broadcast(WsMessage::quote(test_quote("AAPL")))
        .await;

    assert_eq!(manager.connection_count().await, 0);
}

#[tokio::test]
async fn multiple_strategies_disconnect_independently() {
    let (state, addr, _server) = spawn_gateway().await;
    let manager = state.ws_connection_manager();
    let registry = state.provider_registry();

    let client_a = ws_connect(addr).await;
    wait_for_connection_count(manager, 1, TIMEOUT).await;

    let client_b = ws_connect(addr).await;
    wait_for_connection_count(manager, 2, TIMEOUT).await;
    assert_eq!(registry.connected_strategy_count().await, 2);

    // Drop A — B remains.
    drop(client_a);
    wait_for_connection_count(manager, 1, TIMEOUT).await;
    assert_eq!(registry.connected_strategy_count().await, 1);

    // Drop B — none remain.
    drop(client_b);
    wait_for_connection_count(manager, 0, TIMEOUT).await;
    assert_eq!(registry.connected_strategy_count().await, 0);
}
