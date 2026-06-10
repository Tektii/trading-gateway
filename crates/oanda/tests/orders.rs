mod helpers;

use helpers::{
    oanda_account_json, oanda_ioc_cancel_json, oanda_market_fill_json, oanda_order_json,
    oanda_pending_order_json, oanda_reject_json, test_adapter,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderRequest, OrderQueryParams, OrderRequest, OrderStatus, OrderType, Side, TimeInForce,
};
use tektii_gateway_core::websocket::messages::{AccountEventType, OrderEventType, WsMessage};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};
use tokio::sync::mpsc;

/// Helper to build an `OrderRequest` for Oanda forex tests.
fn forex_order(symbol: &str, side: Side, order_type: OrderType, qty: Decimal) -> OrderRequest {
    OrderRequest {
        symbol: symbol.to_string(),
        side,
        quantity: qty,
        order_type,
        ..test_order_request()
    }
}

// =========================================================================
// Submit Order
// =========================================================================

#[tokio::test]
async fn submit_market_order_fill() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({})),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "456");
    assert_eq!(handle.status, OrderStatus::Filled);
}

#[tokio::test]
async fn submit_market_order_fill_uses_wire_order_id() {
    // OANDA spells the wire field `orderID` (capital ID). The handle and the
    // emitted fill event must both carry that order id — which is the create
    // transaction id — not the ORDER_FILL transaction id.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"orderID": "455"})),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "455", "handle carries the wire order id");
    assert_eq!(handle.status, OrderStatus::Filled);

    let event = rx.try_recv().expect("fill event published");
    let WsMessage::Order {
        event: event_type,
        order,
        ..
    } = event.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderFilled);
    assert_eq!(order.id, "455", "fill event carries the wire order id");
}

#[tokio::test]
async fn submit_market_order_fill_emits_account_snapshot() {
    // A synchronous REST fill must be followed by exactly one account
    // snapshot on the strategy stream: balance from the fill's own
    // accountBalance, equity/margin/unrealized from the summary fetch.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"accountBalance": "100123.45"})),
    )
    .await;
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        200,
        oanda_account_json(),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    adapter.submit_order(&request).await.unwrap();

    let first = rx.try_recv().expect("fill event published");
    let WsMessage::Order {
        event: event_type, ..
    } = first.msg
    else {
        panic!("expected an order event first");
    };
    assert_eq!(event_type, OrderEventType::OrderFilled);

    let second = rx.try_recv().expect("account snapshot follows the fill");
    let WsMessage::Account {
        event,
        account,
        timestamp,
    } = second.msg
    else {
        panic!("expected an account event after the fill");
    };
    assert_eq!(event, AccountEventType::BalanceUpdated);
    assert_eq!(
        timestamp,
        "2024-01-15T10:30:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap(),
        "snapshot timestamp comes from the fill transaction time"
    );
    assert_eq!(account.balance, dec!(100123.45));
    assert_eq!(account.equity, dec!(100500.00));
    assert_eq!(account.margin_used, dec!(5000.00));
    assert_eq!(account.margin_available, dec!(95000.00));
    assert_eq!(account.unrealized_pnl, dec!(500.00));
    assert_eq!(account.currency, "USD");
    assert!(rx.try_recv().is_err(), "exactly two events expected");
}

#[tokio::test]
async fn submit_fill_snapshot_balance_falls_back_to_summary() {
    // OANDA omits accountBalance on some fill responses; the snapshot then
    // carries the summary's balance instead of being dropped.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({})),
    )
    .await;
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        200,
        oanda_account_json(),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    adapter.submit_order(&request).await.unwrap();

    let first = rx.try_recv().expect("fill event published");
    assert!(matches!(first.msg, WsMessage::Order { .. }));

    let second = rx.try_recv().expect("account snapshot follows the fill");
    let WsMessage::Account { account, .. } = second.msg else {
        panic!("expected an account event after the fill");
    };
    assert_eq!(account.balance, dec!(100000.00));
    assert!(rx.try_recv().is_err(), "exactly two events expected");
}

#[tokio::test]
async fn submit_fill_skips_account_snapshot_when_summary_fails() {
    // A failed summary fetch must drop the snapshot, never emit zeros --
    // and must not block the fill event or the order handle.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"accountBalance": "100123.45"})),
    )
    .await;
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        500,
        json!({"errorMessage": "internal server error"}),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Filled);

    let first = rx.try_recv().expect("fill event published");
    assert!(matches!(first.msg, WsMessage::Order { .. }));
    assert!(
        rx.try_recv().is_err(),
        "no account snapshot expected when the summary fetch fails"
    );
}

#[tokio::test]
async fn submit_limit_order_pending() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_pending_order_json(&json!({})),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "789");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_ioc_limit_miss_returns_cancelled() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_ioc_cancel_json(),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        time_in_force: TimeInForce::Ioc,
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "1981");
    assert_eq!(handle.status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn submit_ioc_limit_miss_emits_cancel_event() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_ioc_cancel_json(),
    )
    .await;
    // Mount a healthy summary so a wrongly-wired account snapshot on the
    // cancel path would actually emit and trip the exactly-one-event check
    // below, rather than being silently skipped on a 404.
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        200,
        oanda_account_json(),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        time_in_force: TimeInForce::Ioc,
        client_order_id: Some("client-ioc-1".to_string()),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    adapter.submit_order(&request).await.unwrap();

    let event = rx
        .try_recv()
        .expect("cancel event published on the provider stream");
    let WsMessage::Order {
        event: event_type,
        order,
        ..
    } = event.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderCancelled);
    assert_eq!(order.status, OrderStatus::Cancelled);
    assert_eq!(order.id, "1981");
    assert_eq!(order.client_order_id.as_deref(), Some("client-ioc-1"));
    assert_eq!(order.filled_quantity, Decimal::ZERO);
    assert!(rx.try_recv().is_err(), "exactly one event expected");
}

#[tokio::test]
async fn submit_fill_takes_precedence_over_cancel() {
    // An IOC fill response can carry both a fill and a cancel transaction;
    // the fill must win, and only a fill event may reach strategies.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    let mut body = oanda_market_fill_json(&json!({}));
    body["orderCancelTransaction"] = json!({
        "id": "457",
        "type": "ORDER_CANCEL",
        "orderID": "455",
        "reason": "TIME_IN_FORCE_EXPIRED",
        "time": "2024-01-15T10:30:00.000000000Z"
    });
    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        body,
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        time_in_force: TimeInForce::Ioc,
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Filled);

    let event = rx.try_recv().expect("fill event published");
    let WsMessage::Order {
        event: event_type, ..
    } = event.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderFilled);
    assert!(rx.try_recv().is_err(), "no cancel event expected");
}

#[tokio::test]
async fn submit_partial_ioc_fill_reports_partial_then_cancel() {
    // OANDA reports a partially filled IOC as a fill transaction carrying the
    // filled units plus a cancel transaction for the dead remainder, in the
    // same create response. Strategies must see the real quantities — a
    // partial fill event followed by a terminal cancel — never a full fill.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    let mut body = oanda_market_fill_json(&json!({"units": "6000", "orderID": "455"}));
    body["orderCancelTransaction"] = json!({
        "id": "457",
        "type": "ORDER_CANCEL",
        "orderID": "455",
        "reason": "TIME_IN_FORCE_EXPIRED",
        "time": "2024-01-15T10:30:00.000000000Z"
    });
    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        body,
    )
    .await;
    // Healthy summary so the account snapshot actually emits — pinning that it
    // follows the order frames (partial fill, then cancel) rather than
    // interleaving them.
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/summary",
        200,
        oanda_account_json(),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        time_in_force: TimeInForce::Ioc,
        client_order_id: Some("client-ioc-2".to_string()),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "455");
    assert_eq!(handle.status, OrderStatus::PartiallyFilled);

    let first = rx.try_recv().expect("partial fill event published");
    let WsMessage::Order {
        event: event_type,
        order,
        ..
    } = first.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderPartiallyFilled);
    assert_eq!(order.status, OrderStatus::PartiallyFilled);
    assert_eq!(order.id, "455");
    assert_eq!(order.client_order_id.as_deref(), Some("client-ioc-2"));
    assert_eq!(order.filled_quantity, dec!(6000));
    assert_eq!(order.remaining_quantity, dec!(4000));
    assert_eq!(order.average_fill_price, Some(dec!(1.10000)));

    let second = rx.try_recv().expect("terminal cancel event published");
    let WsMessage::Order {
        event: event_type,
        order,
        ..
    } = second.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderCancelled);
    assert_eq!(order.status, OrderStatus::Cancelled);
    assert_eq!(order.id, "455");
    assert_eq!(order.client_order_id.as_deref(), Some("client-ioc-2"));
    assert_eq!(order.filled_quantity, dec!(6000));
    assert_eq!(order.remaining_quantity, dec!(4000));
    assert_eq!(order.average_fill_price, Some(dec!(1.10000)));

    let third = rx.try_recv().expect("account snapshot follows the cancel");
    let WsMessage::Account { event, .. } = third.msg else {
        panic!("expected an account event after the order frames");
    };
    assert_eq!(event, AccountEventType::BalanceUpdated);

    assert!(rx.try_recv().is_err(), "exactly three events expected");
}

#[tokio::test]
async fn submit_fill_missing_units_falls_back_to_full_fill() {
    // Defensive path: a fill transaction without a `units` field (not observed
    // from OANDA in practice) keeps the pre-existing full-fill report.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"units": null})),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Filled);

    let event = rx.try_recv().expect("fill event published");
    let WsMessage::Order {
        event: event_type,
        order,
        ..
    } = event.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderFilled);
    assert_eq!(order.filled_quantity, dec!(10000));
    assert_eq!(order.remaining_quantity, Decimal::ZERO);
}

#[tokio::test]
async fn submit_partial_ioc_fill_sell_side_uses_abs_units() {
    // Sell fills carry negative units on the wire; reported quantities are
    // unsigned.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    let mut body = oanda_market_fill_json(&json!({"units": "-6000", "orderID": "455"}));
    body["orderCancelTransaction"] = json!({
        "id": "457",
        "type": "ORDER_CANCEL",
        "orderID": "455",
        "reason": "TIME_IN_FORCE_EXPIRED",
        "time": "2024-01-15T10:30:00.000000000Z"
    });
    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        body,
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        time_in_force: TimeInForce::Ioc,
        ..forex_order("EUR_USD", Side::Sell, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::PartiallyFilled);

    let first = rx.try_recv().expect("partial fill event published");
    let WsMessage::Order {
        event: event_type,
        order,
        ..
    } = first.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderPartiallyFilled);
    assert_eq!(order.filled_quantity, dec!(6000));
    assert_eq!(order.remaining_quantity, dec!(4000));
}

#[tokio::test]
async fn submit_partial_fill_without_cancel_emits_partial_only() {
    // Defensive path: a partial fill with no accompanying cancel transaction
    // (not observed from OANDA in practice). The real quantities are reported
    // and the order stays non-terminal — no fabricated cancel event.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let (tx, mut rx) = mpsc::unbounded_channel();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"units": "6000"})),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::PartiallyFilled);

    let event = rx.try_recv().expect("partial fill event published");
    let WsMessage::Order {
        event: event_type,
        order,
        ..
    } = event.msg
    else {
        panic!("expected an order event");
    };
    assert_eq!(event_type, OrderEventType::OrderPartiallyFilled);
    assert_eq!(order.status, OrderStatus::PartiallyFilled);
    assert_eq!(order.filled_quantity, dec!(6000));
    assert_eq!(order.remaining_quantity, dec!(4000));

    assert!(rx.try_recv().is_err(), "exactly one event expected");
}

#[tokio::test]
async fn submit_cancel_without_create_falls_back_to_cancel_id() {
    // Defensive path: a cancel transaction with no create transaction. Not
    // observed from Oanda in practice, but the handle must still be terminal.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let mut body = oanda_ioc_cancel_json();
    body.as_object_mut()
        .unwrap()
        .remove("orderCreateTransaction");
    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        body,
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        time_in_force: TimeInForce::Ioc,
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "1982");
    assert_eq!(handle.status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn submit_stop_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_pending_order_json(&json!({"id": "790"})),
    )
    .await;

    let request = OrderRequest {
        stop_price: Some(dec!(1.09000)),
        ..forex_order("EUR_USD", Side::Sell, OrderType::Stop, dec!(5000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "790");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_order_with_bracket() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"id": "460"})),
    )
    .await;

    let request = OrderRequest {
        stop_loss: Some(dec!(1.08000)),
        take_profit: Some(dec!(1.12000)),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "460");
    assert_eq!(handle.status, OrderStatus::Filled);
}

#[tokio::test]
async fn submit_bracket_stamps_leg_client_ids() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"id": "461"})),
    )
    .await;

    let request = OrderRequest {
        stop_loss: Some(dec!(1.08000)),
        take_profit: Some(dec!(1.12000)),
        client_order_id: Some("strat-42".to_string()),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000))
    };
    adapter.submit_order(&request).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = requests[0].body_json().unwrap();
    let order = &body["order"];
    assert_eq!(order["clientExtensions"]["id"], "strat-42");
    assert_eq!(
        order["stopLossOnFill"]["clientExtensions"]["id"],
        "strat-42-sl"
    );
    assert_eq!(
        order["takeProfitOnFill"]["clientExtensions"]["id"],
        "strat-42-tp"
    );
}

#[tokio::test]
async fn submit_bracket_without_client_id_omits_leg_client_extensions() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"id": "462"})),
    )
    .await;

    let request = OrderRequest {
        stop_loss: Some(dec!(1.08000)),
        take_profit: Some(dec!(1.12000)),
        client_order_id: None,
        ..forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000))
    };
    adapter.submit_order(&request).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = requests[0].body_json().unwrap();
    let order = &body["order"];
    // The legs themselves must still be present — only clientExtensions is omitted.
    assert_eq!(order["stopLossOnFill"]["price"], "1.08000");
    assert_eq!(order["takeProfitOnFill"]["price"], "1.12000");
    assert!(order["stopLossOnFill"].get("clientExtensions").is_none());
    assert!(order["takeProfitOnFill"].get("clientExtensions").is_none());
}

#[tokio::test]
async fn submit_bracket_with_empty_client_id_omits_leg_client_extensions() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_market_fill_json(&json!({"id": "463"})),
    )
    .await;

    let request = OrderRequest {
        stop_loss: Some(dec!(1.08000)),
        take_profit: Some(dec!(1.12000)),
        client_order_id: Some(String::new()),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000))
    };
    adapter.submit_order(&request).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = requests[0].body_json().unwrap();
    let order = &body["order"];
    assert_eq!(order["stopLossOnFill"]["price"], "1.08000");
    assert_eq!(order["takeProfitOnFill"]["price"], "1.12000");
    assert!(order["stopLossOnFill"].get("clientExtensions").is_none());
    assert!(order["takeProfitOnFill"].get("clientExtensions").is_none());
    // An empty client id must not be sent on the parent either.
    assert!(order.get("clientExtensions").is_none());
}

#[tokio::test]
async fn submit_order_with_client_order_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_pending_order_json(&json!({})),
    )
    .await;

    let request = OrderRequest {
        limit_price: Some(dec!(1.10000)),
        client_order_id: Some("my-client-001".to_string()),
        ..forex_order("EUR_USD", Side::Buy, OrderType::Limit, dec!(10000))
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.client_order_id, Some("my-client-001".to_string()));
}

#[tokio::test]
async fn submit_order_rejected_via_transaction() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        201,
        oanda_reject_json("INSUFFICIENT_MARGIN"),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let err = adapter.submit_order(&request).await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, Some("INSUFFICIENT_FUNDS".to_string()));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_order_http_422() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v3/accounts/test-account-123/orders",
        422,
        json!({"errorMessage": "Order rejected", "rejectReason": "MARKET_HALTED"}),
    )
    .await;

    let request = forex_order("EUR_USD", Side::Buy, OrderType::Market, dec!(10000));
    let err = adapter.submit_order(&request).await.unwrap_err();
    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            assert_eq!(reject_code, Some("MARKET_CLOSED".to_string()));
        }
        other => panic!("Expected OrderRejected, got {other:?}"),
    }
}

// =========================================================================
// Get Order
// =========================================================================

#[tokio::test]
async fn get_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/123",
        200,
        json!({"order": oanda_order_json(&json!({}))}),
    )
    .await;

    let order = adapter.get_order("123").await.unwrap();
    assert_eq!(order.id, "123");
    assert_eq!(order.symbol, "EUR_USD");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.order_type, OrderType::Limit);
    assert_eq!(order.quantity, dec!(10000));
    assert_eq!(order.limit_price, Some(dec!(1.10000)));
    assert_eq!(order.status, OrderStatus::Open);
    assert_eq!(order.time_in_force, TimeInForce::Gtc);
}

#[tokio::test]
async fn get_order_sell_side() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/124",
        200,
        json!({"order": oanda_order_json(&json!({"id": "124", "units": "-5000"}))}),
    )
    .await;

    let order = adapter.get_order("124").await.unwrap();
    assert_eq!(order.side, Side::Sell);
    assert_eq!(order.quantity, dec!(5000));
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/999",
        404,
        json!({"errorMessage": "Order not found"}),
    )
    .await;

    let err = adapter.get_order("999").await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

// =========================================================================
// Get Orders (list)
// =========================================================================

#[tokio::test]
async fn get_orders_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [
                oanda_order_json(&json!({"id": "1"})),
                oanda_order_json(&json!({"id": "2"})),
            ]
        }),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].id, "1");
    assert_eq!(orders[1].id, "2");
}

#[tokio::test]
async fn get_orders_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({"orders": []}),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_orders(&params).await.unwrap();
    assert!(orders.is_empty());
}

#[tokio::test]
async fn get_orders_with_symbol_filter() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [oanda_order_json(&json!({}))]
        }),
    )
    .await;

    let params = OrderQueryParams {
        symbol: Some("EUR_USD".to_string()),
        ..Default::default()
    };
    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 1);
}

// =========================================================================
// Get Order History
// =========================================================================

#[tokio::test]
async fn get_order_history() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [
                oanda_order_json(&json!({"id": "10", "state": "FILLED"})),
                oanda_order_json(&json!({"id": "11", "state": "CANCELLED"})),
            ]
        }),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_order_history(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].status, OrderStatus::Filled);
    assert_eq!(orders[1].status, OrderStatus::Cancelled);
}

// =========================================================================
// Modify Order (PUT replacement)
// =========================================================================

#[tokio::test]
async fn modify_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // 1. GET current order
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/100",
        200,
        json!({"order": oanda_order_json(&json!({"id": "100"}))}),
    )
    .await;

    // 2. PUT replacement -> returns orderCreateTransaction with new ID
    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/100",
        201,
        json!({
            "orderCancelTransaction": {"id": "101", "type": "ORDER_CANCEL"},
            "orderCreateTransaction": {"id": "102", "type": "LIMIT_ORDER"}
        }),
    )
    .await;

    // 3. GET the new order (adapter fetches the replacement)
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/102",
        200,
        json!({"order": oanda_order_json(&json!({"id": "102", "price": "1.11000"}))}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(1.11000)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let result = adapter.modify_order("100", &request).await.unwrap();
    assert_eq!(result.order.id, "102");
    assert_eq!(result.previous_order_id, Some("100".to_string()));
    assert_eq!(result.order.limit_price, Some(dec!(1.11000)));
}

#[tokio::test]
async fn modify_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders/999",
        404,
        json!({"errorMessage": "Order not found"}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(1.11000)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let err = adapter.modify_order("999", &request).await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

// =========================================================================
// Cancel Order
// =========================================================================

#[tokio::test]
async fn cancel_order_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/100/cancel",
        200,
        json!({"orderCancelTransaction": {"id": "100", "type": "ORDER_CANCEL"}}),
    )
    .await;

    let result = adapter.cancel_order("100").await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.id, "100");
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn cancel_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/999/cancel",
        404,
        json!({"errorMessage": "Order not found"}),
    )
    .await;

    let err = adapter.cancel_order("999").await.unwrap_err();
    assert!(matches!(err, GatewayError::OrderNotFound { .. }));
}

// =========================================================================
// Cancel All Orders (default trait impl)
// =========================================================================

#[tokio::test]
async fn cancel_all_orders() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // GET open orders returns 2
    mount_json(
        &server,
        "GET",
        "/v3/accounts/test-account-123/orders",
        200,
        json!({
            "orders": [
                oanda_order_json(&json!({"id": "1"})),
                oanda_order_json(&json!({"id": "2"})),
            ]
        }),
    )
    .await;

    // Cancel each
    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/1/cancel",
        200,
        json!({"orderCancelTransaction": {"id": "1", "type": "ORDER_CANCEL"}}),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        "/v3/accounts/test-account-123/orders/2/cancel",
        200,
        json!({"orderCancelTransaction": {"id": "2", "type": "ORDER_CANCEL"}}),
    )
    .await;

    let result = adapter.cancel_all_orders(None).await.unwrap();
    assert_eq!(result.cancelled_count, 2);
}
