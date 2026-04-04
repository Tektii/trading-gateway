//! Shared test helpers for Saxo adapter integration tests.
#![allow(dead_code)]

use rust_decimal::Decimal;
use secrecy::SecretBox;
use serde_json::{Value, json};
use tektii_gateway_core::models::{
    ClosePositionRequest, OrderRequest, OrderType, Side, TradingPlatform,
};
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_saxo::{SaxoAdapter, SaxoCredentials};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::merge_json;
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// =========================================================================
// Adapter factories
// =========================================================================

/// Create a `SaxoAdapter` pointed at a wiremock server (precheck disabled).
pub async fn test_adapter(server: &MockServer, base_url: &str) -> SaxoAdapter {
    mount_startup_mocks(server).await;
    build_adapter(base_url, false).await
}

/// Create a `SaxoAdapter` with margin precheck enabled.
pub async fn test_adapter_with_precheck(server: &MockServer, base_url: &str) -> SaxoAdapter {
    mount_startup_mocks(server).await;
    build_adapter(base_url, true).await
}

/// Mount the instrument-loading and client-key endpoints needed by `SaxoAdapter::new()`.
async fn mount_startup_mocks(server: &MockServer) {
    // The adapter loads instruments 4 times (one per asset type), each hitting
    // /ref/v1/instruments?AssetTypes=X&$top=1000&$skip=0.
    // We return ALL instruments in every response — duplicates are harmlessly ignored
    // by the instrument map (which deduplicates by symbol).
    let all_instruments = saxo_instruments_json(&[
        (21, "FxSpot", "EURUSD"),
        (101, "CfdOnIndex", "US500"),
        (201, "CfdOnStock", "AAPL"),
        (301, "CfdOnFutures", "OIL"),
    ]);

    Mock::given(method("GET"))
        .and(path("/ref/v1/instruments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&all_instruments))
        .mount(server)
        .await;

    // Client key
    Mock::given(method("GET"))
        .and(path("/port/v1/clients/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(saxo_client_json()))
        .mount(server)
        .await;
}

async fn build_adapter(base_url: &str, precheck: bool) -> SaxoAdapter {
    let credentials = SaxoCredentials {
        client_id: "test-client-id".to_string(),
        client_secret: SecretBox::new(Box::new("test-secret".to_string())),
        account_key: "test-account-key".to_string(),
        access_token: Some(SecretBox::new(Box::new("test-access-token".to_string()))),
        refresh_token: Some(SecretBox::new(Box::new("test-refresh-token".to_string()))),
        rest_url: Some(base_url.to_string()),
        auth_url: Some(base_url.to_string()),
        ws_url: None,
        precheck_orders: Some(precheck),
    };
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    SaxoAdapter::new(&credentials, broadcaster, TradingPlatform::SaxoSim)
        .await
        .expect("Failed to create test SaxoAdapter")
}

// =========================================================================
// Saxo JSON response builders (all PascalCase)
// =========================================================================

/// Instrument page response for `GET /ref/v1/instruments`.
pub fn saxo_instruments_json(entries: &[(u32, &str, &str)]) -> Value {
    json!({
        "Data": entries.iter().map(|(uic, asset_type, symbol)| json!({
            "Uic": uic,
            "AssetType": asset_type,
            "Description": format!("Test {symbol}"),
            "Symbol": symbol,
            "CurrencyCode": "USD",
            "PriceDecimals": 5
        })).collect::<Vec<_>>()
    })
}

/// Client key response for `GET /port/v1/clients/me`.
pub fn saxo_client_json() -> Value {
    json!({
        "ClientKey": "test-client-key",
        "DefaultAccountKey": "test-account-key"
    })
}

/// Account balance response for `GET /port/v1/balances/me`.
pub fn saxo_balance_json() -> Value {
    json!({
        "TotalValue": 100_500.0,
        "CashBalance": 100_000.0,
        "MarginUsedByCurrentPositions": 5000.0,
        "MarginAvailable": 95000.0,
        "UnrealizedPositionsValue": 500.0,
        "Currency": "USD"
    })
}

/// Single Saxo order with optional overrides.
pub fn saxo_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "OrderId": "ORD-123",
        "Uic": 21,
        "AssetType": "FxSpot",
        "BuySell": "Buy",
        "OrderType": "Limit",
        "Amount": 100_000.0,
        "FilledAmount": 0.0,
        "Price": 1.1,
        "Status": "Working",
        "Duration": { "DurationType": "GoodTillCancel" }
    });
    merge_json(&mut base, overrides);
    base
}

/// Order creation response for `POST /trade/v2/orders`.
pub fn saxo_order_response_json(order_id: &str) -> Value {
    json!({ "OrderId": order_id })
}

/// Net position with optional overrides.
pub fn saxo_position_json(overrides: &Value) -> Value {
    let mut base = json!({
        "NetPositionId": "POS-1",
        "NetPositionBase": {
            "Uic": 21,
            "AssetType": "FxSpot",
            "Amount": 100_000.0,
            "AverageOpenPrice": 1.1,
            "MarketValue": 100_500.0,
            "ProfitLossOnTrade": 500.0,
            "CurrentPrice": 1.105
        }
    });
    merge_json(&mut base, overrides);
    base
}

/// InfoPrice (quote) response for `GET /trade/v1/infoprices`.
pub fn saxo_quote_json() -> Value {
    json!({
        "Uic": 21,
        "AssetType": "FxSpot",
        "Quote": {
            "Bid": 1.099,
            "Ask": 1.101,
            "Mid": 1.1
        }
    })
}

/// Chart data response for `GET /chart/v3/charts`.
pub fn saxo_chart_json() -> Value {
    json!({
        "Data": [{
            "Time": "2024-01-15T10:00:00.000000Z",
            "OpenBid": 1.0995,
            "OpenAsk": 1.1005,
            "HighBid": 1.1045,
            "HighAsk": 1.1055,
            "LowBid": 1.0945,
            "LowAsk": 1.0955,
            "CloseBid": 1.1015,
            "CloseAsk": 1.1025,
            "Volume": 1000.0
        }]
    })
}

/// Closed position (trade) with optional overrides.
pub fn saxo_closed_position_json(overrides: &Value) -> Value {
    let mut base = json!({
        "ClosedPositionId": "TRADE-1",
        "ClosedPosition": {
            "Uic": 21,
            "AssetType": "FxSpot",
            "BuySell": "Buy",
            "Amount": 100_000.0,
            "OpenPrice": 1.1,
            "ClosePrice": 1.105,
            "ProfitLoss": 500.0,
            "CostsTotal": 2.5,
            "ExecutionTimeClose": "2024-01-15T10:30:00.000000Z"
        }
    });
    merge_json(&mut base, overrides);
    base
}

/// Precheck success response.
pub fn saxo_precheck_ok_json() -> Value {
    json!({
        "PreCheckResult": "Ok",
        "EstimatedCashRequired": 2500.0,
        "MarginUtilizationPct": 45.0
    })
}

/// Precheck failure response.
pub fn saxo_precheck_error_json(error_code: &str, message: &str) -> Value {
    json!({
        "PreCheckResult": "Error",
        "ErrorInfo": {
            "ErrorCode": error_code,
            "Message": message
        }
    })
}

// =========================================================================
// Order request helper
// =========================================================================

/// Build an `OrderRequest` with sensible defaults for Saxo forex testing.
pub fn forex_order(symbol: &str, side: Side, order_type: OrderType, qty: Decimal) -> OrderRequest {
    OrderRequest {
        symbol: symbol.to_string(),
        side,
        quantity: qty,
        order_type,
        ..test_order_request()
    }
}

/// Build a default `ClosePositionRequest`.
pub fn close_request() -> ClosePositionRequest {
    ClosePositionRequest {
        order_type: None,
        limit_price: None,
        quantity: None,
        cancel_associated_orders: true,
    }
}
