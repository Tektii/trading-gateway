mod helpers;

use helpers::{binance_spot_account_json, binance_spot_order_json, test_spot_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{ClosePositionRequest, PositionSide};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_positions_filters_dust() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    // Account with BTC (non-dust) and a dust balance
    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        200,
        json!({
            "updateTime": 1_704_067_200_000_i64,
            "accountType": "SPOT",
            "balances": [
                { "asset": "USDT", "free": "50000.00", "locked": "0.00" },
                { "asset": "BTC", "free": "1.50000000", "locked": "0.00000000" },
                { "asset": "DOGE", "free": "0.00000001", "locked": "0.00000000" }
            ],
            "permissions": ["SPOT"]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    // USDT filtered as quote currency, DOGE filtered as dust — only BTC remains
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].symbol, "BTCUSDT");
    assert_eq!(positions[0].side, PositionSide::Long);
    assert_eq!(positions[0].quantity, dec!(1.5));
}

#[tokio::test]
async fn get_positions_filters_quote_currencies() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        200,
        binance_spot_account_json(),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    // USDT and BUSD filtered as quote currencies — only BTC and ETH remain
    assert_eq!(positions.len(), 2);
    let symbols: Vec<&str> = positions.iter().map(|p| p.symbol.as_str()).collect();
    assert!(symbols.contains(&"BTCUSDT"));
    assert!(symbols.contains(&"ETHUSDT"));
}

#[tokio::test]
async fn get_positions_with_symbol_filter() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        200,
        binance_spot_account_json(),
    )
    .await;

    let positions = adapter.get_positions(Some("BTCUSDT")).await.unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].symbol, "BTCUSDT");
}

#[tokio::test]
async fn get_positions_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        200,
        json!({
            "updateTime": 0,
            "accountType": "SPOT",
            "balances": [
                { "asset": "USDT", "free": "100.00", "locked": "0.00" }
            ],
            "permissions": ["SPOT"]
        }),
    )
    .await;

    let positions = adapter.get_positions(None).await.unwrap();
    assert!(positions.is_empty());
}

#[tokio::test]
async fn get_position_not_found() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        200,
        json!({
            "updateTime": 0,
            "accountType": "SPOT",
            "balances": [],
            "permissions": ["SPOT"]
        }),
    )
    .await;

    let err = adapter.get_position("XYZUSDT").await.unwrap_err();
    assert!(
        matches!(err, GatewayError::PositionNotFound { .. }),
        "Expected PositionNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn close_position_submits_sell_order() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_spot_adapter(&base_url);

    // close_position calls get_position (which calls get_positions via get_account)
    mount_json(
        &server,
        "GET",
        "/api/v3/account",
        200,
        binance_spot_account_json(),
    )
    .await;

    // Then submits a sell order
    mount_json(
        &server,
        "POST",
        "/api/v3/order",
        200,
        binance_spot_order_json(&json!({"side": "SELL", "type": "MARKET"})),
    )
    .await;

    let request = ClosePositionRequest {
        quantity: None,
        order_type: None,
        limit_price: None,
        cancel_associated_orders: true,
    };

    let handle = adapter.close_position("BTCUSDT", &request).await.unwrap();
    assert_eq!(
        handle.status,
        tektii_gateway_core::models::OrderStatus::Open
    );
}
