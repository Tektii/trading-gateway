mod helpers;

use helpers::{alpaca_activity_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{Side, TradeQueryParams};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_trades_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account/activities/FILL",
        200,
        json!([alpaca_activity_json("AAPL")]),
    )
    .await;

    let params = TradeQueryParams {
        symbol: None,
        order_id: None,
        since: None,
        until: None,
        limit: None,
    };

    let trades = adapter.get_trades(&params).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].id, "activity-001");
    assert_eq!(trades[0].order_id, "order-abc-123");
    assert_eq!(trades[0].symbol, "AAPL");
    assert_eq!(trades[0].side, Side::Buy);
    assert_eq!(trades[0].quantity, dec!(10));
    assert_eq!(trades[0].price, dec!(150.50));
    assert_eq!(trades[0].commission, dec!(0));
}

#[tokio::test]
async fn get_trades_with_symbol_filter() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account/activities/FILL",
        200,
        json!([alpaca_activity_json("AAPL"), alpaca_activity_json("MSFT"),]),
    )
    .await;

    let params = TradeQueryParams {
        symbol: Some("AAPL".to_string()),
        order_id: None,
        since: None,
        until: None,
        limit: None,
    };

    let trades = adapter.get_trades(&params).await.unwrap();
    // Client-side filter: only AAPL returned
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].symbol, "AAPL");
}

#[tokio::test]
async fn get_trades_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account/activities/FILL",
        200,
        json!([]),
    )
    .await;

    let params = TradeQueryParams::default();
    let trades = adapter.get_trades(&params).await.unwrap();
    assert!(trades.is_empty());
}

#[tokio::test]
async fn get_trades_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/v2/account/activities/FILL",
        500,
        json!({"message": "Internal error"}),
    )
    .await;

    let params = TradeQueryParams::default();
    let err = adapter.get_trades(&params).await.unwrap_err();
    assert!(
        matches!(err, GatewayError::ProviderError { .. }),
        "Expected ProviderError, got: {err:?}"
    );
}
