mod helpers;

use helpers::{saxo_closed_position_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::error::GatewayError;
use tektii_gateway_core::models::{Side, TradeQueryParams};
use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

#[tokio::test]
async fn get_trades_success() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/closedpositions",
        200,
        json!({
            "Data": [saxo_closed_position_json(&json!({}))]
        }),
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
    assert_eq!(trades[0].id, "TRADE-1");
    assert_eq!(trades[0].symbol, "EURUSD:FxSpot");
    assert_eq!(trades[0].side, Side::Buy);
    assert_eq!(trades[0].quantity, dec!(100000));
    assert_eq!(trades[0].price, dec!(1.105));
    assert_eq!(trades[0].commission, dec!(2.5));
}

#[tokio::test]
async fn get_trades_sell_side() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/closedpositions",
        200,
        json!({
            "Data": [saxo_closed_position_json(&json!({
                "ClosedPosition": {
                    "Uic": 21,
                    "AssetType": "FxSpot",
                    "BuySell": "Sell",
                    "Amount": 50000.0,
                    "OpenPrice": 1.1,
                    "ClosePrice": 1.095,
                    "ProfitLoss": 250.0,
                    "CostsTotal": 1.5,
                    "ExecutionTimeClose": "2024-01-15T10:30:00.000000Z"
                }
            }))]
        }),
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
    assert_eq!(trades[0].side, Side::Sell);
}

#[tokio::test]
async fn get_trades_empty() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/closedpositions",
        200,
        json!({ "Data": [] }),
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
    assert!(trades.is_empty());
}

#[tokio::test]
async fn get_trades_error() {
    let (server, base_url) = start_mock_server().await;
    let adapter = test_adapter(&server, &base_url).await;

    mount_json(
        &server,
        "GET",
        "/port/v1/closedpositions",
        500,
        json!({"Message": "Server Error"}),
    )
    .await;

    let params = TradeQueryParams {
        symbol: None,
        order_id: None,
        since: None,
        until: None,
        limit: None,
    };
    let err = adapter.get_trades(&params).await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError { .. }));
}
