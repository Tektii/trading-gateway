//! Integration tests for the graceful shutdown sequence.
//!
//! Tests `run_shutdown_sequence()` to verify the orchestrated flow:
//! strategy notification, shutdown flag, exit state persistence,
//! provider cleanup, and cancellation token.

use std::sync::Arc;
use std::time::Duration;

use rust_decimal_macros::dec;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tektii_gateway_core::api::state::GatewayState;
use tektii_gateway_core::config::ReconnectionConfig;
use tektii_gateway_core::correlation::CorrelationStore;
use tektii_gateway_core::exit_management::ExitHandlerRegistry;
use tektii_gateway_core::exit_management::handler::ExitHandler;
use tektii_gateway_core::exit_management::types::{ExitEntryParams, ExitLegType};
use tektii_gateway_core::models::{Side, TradingPlatform};
use tektii_gateway_core::shutdown::{ExitStateSnapshot, run_shutdown_sequence};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::connection::{WsConnection, WsConnectionManager};
use tektii_gateway_core::websocket::messages::{ConnectionEventType, WsMessage};
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_test_support::mock_state::test_gateway_state;

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;

/// Create shared infrastructure for shutdown tests.
fn setup() -> (
    Arc<WsConnectionManager>,
    GatewayState,
    Arc<ExitHandlerRegistry>,
    Arc<ProviderRegistry>,
    CancellationToken,
) {
    let ws_manager = Arc::new(WsConnectionManager::new());
    let cancel_token = CancellationToken::new();
    let correlation_store = Arc::new(CorrelationStore::new());
    let provider_registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]),
        cancel_token.clone(),
        ReconnectionConfig::default(),
        correlation_store,
    ));
    let exit_handler_registry = Arc::new(ExitHandlerRegistry::new());
    let gateway_state = test_gateway_state();

    (
        ws_manager,
        gateway_state,
        exit_handler_registry,
        provider_registry,
        cancel_token,
    )
}

/// Create a test WsConnection and return the receiver for inspecting sent messages.
fn test_connection(port: u16) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(1024);
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let conn = WsConnection {
        id: uuid::Uuid::new_v4(),
        sender: tx,
        addr,
        last_activity: Arc::new(tokio::sync::RwLock::new(std::time::Instant::now())),
        connected_at: std::time::Instant::now(),
    };
    (conn, rx)
}

/// Create an ExitHandlerRegistry with `n` pending exit entries.
fn registry_with_pending(n: usize) -> Arc<ExitHandlerRegistry> {
    let state_manager = Arc::new(StateManager::new());
    let handler = ExitHandler::with_defaults(state_manager, PLATFORM);

    for i in 0..n {
        let entry = tektii_gateway_core::exit_management::types::ExitEntry::new(ExitEntryParams {
            primary_order_id: format!("order-{i}"),
            order_type: ExitLegType::StopLoss,
            symbol: format!("SYM{i}"),
            side: Side::Sell,
            trigger_price: dec!(100.0),
            limit_price: None,
            quantity: dec!(10.0),
            platform: PLATFORM,
        });
        handler.insert_entry(entry);
    }

    let registry = ExitHandlerRegistry::new();
    registry.register(PLATFORM, Arc::new(handler));
    Arc::new(registry)
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn shutdown_sequence_notifies_connected_strategies() {
    let (ws_manager, gateway_state, exit_handler_registry, provider_registry, cancel_token) =
        setup();

    // Add two strategy connections
    let (conn1, mut rx1) = test_connection(9001);
    let (conn2, mut rx2) = test_connection(9002);
    ws_manager.add_connection(conn1).await;
    ws_manager.add_connection(conn2).await;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("exit-state.json");

    run_shutdown_sequence(
        &ws_manager,
        &gateway_state,
        &exit_handler_registry,
        &path,
        &provider_registry,
        &cancel_token,
    )
    .await;

    // Both strategies should receive Disconnecting then Close(1001)
    for rx in [&mut rx1, &mut rx2] {
        let msg1 = rx.recv().await.expect("should receive Disconnecting");
        assert!(
            matches!(
                msg1,
                WsMessage::Connection {
                    event: ConnectionEventType::Disconnecting,
                    ..
                }
            ),
            "expected Disconnecting, got {msg1:?}"
        );

        let msg2 = rx.recv().await.expect("should receive Close");
        assert!(
            matches!(msg2, WsMessage::Close { code: 1001, .. }),
            "expected Close 1001, got {msg2:?}"
        );
    }
}

#[tokio::test]
async fn shutdown_sequence_sets_shutdown_flag() {
    let (ws_manager, gateway_state, exit_handler_registry, provider_registry, cancel_token) =
        setup();
    let clone = gateway_state.clone();

    assert!(!gateway_state.is_shutting_down());

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("exit-state.json");

    run_shutdown_sequence(
        &ws_manager,
        &gateway_state,
        &exit_handler_registry,
        &path,
        &provider_registry,
        &cancel_token,
    )
    .await;

    assert!(gateway_state.is_shutting_down());
    assert!(clone.is_shutting_down());
}

#[tokio::test]
async fn shutdown_sequence_writes_exit_state_with_pending_entries() {
    let (ws_manager, gateway_state, _, provider_registry, cancel_token) = setup();
    let exit_handler_registry = registry_with_pending(3);

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("exit-state.json");

    run_shutdown_sequence(
        &ws_manager,
        &gateway_state,
        &exit_handler_registry,
        &path,
        &provider_registry,
        &cancel_token,
    )
    .await;

    // File should exist with 3 entries
    assert!(path.exists(), "exit state snapshot should be written");

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let snapshot: ExitStateSnapshot = serde_json::from_str(&content).unwrap();
    assert_eq!(snapshot.entries.len(), 3);

    for entry in &snapshot.entries {
        assert_eq!(entry.status, "pending");
        assert_eq!(entry.side, Side::Sell);
        assert_eq!(entry.order_type, ExitLegType::StopLoss);
        assert_eq!(entry.platform, PLATFORM);
    }

    // Shutdown flag should also be set (whole sequence completed)
    assert!(gateway_state.is_shutting_down());
}

#[tokio::test]
async fn shutdown_sequence_completes_within_time_bound() {
    let (ws_manager, gateway_state, exit_handler_registry, provider_registry, cancel_token) =
        setup();

    // Add a strategy connection (triggers 500ms broadcast pause)
    let (conn, _rx) = test_connection(9003);
    ws_manager.add_connection(conn).await;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("exit-state.json");

    // 2 seconds is generous: 500ms broadcast pause + margin for task teardown
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        run_shutdown_sequence(
            &ws_manager,
            &gateway_state,
            &exit_handler_registry,
            &path,
            &provider_registry,
            &cancel_token,
        ),
    )
    .await;

    assert!(
        result.is_ok(),
        "shutdown sequence should complete within 2s"
    );
    assert!(gateway_state.is_shutting_down());
}

#[tokio::test]
async fn shutdown_sequence_cancels_token_and_clears_providers() {
    let (ws_manager, gateway_state, exit_handler_registry, provider_registry, cancel_token) =
        setup();

    assert!(!cancel_token.is_cancelled());
    assert_eq!(provider_registry.shared_provider_count().await, 0);

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("exit-state.json");

    run_shutdown_sequence(
        &ws_manager,
        &gateway_state,
        &exit_handler_registry,
        &path,
        &provider_registry,
        &cancel_token,
    )
    .await;

    assert!(cancel_token.is_cancelled());
    assert_eq!(provider_registry.shared_provider_count().await, 0);
}
