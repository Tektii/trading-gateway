mod helpers;

use helpers::{alpaca_order_json, alpaca_position_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{ClosePositionRequest, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

// =========================================================================
// Get Positions
// =========================================================================

#[tokio::test]
async fn get_positions_all() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/positions",
        200,
        json!([alpaca_position_json("AAPL"), alpaca_position_json("MSFT"),]),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].symbol, "AAPL");
    assert_eq!(positions[1].symbol, "MSFT");
}

#[tokio::test]
async fn get_positions_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(&server, "GET", "/v2/positions", 200, json!([])).await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

// =========================================================================
// Get Position
// =========================================================================

#[tokio::test]
async fn get_position_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/positions/AAPL",
        200,
        alpaca_position_json("AAPL"),
    )
    .await;

    let position = adapter.get_position("AAPL").await.unwrap();

    assert_eq!(position.id, "AAPL_position");
    assert_eq!(position.symbol, "AAPL");
    assert_eq!(position.side, PositionSide::Long);
    assert_eq!(position.quantity, dec!(10));
    assert_eq!(position.average_entry_price, dec!(150));
    assert_eq!(position.current_price, dec!(155));
    assert_eq!(position.unrealized_pnl, dec!(50));
}

#[tokio::test]
async fn get_position_short() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/positions/TSLA",
        200,
        json!({
            "symbol": "TSLA",
            "qty": "-5",
            "avg_entry_price": "200.00",
            "current_price": "190.00",
            "market_value": "-950.00",
            "unrealized_pl": "50.00",
            "unrealized_plpc": "0.05"
        }),
    )
    .await;

    let position = adapter.get_position("TSLA").await.unwrap();

    assert_eq!(position.side, PositionSide::Short);
    assert_eq!(position.quantity, dec!(5)); // abs value
}

#[tokio::test]
async fn get_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/positions/NOPE",
        404,
        json!({"message": "Position not found"}),
    )
    .await;

    let err = adapter.get_position("NOPE").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::PositionNotFound { .. }),
        "Expected PositionNotFound, got: {err:?}"
    );
}

// =========================================================================
// Close Position
// =========================================================================

#[tokio::test]
async fn close_position_full() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    Mock::given(method("DELETE"))
        .and(path("/v2/positions/AAPL"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(alpaca_order_json(&json!({
                "id": "close-order-1",
                "symbol": "AAPL",
                "side": "sell"
            }))),
        )
        .mount(&server)
        .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };

    let handle = adapter.close_position("AAPL", &request).await.unwrap();
    assert_eq!(handle.id, "close-order-1");
}

#[tokio::test]
async fn close_position_partial() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    // Partial close: DELETE /v2/positions/AAPL?qty=5
    // Wiremock path matcher doesn't include query params, so this works
    Mock::given(method("DELETE"))
        .and(path("/v2/positions/AAPL"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(alpaca_order_json(&json!({
                "id": "partial-close-1",
                "symbol": "AAPL",
                "side": "sell",
                "qty": "5"
            }))),
        )
        .mount(&server)
        .await;

    let request = ClosePositionRequest {
        quantity: Some(dec!(5)),
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };

    let handle = adapter.close_position("AAPL", &request).await.unwrap();
    assert_eq!(handle.id, "partial-close-1");
}

#[tokio::test]
async fn close_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    Mock::given(method("DELETE"))
        .and(path("/v2/positions/NOPE"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "not found"})))
        .mount(&server)
        .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };

    let err = adapter.close_position("NOPE", &request).await.unwrap_err();
    assert!(
        matches!(err, GatewayError::PositionNotFound { .. }),
        "Expected PositionNotFound, got: {err:?}"
    );
}

// =========================================================================
// Close All Positions
// =========================================================================

#[tokio::test]
async fn close_all_positions() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    Mock::given(method("DELETE"))
        .and(path("/v2/positions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            alpaca_order_json(&json!({"id": "close-1", "side": "sell"})),
            alpaca_order_json(&json!({"id": "close-2", "side": "sell"})),
        ])))
        .mount(&server)
        .await;

    let handles = adapter.close_all_positions(None).await.unwrap();
    assert_eq!(handles.len(), 2);
    assert_eq!(handles[0].id, "close-1");
    assert_eq!(handles[1].id, "close-2");
}
