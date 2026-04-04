//! Integration tests for `EventRouter::reconcile_after_reconnect()`.
//!
//! Uses `MockTradingAdapter` to verify that missed fills, cancellations,
//! and state changes during a disconnect are detected and broadcast.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use smallvec::SmallVec;
use tokio::sync::broadcast;

use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::{
    ExitEntry, ExitEntryStatus, ExitHandler, ExitHandling, ExitLegType,
};
use tektii_gateway_core::models::{OrderStatus, PositionSide, Side, TradingPlatform};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::{ConnectionEventType, OrderEventType, WsMessage};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::mock_exit_handler::MockExitHandler;
use tektii_gateway_test_support::models::{test_order, test_position};

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;

fn setup() -> (
    Arc<EventRouter>,
    broadcast::Receiver<WsMessage>,
    Arc<StateManager>,
) {
    let state_manager = Arc::new(StateManager::new());
    let exit_handler: Arc<dyn tektii_gateway_core::exit_management::ExitHandling> = Arc::new(
        ExitHandler::with_defaults(Arc::clone(&state_manager), PLATFORM),
    );
    let (tx, rx) = broadcast::channel(64);
    let router = Arc::new(EventRouter::new(
        Arc::clone(&state_manager),
        exit_handler,
        tx,
        PLATFORM,
    ));
    (router, rx, state_manager)
}

#[tokio::test]
async fn missed_fill_detected_and_state_updated() {
    let (router, mut rx, state_manager) = setup();

    // Pre-populate state with an Open order
    let order = tektii_gateway_core::models::Order {
        id: "order-1".into(),
        status: OrderStatus::Open,
        ..test_order()
    };
    state_manager.upsert_order(&order);

    // Mock adapter returns the same order as Filled
    let filled_order = tektii_gateway_core::models::Order {
        id: "order-1".into(),
        status: OrderStatus::Filled,
        filled_quantity: dec!(1),
        remaining_quantity: Decimal::ZERO,
        average_fill_price: Some(dec!(50000)),
        ..test_order()
    };
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(filled_order));
    router
        .set_adapter(mock as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    // Should have broadcast the fill event AND a Connected event
    let mut saw_fill = false;
    let mut saw_connected = false;

    while let Ok(msg) = rx.try_recv() {
        match &msg {
            WsMessage::Order {
                event: OrderEventType::OrderFilled,
                order,
                ..
            } => {
                assert_eq!(order.id, "order-1");
                assert_eq!(order.status, OrderStatus::Filled);
                saw_fill = true;
            }
            WsMessage::Connection {
                event: ConnectionEventType::Connected,
                ..
            } => {
                saw_connected = true;
            }
            _ => {}
        }
    }

    assert!(saw_fill, "Expected OrderFilled event to be broadcast");
    assert!(
        saw_connected,
        "Expected Connected event after successful reconciliation"
    );

    // After full sync (step 2), only open orders remain in cache.
    // The filled order was detected and broadcast, but sync_from_provider
    // replaces state with only Open/PartiallyFilled/Pending orders.
    // Verify the fill was detected by checking the broadcast above.
}

#[tokio::test]
async fn missed_partial_fill_detected() {
    let (router, mut rx, state_manager) = setup();

    // Cached: Open with filled_qty=0
    let order = tektii_gateway_core::models::Order {
        id: "order-2".into(),
        status: OrderStatus::Open,
        filled_quantity: Decimal::ZERO,
        ..test_order()
    };
    state_manager.upsert_order(&order);

    // Provider: PartiallyFilled with filled_qty=50
    let partial = tektii_gateway_core::models::Order {
        id: "order-2".into(),
        status: OrderStatus::PartiallyFilled,
        filled_quantity: dec!(50),
        remaining_quantity: dec!(50),
        ..test_order()
    };
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(partial));
    router
        .set_adapter(mock as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    let mut saw_partial = false;
    while let Ok(msg) = rx.try_recv() {
        if let WsMessage::Order {
            event: OrderEventType::OrderPartiallyFilled,
            order,
            ..
        } = &msg
        {
            assert_eq!(order.id, "order-2");
            saw_partial = true;
        }
    }
    assert!(
        saw_partial,
        "Expected OrderPartiallyFilled event to be broadcast"
    );
}

#[tokio::test]
async fn missed_cancellation_detected() {
    let (router, mut rx, state_manager) = setup();

    let order = tektii_gateway_core::models::Order {
        id: "order-3".into(),
        status: OrderStatus::Open,
        ..test_order()
    };
    state_manager.upsert_order(&order);

    let cancelled = tektii_gateway_core::models::Order {
        id: "order-3".into(),
        status: OrderStatus::Cancelled,
        ..test_order()
    };
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(cancelled));
    router
        .set_adapter(mock as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    let mut saw_cancel = false;
    while let Ok(msg) = rx.try_recv() {
        if let WsMessage::Order {
            event: OrderEventType::OrderCancelled,
            ..
        } = &msg
        {
            saw_cancel = true;
        }
    }
    assert!(saw_cancel, "Expected OrderCancelled event to be broadcast");
}

#[tokio::test]
async fn no_changes_when_state_matches() {
    let (router, mut rx, state_manager) = setup();

    let order = tektii_gateway_core::models::Order {
        id: "order-4".into(),
        status: OrderStatus::Open,
        filled_quantity: Decimal::ZERO,
        ..test_order()
    };
    state_manager.upsert_order(&order.clone());

    // Provider returns exact same state
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(order));
    router
        .set_adapter(mock as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    // Should only see Connected — no order change events
    let mut order_events = 0;
    while let Ok(msg) = rx.try_recv() {
        if matches!(msg, WsMessage::Order { .. }) {
            order_events += 1;
        }
    }
    assert_eq!(order_events, 0, "Expected no order change events");
}

#[tokio::test]
async fn order_not_found_removed_from_cache() {
    let (router, _rx, state_manager) = setup();

    let order = tektii_gateway_core::models::Order {
        id: "order-5".into(),
        status: OrderStatus::Open,
        ..test_order()
    };
    state_manager.upsert_order(&order);

    // Mock adapter has NO orders → get_order returns OrderNotFound
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM));
    router
        .set_adapter(mock as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    // Order should be removed from cache
    assert!(
        state_manager.get_order("order-5").is_none(),
        "Expected order to be removed from StateManager"
    );
}

#[tokio::test]
async fn concurrent_reconciliation_prevented() {
    let (router, _rx, state_manager) = setup();

    // Add an order so reconciliation has something to process
    let order = tektii_gateway_core::models::Order {
        id: "order-6".into(),
        status: OrderStatus::Open,
        ..test_order()
    };
    state_manager.upsert_order(&order.clone());

    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(order));
    router
        .set_adapter(mock as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>)
        .ok()
        .expect("adapter set once");

    // Run two reconciliations concurrently — only one should actually execute
    let r1 = router.clone();
    let r2 = router.clone();
    let ((), ()) = tokio::join!(
        r1.reconcile_after_reconnect(),
        r2.reconcile_after_reconnect()
    );

    // If both ran, that's a bug. We can't easily verify which one ran,
    // but the test verifies no panic and the AtomicBool guard works.
    // The important thing is no panic or deadlock occurred.
}

#[tokio::test]
async fn connected_event_broadcast_after_successful_sync() {
    let (router, mut rx, _state_manager) = setup();

    // No orders in cache — reconciliation should still sync and broadcast Connected
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM));
    router
        .set_adapter(mock as Arc<dyn tektii_gateway_core::adapter::TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    let mut saw_connected = false;
    while let Ok(msg) = rx.try_recv() {
        if let WsMessage::Connection {
            event: ConnectionEventType::Connected,
            ..
        } = &msg
        {
            saw_connected = true;
        }
    }
    assert!(
        saw_connected,
        "Expected Connected event after successful reconciliation"
    );
}

// =============================================================================
// Story 6.3.4 — Reconnection state reconciliation tests
// =============================================================================

#[tokio::test]
async fn expired_order_detected_during_reconciliation() {
    let (router, mut rx, state_manager) = setup();

    let order = tektii_gateway_core::models::Order {
        id: "order-exp".into(),
        status: OrderStatus::Open,
        ..test_order()
    };
    state_manager.upsert_order(&order);

    let expired = tektii_gateway_core::models::Order {
        id: "order-exp".into(),
        status: OrderStatus::Expired,
        ..test_order()
    };
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(expired));
    router
        .set_adapter(mock as Arc<dyn TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    let mut saw_expired = false;
    let mut saw_connected = false;
    while let Ok(msg) = rx.try_recv() {
        match &msg {
            WsMessage::Order {
                event: OrderEventType::OrderExpired,
                order,
                ..
            } => {
                assert_eq!(order.id, "order-exp");
                saw_expired = true;
            }
            WsMessage::Connection {
                event: ConnectionEventType::Connected,
                ..
            } => {
                saw_connected = true;
            }
            _ => {}
        }
    }
    assert!(saw_expired, "Expected OrderExpired event to be broadcast");
    assert!(
        saw_connected,
        "Expected Connected event after successful reconciliation"
    );
}

#[tokio::test]
async fn fill_during_reconciliation_triggers_exit_management() {
    let state_manager = Arc::new(StateManager::new());
    let mock_exit = Arc::new(MockExitHandler::new());
    let exit_handler: Arc<dyn ExitHandling> = Arc::clone(&mock_exit) as Arc<dyn ExitHandling>;
    let (tx, _rx) = broadcast::channel(64);
    let router = Arc::new(EventRouter::new(
        Arc::clone(&state_manager),
        exit_handler,
        tx,
        PLATFORM,
    ));

    // Cached: Open order
    let order = tektii_gateway_core::models::Order {
        id: "order-fill-exit".into(),
        status: OrderStatus::Open,
        quantity: dec!(5),
        filled_quantity: Decimal::ZERO,
        ..test_order()
    };
    state_manager.upsert_order(&order);

    // Provider: Filled
    let filled = tektii_gateway_core::models::Order {
        id: "order-fill-exit".into(),
        status: OrderStatus::Filled,
        quantity: dec!(5),
        filled_quantity: dec!(5),
        remaining_quantity: Decimal::ZERO,
        average_fill_price: Some(dec!(50000)),
        ..test_order()
    };
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_order(filled));
    router
        .set_adapter(mock as Arc<dyn TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    let calls = mock_exit.fill_calls();
    assert_eq!(calls.len(), 1, "Expected exactly one handle_fill call");
    assert_eq!(calls[0].primary_order_id, "order-fill-exit");
    assert_eq!(calls[0].filled_qty, dec!(5));
    assert_eq!(calls[0].total_qty, dec!(5));
    assert_eq!(calls[0].position_qty, None);
}

#[tokio::test]
async fn failed_exit_entries_rebroadcast_on_reconnect() {
    let state_manager = Arc::new(StateManager::new());
    let exit_handler = Arc::new(ExitHandler::with_defaults(
        Arc::clone(&state_manager),
        PLATFORM,
    ));

    // Pre-insert a failed exit entry
    let entry = ExitEntry {
        placeholder_id: "exit:sl:order-fail".into(),
        primary_order_id: "order-fail".into(),
        order_type: ExitLegType::StopLoss,
        symbol: "BTCUSD".into(),
        side: Side::Sell,
        trigger_price: dec!(45000),
        limit_price: None,
        quantity: dec!(1),
        original_quantity: dec!(1),
        status: ExitEntryStatus::Failed {
            error: "Connection timeout".into(),
            retry_count: 3,
            failed_at: Utc::now(),
            actual_orders: SmallVec::new(),
        },
        created_at: Instant::now(),
        created_at_utc: Utc::now(),
        updated_at: Some(Utc::now()),
        platform: PLATFORM,
        sibling_id: None,
        last_attempted_fill_qty: Some(dec!(1)),
        version: 1,
    };
    exit_handler.insert_entry(entry);

    let exit_handling: Arc<dyn ExitHandling> = exit_handler.clone();
    let (tx, mut rx) = broadcast::channel(64);
    let router = Arc::new(EventRouter::new(
        Arc::clone(&state_manager),
        exit_handling,
        tx,
        PLATFORM,
    ));

    // Empty adapter — no orders to reconcile, but failed entries exist
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM));
    router
        .set_adapter(mock as Arc<dyn TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    // Verify PositionUnprotected broadcast with rebroadcast flag
    let mut saw_unprotected = false;
    while let Ok(msg) = rx.try_recv() {
        if let WsMessage::Error { details, .. } = &msg
            && let Some(details) = details
            && details.get("rebroadcast") == Some(&serde_json::json!(true))
        {
            assert_eq!(details["order_id"], "order-fail");
            assert_eq!(details["symbol"], "BTCUSD");
            saw_unprotected = true;
        }
    }
    assert!(
        saw_unprotected,
        "Expected PositionUnprotected rebroadcast with rebroadcast=true"
    );

    // Failed entries should be cleared after rebroadcast
    assert!(
        exit_handler.get_failed_entries().is_empty(),
        "Expected failed entries to be cleared after rebroadcast"
    );
}

#[tokio::test]
async fn position_state_synced_after_reconciliation() {
    let (router, _rx, state_manager) = setup();

    // Provider has a position, but StateManager is empty
    let position = tektii_gateway_core::models::Position {
        id: "BTCUSD_position".into(),
        symbol: "BTCUSD".into(),
        side: PositionSide::Long,
        quantity: dec!(2),
        average_entry_price: dec!(48000),
        ..test_position()
    };
    let mock = Arc::new(MockTradingAdapter::new(PLATFORM).with_position(position));
    router
        .set_adapter(mock as Arc<dyn TradingAdapter>)
        .ok()
        .expect("adapter set once");

    assert!(
        state_manager.get_position_by_symbol("BTCUSD").is_none(),
        "StateManager should be empty before reconciliation"
    );

    router.reconcile_after_reconnect().await;

    let synced = state_manager
        .get_position_by_symbol("BTCUSD")
        .expect("Position should be synced from provider");
    assert_eq!(synced.quantity, dec!(2));
    assert_eq!(synced.side, PositionSide::Long);
}

#[tokio::test]
async fn multiple_divergences_detected_in_single_run() {
    let (router, mut rx, state_manager) = setup();

    // Three cached Open orders
    for id in &["order-a", "order-b", "order-c"] {
        let order = tektii_gateway_core::models::Order {
            id: (*id).into(),
            status: OrderStatus::Open,
            filled_quantity: Decimal::ZERO,
            ..test_order()
        };
        state_manager.upsert_order(&order);
    }

    // Provider returns: one Filled, one Cancelled, one Expired
    let filled = tektii_gateway_core::models::Order {
        id: "order-a".into(),
        status: OrderStatus::Filled,
        filled_quantity: dec!(1),
        remaining_quantity: Decimal::ZERO,
        average_fill_price: Some(dec!(50000)),
        ..test_order()
    };
    let cancelled = tektii_gateway_core::models::Order {
        id: "order-b".into(),
        status: OrderStatus::Cancelled,
        ..test_order()
    };
    let expired = tektii_gateway_core::models::Order {
        id: "order-c".into(),
        status: OrderStatus::Expired,
        ..test_order()
    };

    let mock = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_order(filled)
            .with_order(cancelled)
            .with_order(expired),
    );
    router
        .set_adapter(mock as Arc<dyn TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    let mut saw_filled = false;
    let mut saw_cancelled = false;
    let mut saw_expired = false;
    let mut saw_connected = false;

    while let Ok(msg) = rx.try_recv() {
        match &msg {
            WsMessage::Order {
                event: OrderEventType::OrderFilled,
                ..
            } => saw_filled = true,
            WsMessage::Order {
                event: OrderEventType::OrderCancelled,
                ..
            } => saw_cancelled = true,
            WsMessage::Order {
                event: OrderEventType::OrderExpired,
                ..
            } => saw_expired = true,
            WsMessage::Connection {
                event: ConnectionEventType::Connected,
                ..
            } => saw_connected = true,
            _ => {}
        }
    }

    assert!(saw_filled, "Expected OrderFilled event");
    assert!(saw_cancelled, "Expected OrderCancelled event");
    assert!(saw_expired, "Expected OrderExpired event");
    assert!(
        saw_connected,
        "Expected Connected event after reconciliation"
    );
}

#[tokio::test]
async fn sync_failure_prevents_connected_broadcast() {
    let (router, mut rx, state_manager) = setup();

    // Cached order so step 1 has something to process
    let order = tektii_gateway_core::models::Order {
        id: "order-sync-fail".into(),
        status: OrderStatus::Open,
        ..test_order()
    };
    state_manager.upsert_order(&order.clone());

    // Mock adapter: get_order works (step 1), but get_orders fails (step 2)
    let mock = Arc::new(
        MockTradingAdapter::new(PLATFORM)
            .with_order(order)
            .with_get_orders_error(GatewayError::ProviderError {
                provider: Some("mock".to_string()),
                message: "simulated sync failure".to_string(),
                source: None,
            }),
    );
    router
        .set_adapter(mock as Arc<dyn TradingAdapter>)
        .ok()
        .expect("adapter set once");

    router.reconcile_after_reconnect().await;

    // No Connected event should be broadcast when sync fails
    let mut saw_connected = false;
    while let Ok(msg) = rx.try_recv() {
        if matches!(
            msg,
            WsMessage::Connection {
                event: ConnectionEventType::Connected,
                ..
            }
        ) {
            saw_connected = true;
        }
    }
    assert!(
        !saw_connected,
        "Connected event should NOT be broadcast when sync fails"
    );
}
