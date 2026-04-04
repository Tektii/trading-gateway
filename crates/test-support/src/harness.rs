//! Gateway test harness for end-to-end integration tests.
//!
//! Spins up a full gateway (REST + WebSocket) backed by a [`MockTradingAdapter`]
//! and a mock provider event stream. Tests can inject events via `event_tx` and
//! interact with the gateway through HTTP and WebSocket clients.
//!
//! # Example
//!
//! ```ignore
//! use tektii_gateway_test_support::harness::*;
//! use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
//!
//! let gw = spawn_test_gateway(MockTradingAdapter::new(TradingPlatform::AlpacaPaper)).await;
//! // Make REST calls to gw.base_url()
//! // Connect WS client to gw.ws_url()
//! // Inject events via gw.inject_event(...)
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use tektii_gateway_core::adapter::{AdapterRegistry, TradingAdapter};
use tektii_gateway_core::api::routes::create_gateway_router;
use tektii_gateway_core::api::state::GatewayState;
use tektii_gateway_core::config::ReconnectionConfig;
use tektii_gateway_core::correlation::CorrelationStore;
use tektii_gateway_core::exit_management::ExitHandlerRegistry;
use tektii_gateway_core::metrics::{MetricsHandle, metrics_handler};
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::connection::WsConnectionManager;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_core::websocket::server::ws_handler;

use crate::mock_adapter::MockTradingAdapter;
use crate::mock_provider::MockWebSocketProvider;

/// A running test gateway with REST and WebSocket servers.
///
/// The gateway binds to a random port and is backed by a [`MockTradingAdapter`].
/// Inject broker events via [`inject_event`](Self::inject_event).
pub struct TestGateway {
    /// Shared gateway state (for inspecting connection counts, etc.).
    pub state: GatewayState,

    /// The bound address of the gateway server.
    pub addr: SocketAddr,

    /// Channel to inject events into the provider event stream.
    /// These events will be broadcast to all connected strategy WebSocket clients.
    pub event_tx: mpsc::UnboundedSender<WsMessage>,

    /// Handle to the server task (aborted on drop).
    server_handle: JoinHandle<()>,

    /// Cancellation token for graceful shutdown.
    cancel: CancellationToken,

    /// Metrics handle (only present when gateway was spawned with metrics).
    metrics_handle: Option<Arc<MetricsHandle>>,
}

impl TestGateway {
    /// Base URL for REST API calls (e.g., `http://127.0.0.1:12345/v1`).
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Root URL for health endpoints (e.g., `http://127.0.0.1:12345`).
    #[must_use]
    pub fn root_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// WebSocket URL for strategy connections (e.g., `ws://127.0.0.1:12345/v1/ws`).
    #[must_use]
    pub fn ws_url(&self) -> String {
        format!("ws://{}/v1/ws", self.addr)
    }

    /// Inject a broker event into the provider event stream.
    ///
    /// The event will be broadcast to all connected strategy WebSocket clients
    /// (subject to subscription filter — empty filter matches all).
    pub fn inject_event(&self, event: WsMessage) {
        let _ = self.event_tx.send(event);
    }

    /// Render current Prometheus metrics output (if metrics were initialised).
    ///
    /// Returns `None` if the gateway was spawned without metrics support.
    #[must_use]
    pub fn metrics_output(&self) -> Option<String> {
        self.metrics_handle.as_ref().map(|h| h.render())
    }

    /// Shut down the gateway server.
    pub fn shutdown(self) {
        self.cancel.cancel();
        self.server_handle.abort();
    }
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.server_handle.abort();
    }
}

/// Spawn a full gateway backed by the given mock adapter.
///
/// The gateway binds to a random port and starts accepting connections immediately.
/// Returns a [`TestGateway`] with the server address, shared state, and an event
/// injection channel.
pub async fn spawn_test_gateway(adapter: MockTradingAdapter) -> TestGateway {
    spawn_test_gateway_with_adapter(Arc::new(adapter)).await
}

/// Spawn a full gateway backed by a shared mock adapter reference.
///
/// Like [`spawn_test_gateway`], but accepts an `Arc<MockTradingAdapter>` so the caller
/// can retain a clone for post-test assertions (e.g., checking `cancelled_orders()`).
pub async fn spawn_test_gateway_with_adapter(adapter: Arc<MockTradingAdapter>) -> TestGateway {
    let platform = adapter.platform();
    let adapter: Arc<dyn TradingAdapter> = adapter;

    // Build adapter registry
    let adapter_registry = AdapterRegistry::new(Arc::clone(&adapter), platform);

    // Create shared infrastructure
    let ws_manager = Arc::new(WsConnectionManager::new());
    let correlation_store = Arc::new(CorrelationStore::new());
    let cancel = CancellationToken::new();

    let provider_registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]), // empty = match all events
        cancel.clone(),
        ReconnectionConfig::default(),
        correlation_store.clone(),
    ));

    let exit_handler_registry = Arc::new(ExitHandlerRegistry::new());

    // Build gateway state
    let state = GatewayState::new(
        adapter_registry,
        ws_manager,
        provider_registry.clone(),
        exit_handler_registry,
        correlation_store,
        None,
    );

    // Create provider event stream and register
    let (event_tx, stream) = MockWebSocketProvider::make_event_stream();
    let provider = MockWebSocketProvider::new(false); // no reconnection needed
    provider_registry
        .register_provider(platform, Box::new(provider), stream, vec![], vec![])
        .await
        .expect("register_provider should succeed");

    // Build Axum router (REST + WebSocket)
    let (router, _api) = create_gateway_router().split_for_parts();
    let app = router
        .route(
            "/v1/ws",
            get(
                |ws: axum::extract::WebSocketUpgrade,
                 axum_state: axum::extract::State<GatewayState>,
                 conn: ConnectInfo<SocketAddr>| async move {
                    ws_handler(ws, axum_state, conn)
                },
            ),
        )
        .with_state(state.clone());

    // Bind to random port and start serving
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // Yield to let the server start accepting connections
    tokio::task::yield_now().await;

    TestGateway {
        state,
        addr,
        event_tx,
        server_handle,
        cancel,
        metrics_handle: None,
    }
}

/// Spawn a full gateway with Prometheus metrics support.
///
/// Like [`spawn_test_gateway`], but also installs the Prometheus metrics recorder
/// and mounts the `/metrics` endpoint. Returns `None` if the global recorder was
/// already claimed by another test process.
pub async fn spawn_test_gateway_with_metrics(adapter: MockTradingAdapter) -> Option<TestGateway> {
    let metrics_handle = MetricsHandle::try_init()?;
    let metrics_handle = Arc::new(metrics_handle);

    let platform = adapter.platform();
    let adapter: Arc<dyn TradingAdapter> = Arc::new(adapter);

    let adapter_registry = AdapterRegistry::new(Arc::clone(&adapter), platform);

    let ws_manager = Arc::new(WsConnectionManager::new());
    let correlation_store = Arc::new(CorrelationStore::new());
    let cancel = CancellationToken::new();

    let provider_registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]),
        cancel.clone(),
        ReconnectionConfig::default(),
        correlation_store.clone(),
    ));

    let exit_handler_registry = Arc::new(ExitHandlerRegistry::new());

    let state = GatewayState::new(
        adapter_registry,
        ws_manager,
        provider_registry.clone(),
        exit_handler_registry,
        correlation_store,
        None,
    );

    let (event_tx, stream) = MockWebSocketProvider::make_event_stream();
    let provider = MockWebSocketProvider::new(false);
    provider_registry
        .register_provider(platform, Box::new(provider), stream, vec![], vec![])
        .await
        .expect("register_provider should succeed");

    let (router, _api) = create_gateway_router().split_for_parts();
    let app = router
        .route(
            "/v1/ws",
            get(
                |ws: axum::extract::WebSocketUpgrade,
                 axum_state: axum::extract::State<GatewayState>,
                 conn: ConnectInfo<SocketAddr>| async move {
                    ws_handler(ws, axum_state, conn)
                },
            ),
        )
        .with_state(state.clone())
        .route_service(
            "/metrics",
            get(metrics_handler).with_state(Arc::clone(&metrics_handle)),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    tokio::task::yield_now().await;

    Some(TestGateway {
        state,
        addr,
        event_tx,
        server_handle,
        cancel,
        metrics_handle: Some(metrics_handle),
    })
}

// =============================================================================
// Strategy WebSocket client
// =============================================================================

/// A WebSocket client that connects to the gateway as a strategy.
///
/// Thin wrapper around `tokio-tungstenite` with typed message helpers.
pub struct StrategyClient {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl StrategyClient {
    /// Connect to the gateway's WebSocket endpoint.
    pub async fn connect(gateway: &TestGateway) -> Self {
        let (stream, _response) = tokio_tungstenite::connect_async(&gateway.ws_url())
            .await
            .expect("WebSocket connect failed");
        Self { stream }
    }

    /// Receive the next message, deserialised as `WsMessage`.
    ///
    /// Returns `None` if the timeout expires or the connection is closed.
    pub async fn recv_message(&mut self, timeout: Duration) -> Option<WsMessage> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let result = tokio::time::timeout(timeout, self.stream.next()).await;
        match result {
            Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str(&text).ok(),
            _ => None,
        }
    }

    /// Send a JSON-serialised value as a text frame.
    pub async fn send_json(&mut self, value: &serde_json::Value) {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let text = serde_json::to_string(value).expect("serialize");
        self.stream
            .send(Message::Text(text.into()))
            .await
            .expect("send");
    }

    /// Send a close frame (for disconnect tests).
    pub async fn close(mut self) {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let _ = self.stream.send(Message::Close(None)).await;
    }

    /// Get back the raw WebSocket stream.
    pub fn into_inner(
        self,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        self.stream
    }
}

/// Default platform for test gateways.
pub const DEFAULT_PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;
