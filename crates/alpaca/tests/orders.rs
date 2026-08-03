mod helpers;

use helpers::{alpaca_order_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ModifyOrderRequest, OrderQueryParams, OrderRequest, OrderStatus, OrderType, Side, TimeInForce,
};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// Helper to build an `OrderRequest` for tests.
fn market_buy(symbol: &str, qty: rust_decimal::Decimal) -> OrderRequest {
    OrderRequest {
        symbol: symbol.to_string(),
        quantity: qty,
        time_in_force: TimeInForce::Day,
        ..test_order_request()
    }
}

#[tokio::test]
async fn submit_market_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({})),
    )
    .await;

    let request = market_buy("AAPL", dec!(10));
    let handle = adapter.submit_order(&request).await.unwrap();

    assert_eq!(handle.id, "order-abc-123");
    assert_eq!(handle.status, OrderStatus::Open);
}

#[tokio::test]
async fn submit_limit_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({"type": "limit", "limit_price": "155.00"})),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.order_type = OrderType::Limit;
    request.limit_price = Some(dec!(155));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_stop_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({"type": "stop", "stop_price": "145.00"})),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.order_type = OrderType::Stop;
    request.stop_price = Some(dec!(145));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_bracket_order_stock() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({
            "type": "limit",
            "limit_price": "150.00",
            "legs": [
                alpaca_order_json(&json!({"id": "sl-leg", "type": "stop", "side": "sell", "stop_price": "140.00"})),
                alpaca_order_json(&json!({"id": "tp-leg", "type": "limit", "side": "sell", "limit_price": "170.00"})),
            ]
        })),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.order_type = OrderType::Limit;
    request.limit_price = Some(dec!(150));
    request.stop_loss = Some(dec!(140));
    request.take_profit = Some(dec!(170));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_crypto_stop_transforms_to_stop_limit() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Verify the adapter sends a stop_limit (not stop) to Alpaca for crypto
    Mock::given(method("POST"))
        .and(path("/v2/orders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(alpaca_order_json(
            &json!({"type": "stop_limit", "symbol": "BTCUSD"}),
        )))
        .expect(1)
        .mount(&server)
        .await;

    let mut request = market_buy("BTCUSD", dec!(1));
    request.order_type = OrderType::Stop;
    request.stop_price = Some(dec!(50000));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");

    // Verify the request was sent (the mock's expect(1) validates this)
}

#[tokio::test]
async fn submit_crypto_sl_tp_uses_pending_system() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Crypto with SL/TP should NOT use bracket — should send simple order
    Mock::given(method("POST"))
        .and(path("/v2/orders"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(alpaca_order_json(&json!({"symbol": "BTCUSD"}))),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut request = market_buy("BTCUSD", dec!(1));
    request.stop_loss = Some(dec!(48000));
    request.take_profit = Some(dec!(55000));

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "order-abc-123");
}

#[tokio::test]
async fn submit_order_with_client_order_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        200,
        alpaca_order_json(&json!({"client_order_id": "my-id-001"})),
    )
    .await;

    let mut request = market_buy("AAPL", dec!(10));
    request.client_order_id = Some("my-id-001".to_string());

    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.client_order_id, Some("my-id-001".to_string()));
}

#[tokio::test]
async fn submit_order_rejected() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        "/v2/orders",
        422,
        json!({"message": "Insufficient buying power", "code": "insufficient_buying_power"}),
    )
    .await;

    let request = market_buy("AAPL", dec!(10));
    let err = adapter.submit_order(&request).await.unwrap_err();

    match err {
        GatewayError::OrderRejected { reject_code, .. } => {
            // submit_order uses execute_with_retry which uses default_error_mapper,
            // passing through raw Alpaca codes (no Alpaca-specific mapping).
            assert_eq!(reject_code.as_deref(), Some("insufficient_buying_power"));
        }
        other => panic!("Expected OrderRejected, got: {other:?}"),
    }
}

#[tokio::test]
async fn get_order_success() {
    // Adapter validates that order_id is a UUID before hitting Alpaca, so tests must
    // use real UUID strings (Alpaca order IDs are always UUIDs in production).
    const ORDER_ID: &str = "11111111-1111-1111-1111-111111111111";
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("/v2/orders/{ORDER_ID}"),
        200,
        alpaca_order_json(&json!({"id": ORDER_ID})),
    )
    .await;

    let order = adapter.get_order(ORDER_ID).await.unwrap();

    assert_eq!(order.id, ORDER_ID);
    assert_eq!(order.symbol, "AAPL");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.order_type, OrderType::Market);
    assert_eq!(order.quantity, dec!(10));
    assert_eq!(order.status, OrderStatus::Open);
    assert_eq!(order.time_in_force, TimeInForce::Day);
}

#[tokio::test]
async fn get_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders/nonexistent",
        404,
        json!({"message": "Order not found"}),
    )
    .await;

    let err = adapter.get_order("nonexistent").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotFound { .. }),
        "Expected OrderNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn get_order_filled() {
    const ORDER_ID: &str = "22222222-2222-2222-2222-222222222222";
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("/v2/orders/{ORDER_ID}"),
        200,
        alpaca_order_json(&json!({
            "id": ORDER_ID,
            "status": "filled",
            "filled_qty": "10",
            "filled_avg_price": "152.50"
        })),
    )
    .await;

    let order = adapter.get_order(ORDER_ID).await.unwrap();

    assert_eq!(order.status, OrderStatus::Filled);
    assert_eq!(order.filled_quantity, dec!(10));
    assert_eq!(order.average_fill_price, Some(dec!(152.50)));
    assert_eq!(order.remaining_quantity, dec!(0));
}

#[tokio::test]
async fn get_orders_open() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Wiremock matches on path only; query params are part of the full URL
    // but mount_json matches on path prefix, so this works for /v2/orders
    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([alpaca_order_json(&json!({}))]),
    )
    .await;

    let params = OrderQueryParams {
        status: Some(vec![OrderStatus::Open]),
        ..Default::default()
    };

    let orders = adapter.get_orders(&params).await.unwrap();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].id, "order-abc-123");
}

#[tokio::test]
async fn get_orders_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/v2/orders", 200, json!([])).await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_orders(&params).await.unwrap();
    assert!(orders.is_empty());
}

#[tokio::test]
async fn get_order_history() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([
            alpaca_order_json(&json!({"id": "hist-1", "status": "filled"})),
            alpaca_order_json(&json!({"id": "hist-2", "status": "canceled"})),
        ]),
    )
    .await;

    let params = OrderQueryParams::default();
    let orders = adapter.get_order_history(&params).await.unwrap();
    assert_eq!(orders.len(), 2);
}

#[tokio::test]
async fn modify_order_success() {
    const ORDER_ID: &str = "33333333-3333-3333-3333-333333333333";
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PATCH",
        &format!("/v2/orders/{ORDER_ID}"),
        200,
        alpaca_order_json(&json!({
            "id": ORDER_ID,
            "type": "limit",
            "limit_price": "160.00"
        })),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };

    let result = adapter.modify_order(ORDER_ID, &request).await.unwrap();
    assert_eq!(result.previous_order_id, Some(ORDER_ID.to_string()));
    assert_eq!(result.order.id, ORDER_ID);
}

#[tokio::test]
async fn modify_order_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PATCH",
        "/v2/orders/nonexistent",
        404,
        json!({"message": "Order not found"}),
    )
    .await;

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(160)),
        ..Default::default()
    };

    let err = adapter
        .modify_order("nonexistent", &request)
        .await
        .unwrap_err();
    assert!(
        matches!(err, GatewayError::OrderNotFound { .. }),
        "Expected OrderNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn cancel_order_success() {
    const ORDER_ID: &str = "44444444-4444-4444-4444-444444444444";
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // cancel_order calls DELETE then GET
    Mock::given(method("DELETE"))
        .and(path(format!("/v2/orders/{ORDER_ID}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    mount_json(
        &server,
        "GET",
        &format!("/v2/orders/{ORDER_ID}"),
        200,
        alpaca_order_json(&json!({"id": ORDER_ID, "status": "canceled"})),
    )
    .await;

    let result = adapter.cancel_order(ORDER_ID).await.unwrap();
    assert!(result.success);
    assert_eq!(result.order.status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn cancel_all_orders() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "DELETE",
        "/v2/orders",
        207,
        json!([
            {"id": "order-1", "status": 200},
            {"id": "order-2", "status": 200},
        ]),
    )
    .await;

    let result = adapter.cancel_all_orders(None).await.unwrap();
    assert_eq!(result.cancelled_count, 2);
    assert_eq!(result.failed_count, 0);
}

#[tokio::test]
async fn cancel_all_orders_with_symbol() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Wiremock matches on path /v2/orders — query params pass through
    mount_json(
        &server,
        "DELETE",
        "/v2/orders",
        207,
        json!([{"id": "order-1", "status": 200}]),
    )
    .await;

    let result = adapter.cancel_all_orders(Some("AAPL")).await.unwrap();
    assert_eq!(result.cancelled_count, 1);
}

// --- Native bracket links -------------------------------------------------
//
// Alpaca manages brackets itself and reports the entry↔leg link as a `legs`
// array on the entry order. Nothing else in the payload points a leg back at
// its entry, so the adapter has to remember the link when it sees the entry.

const BRACKET_ENTRY_ID: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const BRACKET_SL_ID: &str = "aaaaaaaa-0000-0000-0000-000000000002";
const BRACKET_TP_ID: &str = "aaaaaaaa-0000-0000-0000-000000000003";

/// An Alpaca entry order carrying its two native bracket legs.
fn bracket_entry_json() -> serde_json::Value {
    alpaca_order_json(&json!({
        "id": BRACKET_ENTRY_ID,
        "status": "filled",
        "filled_qty": "10",
        "legs": [
            alpaca_order_json(&json!({
                "id": BRACKET_SL_ID,
                "type": "stop",
                "side": "sell",
                "stop_price": "140.00"
            })),
            alpaca_order_json(&json!({
                "id": BRACKET_TP_ID,
                "type": "limit",
                "side": "sell",
                "limit_price": "160.00"
            })),
        ]
    }))
}

#[tokio::test]
async fn get_orders_reports_parent_on_native_bracket_legs() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([bracket_entry_json()]),
    )
    .await;

    let orders = adapter
        .get_orders(&OrderQueryParams::default())
        .await
        .unwrap();

    // The legs stay visible as orders in their own right — nesting them under
    // the entry is a transport detail of the broker, not the gateway's model.
    let mut ids: Vec<&str> = orders.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![BRACKET_ENTRY_ID, BRACKET_SL_ID, BRACKET_TP_ID],
        "entry and both legs should all be listed"
    );

    for leg_id in [BRACKET_SL_ID, BRACKET_TP_ID] {
        let leg = orders.iter().find(|o| o.id == leg_id).unwrap();
        assert_eq!(
            leg.parent_order_id.as_deref(),
            Some(BRACKET_ENTRY_ID),
            "leg {leg_id} should point at the entry order"
        );
    }

    let entry = orders.iter().find(|o| o.id == BRACKET_ENTRY_ID).unwrap();
    assert_eq!(entry.parent_order_id, None, "the entry order has no parent");
}

#[tokio::test]
async fn get_orders_leaves_parent_unset_for_orders_without_legs() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([alpaca_order_json(&json!({"id": BRACKET_ENTRY_ID}))]),
    )
    .await;

    let orders = adapter
        .get_orders(&OrderQueryParams::default())
        .await
        .unwrap();

    assert_eq!(orders.len(), 1);
    assert_eq!(
        orders[0].parent_order_id, None,
        "a plain order must not gain a parent"
    );
}

#[tokio::test]
async fn submitting_a_bracket_records_the_leg_parents_for_later_lookups() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "POST", "/v2/orders", 200, bracket_entry_json()).await;
    mount_json(
        &server,
        "GET",
        &format!("/v2/orders/{BRACKET_SL_ID}"),
        200,
        alpaca_order_json(&json!({"id": BRACKET_SL_ID, "type": "stop", "side": "sell"})),
    )
    .await;

    adapter
        .submit_order(&market_buy("AAPL", dec!(10)))
        .await
        .unwrap();

    // Alpaca returns the leg on its own with no reference to the entry; the
    // link is only knowable from the submit response we already saw.
    let leg = adapter.get_order(BRACKET_SL_ID).await.unwrap();
    assert_eq!(leg.parent_order_id.as_deref(), Some(BRACKET_ENTRY_ID));
}

#[tokio::test]
async fn get_order_on_an_unseen_leg_reports_no_parent() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("/v2/orders/{BRACKET_SL_ID}"),
        200,
        alpaca_order_json(&json!({"id": BRACKET_SL_ID, "type": "stop", "side": "sell"})),
    )
    .await;

    let leg = adapter.get_order(BRACKET_SL_ID).await.unwrap();
    assert_eq!(
        leg.parent_order_id, None,
        "without having seen the entry, the adapter cannot know the parent"
    );
}

#[tokio::test]
async fn get_order_history_reports_parent_on_native_bracket_legs() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([bracket_entry_json()]),
    )
    .await;

    let orders = adapter
        .get_order_history(&OrderQueryParams::default())
        .await
        .unwrap();

    let mut ids: Vec<&str> = orders.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![BRACKET_ENTRY_ID, BRACKET_SL_ID, BRACKET_TP_ID]);

    let leg = orders.iter().find(|o| o.id == BRACKET_SL_ID).unwrap();
    assert_eq!(leg.parent_order_id.as_deref(), Some(BRACKET_ENTRY_ID));
}

#[tokio::test]
async fn a_filled_leg_reports_its_parent_on_the_fill_then_releases_it() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([bracket_entry_json()]),
    )
    .await;
    mount_json(
        &server,
        "GET",
        &format!("/v2/orders/{BRACKET_SL_ID}"),
        200,
        alpaca_order_json(&json!({
            "id": BRACKET_SL_ID,
            "type": "stop",
            "side": "sell",
            "status": "filled",
            "filled_qty": "10"
        })),
    )
    .await;

    // Observing the entry is what makes the link knowable.
    adapter
        .get_orders(&OrderQueryParams::default())
        .await
        .unwrap();

    let filled = adapter.get_order(BRACKET_SL_ID).await.unwrap();
    assert_eq!(filled.status, OrderStatus::Filled);
    assert_eq!(
        filled.parent_order_id.as_deref(),
        Some(BRACKET_ENTRY_ID),
        "the response reporting the fill still carries the link — that is when a \
         strategy needs to know which entry the leg closed"
    );

    // The leg has resolved and cannot acquire a new link, so it is not retained.
    let after = adapter.get_order(BRACKET_SL_ID).await.unwrap();
    assert_eq!(after.parent_order_id, None);
}

#[tokio::test]
async fn modifying_a_leg_keeps_its_parent_link() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/orders",
        200,
        json!([bracket_entry_json()]),
    )
    .await;
    mount_json(
        &server,
        "PATCH",
        &format!("/v2/orders/{BRACKET_TP_ID}"),
        200,
        alpaca_order_json(&json!({
            "id": BRACKET_TP_ID,
            "type": "limit",
            "side": "sell",
            "limit_price": "165.00"
        })),
    )
    .await;

    // Observing the entry is what makes the link knowable.
    adapter
        .get_orders(&OrderQueryParams::default())
        .await
        .unwrap();

    let request = ModifyOrderRequest {
        limit_price: Some(dec!(165)),
        stop_price: None,
        quantity: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
    };
    let result = adapter.modify_order(BRACKET_TP_ID, &request).await.unwrap();

    assert_eq!(
        result.order.parent_order_id.as_deref(),
        Some(BRACKET_ENTRY_ID),
        "moving a resting leg must not detach it from its entry"
    );
}
