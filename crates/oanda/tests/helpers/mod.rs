//! Shared test helpers for Oanda adapter integration tests.
#![allow(dead_code)]

use serde_json::{Value, json};
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_oanda::{OandaAdapter, OandaCredentials};
use tektii_gateway_test_support::wiremock_helpers::merge_json;
use tokio::sync::broadcast;

/// Create an `OandaAdapter` pointed at a wiremock server.
pub fn test_adapter(base_url: &str) -> OandaAdapter {
    let credentials =
        OandaCredentials::new("test-token", "test-account-123").with_rest_url(base_url);
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    OandaAdapter::new(&credentials, broadcaster, TradingPlatform::OandaPractice)
        .expect("Failed to build HTTP client")
}

// =========================================================================
// Oanda JSON response builders
// =========================================================================

/// Oanda account summary response.
pub fn oanda_account_json() -> Value {
    json!({
        "account": {
            "balance": "100000.00",
            "NAV": "100500.00",
            "marginUsed": "5000.00",
            "marginAvailable": "95000.00",
            "unrealizedPL": "500.00",
            "hedgingEnabled": false,
            "currency": "USD"
        }
    })
}

/// Oanda order response with optional overrides.
pub fn oanda_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "id": "123",
        "type": "LIMIT",
        "instrument": "EUR_USD",
        "units": "10000",
        "state": "PENDING",
        "price": "1.10000",
        "timeInForce": "GTC",
        "createTime": "2024-01-15T10:30:00.000000000Z"
    });
    merge_json(&mut base, overrides);
    base
}

/// Oanda market order fill response (orderFillTransaction present).
pub fn oanda_market_fill_json(overrides: &Value) -> Value {
    let mut base = json!({
        "orderFillTransaction": {
            "id": "456",
            "type": "ORDER_FILL",
            "instrument": "EUR_USD",
            "units": "10000",
            "price": "1.10000",
            "time": "2024-01-15T10:30:00.000000000Z"
        }
    });
    if let (Some(obj), Some(fill)) = (overrides.as_object(), base.get_mut("orderFillTransaction")) {
        for (k, v) in obj {
            fill[k] = v.clone();
        }
    }
    base
}

/// Oanda pending order creation response (orderCreateTransaction present).
pub fn oanda_pending_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "orderCreateTransaction": {
            "id": "789",
            "type": "LIMIT_ORDER",
            "instrument": "EUR_USD",
            "units": "10000",
            "time": "2024-01-15T10:30:00.000000000Z"
        }
    });
    if let (Some(obj), Some(create)) = (
        overrides.as_object(),
        base.get_mut("orderCreateTransaction"),
    ) {
        for (k, v) in obj {
            create[k] = v.clone();
        }
    }
    base
}

/// Oanda order rejection response.
pub fn oanda_reject_json(reason: &str) -> Value {
    json!({
        "orderRejectTransaction": {
            "id": "999",
            "type": "ORDER_REJECT",
            "rejectReason": reason,
            "time": "2024-01-15T10:30:00.000000000Z"
        }
    })
}

/// Oanda position with both long and short sides.
pub fn oanda_position_json(instrument: &str, long_units: &str, short_units: &str) -> Value {
    json!({
        "instrument": instrument,
        "long": {
            "units": long_units,
            "averagePrice": "1.10000",
            "unrealizedPL": "50.00"
        },
        "short": {
            "units": short_units,
            "averagePrice": "1.09000",
            "unrealizedPL": "-20.00"
        }
    })
}

/// Oanda trade response.
pub fn oanda_trade_json(instrument: &str, units: &str) -> Value {
    json!({
        "id": "1",
        "instrument": instrument,
        "currentUnits": units,
        "price": "1.10000",
        "openTime": "2024-01-15T10:30:00.000000000Z",
        "state": "OPEN",
        "unrealizedPL": "50.00"
    })
}

/// Oanda pricing (quote) response.
pub fn oanda_pricing_json(instrument: &str) -> Value {
    json!({
        "prices": [{
            "instrument": instrument,
            "bids": [{"price": "1.09900", "liquidity": 1_000_000}],
            "asks": [{"price": "1.10100", "liquidity": 2_000_000}],
            "time": "2024-01-15T10:30:00.000000000Z"
        }]
    })
}

/// Oanda candles (bars) response.
pub fn oanda_candles_json() -> Value {
    json!({
        "candles": [{
            "time": "2024-01-15T10:00:00.000000000Z",
            "mid": {
                "o": "1.10000",
                "h": "1.10500",
                "l": "1.09500",
                "c": "1.10200"
            },
            "volume": 1000,
            "complete": true
        }]
    })
}
