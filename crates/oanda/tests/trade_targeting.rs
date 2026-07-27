//! Per-trade targeting on hedging Oanda accounts.
//!
//! A hedging account can hold several same-side trades on one instrument. A
//! position id naming an Oanda trade id must reduce exactly that trade rather
//! than letting Oanda pick.

mod helpers;

use helpers::{oanda_trade_close_json, oanda_trade_json_with_id, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{
    ClosePositionRequest, OrderRequest, OrderStatus, PositionSide, Side,
};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};
use wiremock::MockServer;

const ACCOUNT: &str = "/v3/accounts/test-account-123";

async fn paths_hit(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("request log enabled")
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

async fn close_body(server: &MockServer, path: &str) -> serde_json::Value {
    let requests = server
        .received_requests()
        .await
        .expect("request log enabled");
    let request = requests
        .iter()
        .find(|r| r.url.path() == path)
        .unwrap_or_else(|| panic!("no request to {path}"));
    serde_json::from_slice(&request.body).expect("close body is json")
}

#[tokio::test]
async fn reduce_only_market_order_closes_the_named_trade() {
    // Two same-side long trades on one instrument: the exit must reduce the
    // trade it names, not whichever one Oanda would have picked.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": oanda_trade_json_with_id("222", "EUR_USD", "10000") }),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/222/close"),
        200,
        oanda_trade_close_json("901", "-10000"),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/111/close"),
        200,
        oanda_trade_close_json("902", "-10000"),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(10000))
        .reduce_only()
        .for_position("222");
    let handle = adapter.submit_order(&request).await.unwrap();

    assert_eq!(handle.id, "901");
    assert_eq!(handle.status, OrderStatus::Filled);

    let hit = paths_hit(&server).await;
    assert!(
        hit.contains(&format!("PUT {ACCOUNT}/trades/222/close")),
        "named trade was closed: {hit:?}"
    );
    assert!(
        !hit.contains(&format!("PUT {ACCOUNT}/trades/111/close")),
        "the other same-side trade was left alone: {hit:?}"
    );
    assert!(
        !hit.iter().any(|p| p.ends_with("/orders")),
        "no generic order was submitted: {hit:?}"
    );
}

#[tokio::test]
async fn reduce_only_market_order_closes_only_the_requested_units() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": oanda_trade_json_with_id("222", "EUR_USD", "10000") }),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/222/close"),
        200,
        oanda_trade_close_json("901", "-4000"),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(4000))
        .reduce_only()
        .for_position("222");
    adapter.submit_order(&request).await.unwrap();

    let body = close_body(&server, &format!("{ACCOUNT}/trades/222/close")).await;
    assert_eq!(body["units"], "4000");
}

#[tokio::test]
async fn reduce_only_market_order_closing_whole_trade_sends_all() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": oanda_trade_json_with_id("222", "EUR_USD", "10000") }),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/222/close"),
        200,
        oanda_trade_close_json("901", "-10000"),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(10000))
        .reduce_only()
        .for_position("222");
    adapter.submit_order(&request).await.unwrap();

    let body = close_body(&server, &format!("{ACCOUNT}/trades/222/close")).await;
    assert_eq!(
        body["units"], "ALL",
        "a full-size reduction closes the trade outright"
    );
}

#[tokio::test]
async fn reduce_only_market_order_larger_than_the_trade_closes_it_whole() {
    // The caller's view of the trade can lag the broker's. Reduce-only means
    // never open, so the overshoot closes what is actually there instead of
    // sending units Oanda would reject.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": oanda_trade_json_with_id("222", "EUR_USD", "10000") }),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/222/close"),
        200,
        oanda_trade_close_json("901", "-10000"),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(15000))
        .reduce_only()
        .for_position("222");
    adapter.submit_order(&request).await.unwrap();

    let body = close_body(&server, &format!("{ACCOUNT}/trades/222/close")).await;
    assert_eq!(body["units"], "ALL");
}

#[tokio::test]
async fn reduce_only_market_order_rejects_an_already_closed_trade() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let mut trade = oanda_trade_json_with_id("222", "EUR_USD", "0");
    trade["state"] = json!("CLOSED");
    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": trade }),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(10000))
        .reduce_only()
        .for_position("222");
    let error = adapter.submit_order(&request).await.unwrap_err();

    assert!(
        matches!(error, GatewayError::PositionNotFound { ref id } if id == "222"),
        "got {error:?}"
    );
    let hit = paths_hit(&server).await;
    assert!(
        !hit.iter().any(|p| p.contains("/close")),
        "nothing was closed: {hit:?}"
    );
}

#[tokio::test]
async fn reduce_only_market_order_rejects_trade_on_another_instrument() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": oanda_trade_json_with_id("222", "GBP_USD", "10000") }),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(10000))
        .reduce_only()
        .for_position("222");
    let error = adapter.submit_order(&request).await.unwrap_err();

    assert!(
        matches!(error, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("position_id")),
        "expected the position id to be rejected, got {error:?}"
    );
    let hit = paths_hit(&server).await;
    assert!(
        !hit.iter().any(|p| p.contains("/close")),
        "nothing was closed: {hit:?}"
    );
}

#[tokio::test]
async fn reduce_only_market_order_rejects_trade_on_the_same_side() {
    // Buying against a long trade would grow it, not reduce it.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": oanda_trade_json_with_id("222", "EUR_USD", "10000") }),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Buy, dec!(10000))
        .reduce_only()
        .for_position("222");
    let error = adapter.submit_order(&request).await.unwrap_err();

    assert!(
        matches!(error, GatewayError::InvalidRequest { ref field, .. } if field.as_deref() == Some("position_id")),
        "expected the position id to be rejected, got {error:?}"
    );
    let hit = paths_hit(&server).await;
    assert!(
        !hit.iter().any(|p| p.contains("/close")),
        "nothing was closed: {hit:?}"
    );
}

#[tokio::test]
async fn reduce_only_market_order_rejects_unknown_trade() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        404,
        json!({"errorMessage": "The trade specified does not exist"}),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(10000))
        .reduce_only()
        .for_position("222");
    let error = adapter.submit_order(&request).await.unwrap_err();

    assert!(
        matches!(error, GatewayError::PositionNotFound { ref id } if id == "222"),
        "expected the named trade to be reported missing, got {error:?}"
    );
}

#[tokio::test]
async fn reduce_only_order_naming_a_side_still_uses_position_fill() {
    // A side-level position id cannot name a trade, so the order goes to
    // Oanda as before and `positionFill` does the reduce-only enforcement.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        &format!("{ACCOUNT}/orders"),
        201,
        json!({
            "orderFillTransaction": {
                "id": "456",
                "type": "ORDER_FILL",
                "instrument": "EUR_USD",
                "units": "-10000",
                "price": "1.10500",
                "time": "2024-01-15T11:00:00.000000000Z"
            }
        }),
    )
    .await;

    let request = OrderRequest::market("EUR_USD", Side::Sell, dec!(10000))
        .reduce_only()
        .for_position("EUR_USD_LONG");
    adapter.submit_order(&request).await.unwrap();

    let body = close_body(&server, &format!("{ACCOUNT}/orders")).await;
    assert_eq!(body["order"]["positionFill"], "REDUCE_ONLY");
}

#[tokio::test]
async fn reduce_only_limit_order_naming_a_trade_still_uses_position_fill() {
    // Oanda closes a trade at market. A resting leg cannot be routed there, so
    // it keeps the position-level enforcement until per-trade resting exits are
    // mapped to Oanda's STOP_LOSS/TAKE_PROFIT order types.
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "POST",
        &format!("{ACCOUNT}/orders"),
        201,
        json!({
            "orderCreateTransaction": {
                "id": "789",
                "type": "LIMIT_ORDER",
                "instrument": "EUR_USD",
                "units": "-10000",
                "time": "2024-01-15T11:00:00.000000000Z"
            }
        }),
    )
    .await;

    let request = OrderRequest::limit("EUR_USD", Side::Sell, dec!(10000), dec!(1.2))
        .reduce_only()
        .for_position("222");
    adapter.submit_order(&request).await.unwrap();

    let body = close_body(&server, &format!("{ACCOUNT}/orders")).await;
    assert_eq!(body["order"]["positionFill"], "REDUCE_ONLY");
}

#[tokio::test]
async fn get_position_resolves_a_trade_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": oanda_trade_json_with_id("222", "EUR_USD", "10000") }),
    )
    .await;

    let position = adapter.get_position("222").await.unwrap();
    assert_eq!(position.id, "222");
    assert_eq!(position.symbol, "EUR_USD");
    assert_eq!(position.side, PositionSide::Long);
    assert_eq!(position.quantity, dec!(10000));
    assert_eq!(position.average_entry_price, dec!(1.10000));
    assert_eq!(position.unrealized_pnl, dec!(50.00));
}

#[tokio::test]
async fn get_position_resolves_a_short_trade_id() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/333"),
        200,
        json!({ "trade": oanda_trade_json_with_id("333", "EUR_USD", "-5000") }),
    )
    .await;

    let position = adapter.get_position("333").await.unwrap();
    assert_eq!(position.side, PositionSide::Short);
    assert_eq!(position.quantity, dec!(5000));
}

#[tokio::test]
async fn get_position_rejects_a_closed_trade() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    let mut trade = oanda_trade_json_with_id("222", "EUR_USD", "0");
    trade["state"] = json!("CLOSED");
    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        200,
        json!({ "trade": trade }),
    )
    .await;

    let error = adapter.get_position("222").await.unwrap_err();
    assert!(
        matches!(error, GatewayError::PositionNotFound { ref id } if id == "222"),
        "a closed trade is not an open position, got {error:?}"
    );
    assert!(
        paths_hit(&server)
            .await
            .contains(&format!("GET {ACCOUNT}/trades/222")),
        "the trade endpoint answered the lookup"
    );
}

#[tokio::test]
async fn get_position_reports_an_unknown_trade_as_missing() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        &format!("{ACCOUNT}/trades/222"),
        404,
        json!({"errorMessage": "The trade specified does not exist"}),
    )
    .await;

    let error = adapter.get_position("222").await.unwrap_err();
    assert!(
        matches!(error, GatewayError::PositionNotFound { ref id } if id == "222"),
        "got {error:?}"
    );
    assert!(
        paths_hit(&server)
            .await
            .contains(&format!("GET {ACCOUNT}/trades/222")),
        "the trade endpoint answered the lookup"
    );
}

#[tokio::test]
async fn close_position_closes_the_named_trade() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/222/close"),
        200,
        oanda_trade_close_json("901", "-10000"),
    )
    .await;
    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/111/close"),
        200,
        oanda_trade_close_json("902", "-10000"),
    )
    .await;

    let handle = adapter
        .close_position("222", &ClosePositionRequest::default())
        .await
        .unwrap();

    assert_eq!(handle.id, "901");
    assert_eq!(handle.status, OrderStatus::Filled);

    let hit = paths_hit(&server).await;
    assert!(
        !hit.contains(&format!("PUT {ACCOUNT}/trades/111/close")),
        "{hit:?}"
    );
    let body = close_body(&server, &format!("{ACCOUNT}/trades/222/close")).await;
    assert_eq!(body["units"], "ALL");
}

#[tokio::test]
async fn close_position_reduces_a_trade_by_the_requested_quantity() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/222/close"),
        200,
        oanda_trade_close_json("901", "-2500"),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: Some(dec!(2500)),
        ..ClosePositionRequest::default()
    };
    adapter.close_position("222", &request).await.unwrap();

    let body = close_body(&server, &format!("{ACCOUNT}/trades/222/close")).await;
    assert_eq!(body["units"], "2500");
}

#[tokio::test]
async fn close_position_reports_an_unknown_trade_as_missing() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "PUT",
        &format!("{ACCOUNT}/trades/222/close"),
        404,
        json!({"errorMessage": "The trade specified does not exist"}),
    )
    .await;

    let error = adapter
        .close_position("222", &ClosePositionRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, GatewayError::PositionNotFound { ref id } if id == "222"),
        "got {error:?}"
    );
    assert!(
        paths_hit(&server)
            .await
            .contains(&format!("PUT {ACCOUNT}/trades/222/close")),
        "the trade close endpoint was the one that 404'd"
    );
}
