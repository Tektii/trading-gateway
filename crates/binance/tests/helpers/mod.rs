//! Shared test helpers for Binance adapter integration tests.
#![allow(dead_code)]

use serde_json::{Value, json};
use tektii_gateway_binance::{
    BinanceCoinFuturesAdapter, BinanceCredentials, BinanceFuturesAdapter, BinanceMarginAdapter,
    BinanceSpotAdapter,
};
use tektii_gateway_core::http::RetryConfig;
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_test_support::wiremock_helpers::merge_json;
use tokio::sync::broadcast;

// =========================================================================
// Adapter factories
// =========================================================================

/// Create a `BinanceSpotAdapter` pointed at a wiremock server.
pub fn test_spot_adapter(base_url: &str) -> BinanceSpotAdapter {
    let credentials = BinanceCredentials::new("test-key", "test-secret").with_base_url(base_url);
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    BinanceSpotAdapter::new(
        &credentials,
        broadcaster,
        TradingPlatform::BinanceSpotTestnet,
    )
    .expect("Failed to build HTTP client")
    .with_retry_config(RetryConfig::for_tests())
}

/// Create a `BinanceFuturesAdapter` pointed at a wiremock server.
pub fn test_futures_adapter(base_url: &str) -> BinanceFuturesAdapter {
    let credentials = BinanceCredentials::new("test-key", "test-secret").with_base_url(base_url);
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    BinanceFuturesAdapter::new(
        &credentials,
        broadcaster,
        TradingPlatform::BinanceFuturesTestnet,
    )
    .expect("Failed to build HTTP client")
    .with_retry_config(RetryConfig::for_tests())
}

/// Create a `BinanceMarginAdapter` pointed at a wiremock server.
pub fn test_margin_adapter(base_url: &str) -> BinanceMarginAdapter {
    let credentials = BinanceCredentials::new("test-key", "test-secret").with_base_url(base_url);
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    BinanceMarginAdapter::new(
        &credentials,
        broadcaster,
        TradingPlatform::BinanceMarginTestnet,
    )
    .expect("Failed to build HTTP client")
    .with_retry_config(RetryConfig::for_tests())
}

/// Create a `BinanceCoinFuturesAdapter` pointed at a wiremock server.
pub fn test_coin_futures_adapter(base_url: &str) -> BinanceCoinFuturesAdapter {
    let credentials = BinanceCredentials::new("test-key", "test-secret").with_base_url(base_url);
    let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
    BinanceCoinFuturesAdapter::new(
        &credentials,
        broadcaster,
        TradingPlatform::BinanceCoinFuturesTestnet,
    )
    .expect("Failed to build HTTP client")
    .with_retry_config(RetryConfig::for_tests())
}

// =========================================================================
// Spot JSON response builders
// =========================================================================

/// Binance Spot account response (camelCase).
pub fn binance_spot_account_json() -> Value {
    json!({
        "updateTime": 1_704_067_200_000_i64,
        "accountType": "SPOT",
        "balances": [
            { "asset": "USDT", "free": "50000.00", "locked": "5000.00" },
            { "asset": "BTC", "free": "1.50000000", "locked": "0.00000000" },
            { "asset": "ETH", "free": "10.00000000", "locked": "0.00000000" },
            { "asset": "BUSD", "free": "1000.00", "locked": "0.00" }
        ],
        "permissions": ["SPOT"]
    })
}

/// Binance Spot order response with optional overrides.
pub fn binance_spot_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "orderId": 123_456,
        "symbol": "BTCUSDT",
        "status": "NEW",
        "clientOrderId": "client-001",
        "price": "0.00000000",
        "origQty": "1.00000000",
        "executedQty": "0.00000000",
        "cummulativeQuoteQty": "0.00000000",
        "type": "MARKET",
        "side": "BUY",
        "timeInForce": "GTC",
        "time": 1_704_067_200_000_i64,
        "updateTime": 1_704_067_200_000_i64,
        "isWorking": true,
        "stopPrice": null
    });
    merge_json(&mut base, overrides);
    base
}

/// Binance Spot trade response.
pub fn binance_spot_trade_json(symbol: &str) -> Value {
    json!({
        "id": 10001,
        "orderId": 123_456,
        "symbol": symbol,
        "price": "42000.00",
        "qty": "1.00000000",
        "quoteQty": "42000.00",
        "commission": "0.00100000",
        "commissionAsset": "BTC",
        "time": 1_704_067_200_000_i64,
        "isBuyer": true,
        "isMaker": false,
        "isBestMatch": true
    })
}

/// Binance Spot book ticker (quote) response.
pub fn binance_spot_quote_json() -> Value {
    json!({
        "symbol": "BTCUSDT",
        "bidPrice": "42000.00",
        "bidQty": "1.50000000",
        "askPrice": "42100.00",
        "askQty": "2.00000000"
    })
}

/// Binance Spot kline (array-of-arrays) response.
pub fn binance_spot_kline_json() -> Value {
    json!([[
        1_704_067_200_000_i64,
        "42000.00",
        "42500.00",
        "41800.00",
        "42200.00",
        "150.50000000",
        1_704_070_800_000_i64,
        "6331500.00",
        1200,
        "80.25000000",
        "3375000.00",
        "0"
    ]])
}

/// Binance OCO order response.
pub fn binance_oco_response_json() -> Value {
    json!({
        "orderListId": 999,
        "contingencyType": "OCO",
        "listStatusType": "EXEC_STARTED",
        "listOrderStatus": "EXECUTING",
        "listClientOrderId": "oco-client-001",
        "transactionTime": 1_704_067_200_000_i64,
        "symbol": "BTCUSDT",
        "orders": [
            { "symbol": "BTCUSDT", "orderId": 200_001, "clientOrderId": "sl-client-001" },
            { "symbol": "BTCUSDT", "orderId": 200_002, "clientOrderId": "tp-client-001" }
        ],
        "orderReports": [
            {
                "symbol": "BTCUSDT",
                "orderId": 200_001,
                "orderListId": 999,
                "clientOrderId": "sl-client-001",
                "transactTime": 1_704_067_200_000_i64,
                "price": "0.00000000",
                "origQty": "1.00000000",
                "executedQty": "0.00000000",
                "cummulativeQuoteQty": "0.00000000",
                "status": "NEW",
                "timeInForce": "GTC",
                "type": "STOP_LOSS_LIMIT",
                "side": "SELL",
                "stopPrice": "40000.00000000"
            },
            {
                "symbol": "BTCUSDT",
                "orderId": 200_002,
                "orderListId": 999,
                "clientOrderId": "tp-client-001",
                "transactTime": 1_704_067_200_000_i64,
                "price": "45000.00000000",
                "origQty": "1.00000000",
                "executedQty": "0.00000000",
                "cummulativeQuoteQty": "0.00000000",
                "status": "NEW",
                "timeInForce": "GTC",
                "type": "LIMIT_MAKER",
                "side": "SELL",
                "stopPrice": null
            }
        ]
    })
}

/// Binance cancel-replace response with optional overrides.
pub fn binance_cancel_replace_json(overrides: &Value) -> Value {
    let mut base = json!({
        "cancelResult": "SUCCESS",
        "newOrderResult": "SUCCESS",
        "cancelResponse": {
            "orderId": 123_456,
            "symbol": "BTCUSDT",
            "status": "CANCELED",
            "clientOrderId": "client-001",
            "price": "41000.00000000",
            "origQty": "1.00000000",
            "executedQty": "0.00000000",
            "cummulativeQuoteQty": "0.00000000",
            "type": "LIMIT",
            "side": "BUY",
            "timeInForce": "GTC",
            "time": 1_704_067_200_000_i64,
            "updateTime": 1_704_067_200_000_i64,
            "isWorking": false,
            "stopPrice": null
        },
        "newOrderResponse": {
            "orderId": 123_457,
            "symbol": "BTCUSDT",
            "status": "NEW",
            "clientOrderId": "client-002",
            "price": "41500.00000000",
            "origQty": "1.00000000",
            "executedQty": "0.00000000",
            "cummulativeQuoteQty": "0.00000000",
            "type": "LIMIT",
            "side": "BUY",
            "timeInForce": "GTC",
            "time": 1_704_067_260_000_i64,
            "updateTime": 1_704_067_260_000_i64,
            "isWorking": true,
            "stopPrice": null
        }
    });
    merge_json(&mut base, overrides);
    base
}

// =========================================================================
// Futures JSON response builders
// =========================================================================

/// Binance Futures account response.
pub fn binance_futures_account_json() -> Value {
    json!({
        "totalWalletBalance": "100000.00",
        "totalUnrealizedProfit": "500.00",
        "totalMarginBalance": "100500.00",
        "availableBalance": "90000.00",
        "maxWithdrawAmount": "90000.00",
        "canTrade": true,
        "canDeposit": true,
        "canWithdraw": true,
        "updateTime": 1_704_067_200_000_i64,
        "assets": [
            {
                "asset": "USDT",
                "walletBalance": "100000.00",
                "unrealizedProfit": "500.00",
                "marginBalance": "100500.00",
                "availableBalance": "90000.00"
            }
        ],
        "positions": [
            {
                "symbol": "BTCUSDT",
                "positionSide": "LONG",
                "positionAmt": "0.000",
                "entryPrice": "0.0",
                "unrealizedProfit": "0.00"
            }
        ]
    })
}

/// Binance Futures order response with optional overrides.
pub fn binance_futures_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "orderId": 456_789,
        "symbol": "BTCUSDT",
        "status": "NEW",
        "clientOrderId": "futures-client-001",
        "price": "0.00",
        "avgPrice": "0.00",
        "origQty": "1.000",
        "executedQty": "0.000",
        "cumQuote": "0.00",
        "type": "MARKET",
        "side": "BUY",
        "timeInForce": "GTC",
        "positionSide": "LONG",
        "reduceOnly": false,
        "stopPrice": null,
        "time": 1_704_067_200_000_i64,
        "updateTime": 1_704_067_200_000_i64
    });
    merge_json(&mut base, overrides);
    base
}

/// Binance Futures position risk response.
pub fn binance_futures_position_json(symbol: &str) -> Value {
    json!({
        "symbol": symbol,
        "positionAmt": "1.000",
        "entryPrice": "42000.00",
        "markPrice": "42500.00",
        "unRealizedProfit": "500.00",
        "liquidationPrice": "35000.00",
        "leverage": "10",
        "positionSide": "LONG",
        "marginType": "cross",
        "isolatedMargin": "0.00",
        "notional": "42500.00",
        "updateTime": 1_704_067_200_000_i64
    })
}

/// Binance Futures trade response.
pub fn binance_futures_trade_json(symbol: &str) -> Value {
    json!({
        "id": 20001,
        "orderId": 456_789,
        "symbol": symbol,
        "price": "42000.00",
        "qty": "1.000",
        "quoteQty": "42000.00",
        "commission": "16.80",
        "commissionAsset": "USDT",
        "time": 1_704_067_200_000_i64,
        "side": "BUY",
        "positionSide": "LONG",
        "maker": false,
        "buyer": true
    })
}

/// Binance Futures book ticker (quote) response.
pub fn binance_futures_quote_json() -> Value {
    json!({
        "symbol": "BTCUSDT",
        "bidPrice": "42000.00",
        "bidQty": "5.000",
        "askPrice": "42100.00",
        "askQty": "3.000",
        "time": 1_704_067_200_000_i64
    })
}

/// Binance Futures kline (array-of-arrays) response.
pub fn binance_futures_kline_json() -> Value {
    json!([[
        1_704_067_200_000_i64,
        "42000.00",
        "42500.00",
        "41800.00",
        "42200.00",
        "250.000",
        1_704_070_800_000_i64,
        "10525000.00",
        2000,
        "130.000",
        "5460000.00",
        "0"
    ]])
}

// =========================================================================
// Margin JSON response builders
// =========================================================================

/// Binance Margin account response.
pub fn binance_margin_account_json() -> Value {
    json!({
        "borrowEnabled": true,
        "marginLevel": "5.00000000",
        "totalAssetOfBtc": "2.50000000",
        "totalLiabilityOfBtc": "0.50000000",
        "totalNetAssetOfBtc": "2.00000000",
        "tradeEnabled": true,
        "transferEnabled": true,
        "userAssets": [
            {
                "asset": "USDT",
                "borrowed": "0.00",
                "free": "50000.00",
                "interest": "0.00",
                "locked": "5000.00",
                "netAsset": "45000.00"
            },
            {
                "asset": "BTC",
                "borrowed": "0.50000000",
                "free": "2.00000000",
                "interest": "0.00010000",
                "locked": "0.00000000",
                "netAsset": "1.49990000"
            }
        ]
    })
}

/// Binance Margin order response with optional overrides.
pub fn binance_margin_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "orderId": 789_012,
        "symbol": "BTCUSDT",
        "status": "NEW",
        "clientOrderId": "margin-client-001",
        "price": "0.00000000",
        "origQty": "1.00000000",
        "executedQty": "0.00000000",
        "cummulativeQuoteQty": "0.00000000",
        "type": "MARKET",
        "side": "BUY",
        "timeInForce": "GTC",
        "time": 1_704_067_200_000_i64,
        "updateTime": 1_704_067_200_000_i64,
        "isWorking": true,
        "isIsolated": false,
        "stopPrice": null
    });
    merge_json(&mut base, overrides);
    base
}

/// Binance Margin trade response.
pub fn binance_margin_trade_json(symbol: &str) -> Value {
    json!({
        "id": 30001,
        "orderId": 789_012,
        "symbol": symbol,
        "price": "42000.00",
        "qty": "1.00000000",
        "quoteQty": "42000.00",
        "commission": "0.00100000",
        "commissionAsset": "BTC",
        "time": 1_704_067_200_000_i64,
        "isBuyer": true,
        "isMaker": false,
        "isBestMatch": true,
        "isIsolated": false
    })
}

// =========================================================================
// Coin-M Futures JSON response builders
// =========================================================================

/// Binance Coin-M Futures account response.
pub fn binance_coin_account_json() -> Value {
    json!({
        "totalWalletBalance": "5.00000000",
        "totalUnrealizedProfit": "0.10000000",
        "totalMarginBalance": "5.10000000",
        "availableBalance": "4.50000000",
        "maxWithdrawAmount": "4.50000000",
        "canTrade": true,
        "canDeposit": true,
        "canWithdraw": true,
        "updateTime": 1_704_067_200_000_i64,
        "assets": [
            {
                "asset": "BTC",
                "walletBalance": "5.00000000",
                "unrealizedProfit": "0.10000000",
                "marginBalance": "5.10000000",
                "availableBalance": "4.50000000"
            }
        ],
        "positions": [
            {
                "symbol": "BTCUSD_PERP",
                "positionSide": "LONG",
                "positionAmt": "0",
                "entryPrice": "0.0",
                "unrealizedProfit": "0.00"
            }
        ]
    })
}

/// Binance Coin-M Futures order response with optional overrides.
pub fn binance_coin_order_json(overrides: &Value) -> Value {
    let mut base = json!({
        "orderId": 345_678,
        "symbol": "BTCUSD_PERP",
        "status": "NEW",
        "clientOrderId": "coin-client-001",
        "price": "0",
        "avgPrice": "0",
        "origQty": "1",
        "executedQty": "0",
        "cumQuote": "0",
        "type": "MARKET",
        "side": "BUY",
        "timeInForce": "GTC",
        "positionSide": "LONG",
        "reduceOnly": false,
        "stopPrice": null,
        "time": 1_704_067_200_000_i64,
        "updateTime": 1_704_067_200_000_i64
    });
    merge_json(&mut base, overrides);
    base
}

/// Binance Coin-M Futures position risk response.
pub fn binance_coin_position_json(symbol: &str) -> Value {
    json!({
        "symbol": symbol,
        "positionAmt": "1",
        "entryPrice": "42000.00",
        "markPrice": "42500.00",
        "unRealizedProfit": "0.00028011",
        "liquidationPrice": "35000.00",
        "leverage": "10",
        "positionSide": "LONG",
        "marginType": "cross",
        "isolatedMargin": "0.00000000",
        "notional": "42500.00",
        "updateTime": 1_704_067_200_000_i64
    })
}

/// Binance Coin-M Futures trade response.
pub fn binance_coin_trade_json(symbol: &str) -> Value {
    json!({
        "id": 40001,
        "orderId": 345_678,
        "symbol": symbol,
        "price": "42000.00",
        "qty": "1",
        "quoteQty": "42000.00",
        "commission": "0.00004000",
        "commissionAsset": "BTC",
        "time": 1_704_067_200_000_i64,
        "side": "BUY",
        "positionSide": "LONG",
        "maker": false,
        "buyer": true
    })
}

/// Binance Coin-M Futures book ticker (quote) response.
pub fn binance_coin_quote_json() -> Value {
    json!({
        "symbol": "BTCUSD_PERP",
        "bidPrice": "42000.00",
        "bidQty": "100",
        "askPrice": "42100.00",
        "askQty": "80",
        "time": 1_704_067_200_000_i64
    })
}
