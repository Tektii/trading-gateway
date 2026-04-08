//! Binance Futures adapter for the trading gateway (USDS-M).

use super::capabilities::BinanceFuturesCapabilities;
use super::types::{
    BINANCE_FUTURES_BASE_URL, BINANCE_FUTURES_TESTNET_URL, BinanceFuturesAccount,
    BinanceFuturesKline, BinanceFuturesOrder, BinanceFuturesPosition, BinanceFuturesQuote,
    BinanceFuturesTrade,
};
use crate::common::auth::current_timestamp_ms;
use crate::common::error::binance_error_mapper;

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Client;
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretBox};
use sha2::Sha256;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};

use tektii_gateway_core::adapter::{ProviderCapabilities, TradingAdapter};
use tektii_gateway_core::circuit_breaker::{
    AdapterCircuitBreaker, CircuitBreakerSnapshot, is_outage_error,
};
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::{ExitHandler, ExitHandling};
use tektii_gateway_core::http::{RetryConfig, execute_with_retry};
use tektii_gateway_core::models::{
    Account, Bar, BarParams, CancelOrderResult, Capabilities, ClosePositionRequest,
    ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams,
    OrderRequest, OrderType, Position, PositionSide, Quote, Side, TimeInForce, Trade,
    TradeQueryParams, TradingPlatform,
};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::WsMessage;

use crate::credentials::BinanceCredentials;

/// Binance Futures adapter implementation (USDS-M).
pub struct BinanceFuturesAdapter {
    client: Client,
    base_url: String,
    api_key: Arc<SecretBox<String>>,
    api_secret: Arc<SecretBox<String>>,
    retry_config: RetryConfig,

    state_manager: Arc<StateManager>,
    exit_handler: Arc<ExitHandler>,
    event_router: Arc<EventRouter>,
    platform: TradingPlatform,
    circuit_breaker: Arc<RwLock<AdapterCircuitBreaker>>,
}

impl BinanceFuturesAdapter {
    /// Create a new Binance Futures adapter.
    ///
    /// # Errors
    ///
    /// Returns `reqwest::Error` if the HTTP client fails to build.
    pub fn new(
        credentials: &BinanceCredentials,
        broadcaster: broadcast::Sender<WsMessage>,
        platform: TradingPlatform,
    ) -> Result<Self, reqwest::Error> {
        let base_url = credentials
            .base_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::BinanceFuturesTestnet => BINANCE_FUTURES_TESTNET_URL.to_string(),
                _ => BINANCE_FUTURES_BASE_URL.to_string(),
            });

        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        let state_manager = Arc::new(StateManager::new());
        let exit_handler = Arc::new(ExitHandler::with_defaults(
            Arc::clone(&state_manager),
            platform,
        ));
        let event_router = Arc::new(EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler) as Arc<dyn ExitHandling>,
            broadcaster,
            platform,
        ));
        let circuit_breaker = Arc::new(RwLock::new(AdapterCircuitBreaker::new(
            3,
            Duration::from_secs(300),
            "binance-futures",
        )));

        Ok(Self {
            client,
            base_url,
            api_key: tektii_gateway_core::arc_secret(&credentials.api_key),
            api_secret: tektii_gateway_core::arc_secret(&credentials.api_secret),
            retry_config: RetryConfig::default(),
            state_manager,
            exit_handler,
            event_router,
            platform,
            circuit_breaker,
        })
    }

    /// Override the retry configuration (useful for tests).
    #[must_use]
    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    #[must_use]
    pub fn state_manager(&self) -> Arc<StateManager> {
        Arc::clone(&self.state_manager)
    }

    #[must_use]
    pub fn exit_handler(&self) -> Arc<ExitHandler> {
        Arc::clone(&self.exit_handler)
    }

    #[must_use]
    pub fn event_router(&self) -> Arc<EventRouter> {
        Arc::clone(&self.event_router)
    }

    async fn check_circuit_breaker(&self) -> GatewayResult<()> {
        let breaker = self.circuit_breaker.read().await;
        if breaker.is_open() {
            return Err(breaker.open_error());
        }
        Ok(())
    }

    async fn record_if_outage(&self, error: &GatewayError) {
        if is_outage_error(error) {
            let mut breaker = self.circuit_breaker.write().await;
            breaker.record_failure();
        }
    }

    fn sign_query_static(api_secret: &str, query: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac =
            // HMAC-SHA256 accepts any key length — new_from_slice is infallible here.
            HmacSha256::new_from_slice(api_secret.as_bytes()).expect("HMAC-SHA256 accepts any key length");
        mac.update(query.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    async fn send_signed_get<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        base_params: &str,
    ) -> GatewayResult<T> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let endpoint = endpoint.to_string();
        let base_params = base_params.to_string();

        let response = execute_with_retry(
            || {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = Arc::clone(&api_key);
                let api_secret = Arc::clone(&api_secret);
                let endpoint = endpoint.clone();
                let base_params = base_params.clone();
                async move {
                    let timestamp = current_timestamp_ms();
                    let query = if base_params.is_empty() {
                        format!("timestamp={timestamp}&recvWindow=5000")
                    } else {
                        format!("{base_params}&timestamp={timestamp}&recvWindow=5000")
                    };
                    let signature = Self::sign_query_static(api_secret.expose_secret(), &query);
                    let url = format!("{base_url}/{endpoint}?{query}&signature={signature}");
                    client
                        .get(&url)
                        .header("X-MBX-APIKEY", api_key.expose_secret().as_str())
                }
            },
            "binance-futures",
            Some(&self.retry_config),
            Some(binance_error_mapper),
        )
        .await?;

        response.json().await.map_err(|e| {
            GatewayError::internal(format!("Failed to parse Binance Futures response: {e}"))
        })
    }

    async fn send_signed_post<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        base_params: &str,
    ) -> GatewayResult<T> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let endpoint = endpoint.to_string();
        let base_params = base_params.to_string();

        let response = execute_with_retry(
            || {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = Arc::clone(&api_key);
                let api_secret = Arc::clone(&api_secret);
                let endpoint = endpoint.clone();
                let base_params = base_params.clone();
                async move {
                    let timestamp = current_timestamp_ms();
                    let query = if base_params.is_empty() {
                        format!("timestamp={timestamp}&recvWindow=5000")
                    } else {
                        format!("{base_params}&timestamp={timestamp}&recvWindow=5000")
                    };
                    let signature = Self::sign_query_static(api_secret.expose_secret(), &query);
                    let url = format!("{base_url}/{endpoint}?{query}&signature={signature}");
                    client
                        .post(&url)
                        .header("X-MBX-APIKEY", api_key.expose_secret().as_str())
                }
            },
            "binance-futures",
            Some(&self.retry_config),
            Some(binance_error_mapper),
        )
        .await?;

        let body = response.text().await.map_err(|e| {
            GatewayError::internal(format!("Failed to read Binance Futures response: {e}"))
        })?;
        serde_json::from_str(&body).map_err(|e| {
            GatewayError::internal(format!("Failed to parse Binance Futures response: {e}"))
        })
    }

    async fn send_signed_delete(&self, endpoint: &str, base_params: &str) -> GatewayResult<()> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_secret = Arc::clone(&self.api_secret);
        let endpoint = endpoint.to_string();
        let base_params = base_params.to_string();

        execute_with_retry(
            || {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = Arc::clone(&api_key);
                let api_secret = Arc::clone(&api_secret);
                let endpoint = endpoint.clone();
                let base_params = base_params.clone();
                async move {
                    let timestamp = current_timestamp_ms();
                    let query = if base_params.is_empty() {
                        format!("timestamp={timestamp}&recvWindow=5000")
                    } else {
                        format!("{base_params}&timestamp={timestamp}&recvWindow=5000")
                    };
                    let signature = Self::sign_query_static(api_secret.expose_secret(), &query);
                    let url = format!("{base_url}/{endpoint}?{query}&signature={signature}");
                    client
                        .delete(&url)
                        .header("X-MBX-APIKEY", api_key.expose_secret().as_str())
                }
            },
            "binance-futures",
            Some(&self.retry_config),
            Some(binance_error_mapper),
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl TradingAdapter for BinanceFuturesAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &BinanceFuturesCapabilities
    }
    fn platform(&self) -> TradingPlatform {
        self.platform
    }
    fn provider_name(&self) -> &'static str {
        "Binance Futures"
    }

    async fn get_account(&self) -> GatewayResult<Account> {
        self.check_circuit_breaker().await?;
        let result: GatewayResult<BinanceFuturesAccount> =
            self.send_signed_get("fapi/v2/account", "").await;
        if let Err(ref e) = result {
            self.record_if_outage(e).await;
        }
        let acct = result?;

        let balance = Decimal::from_str(&acct.available_balance).unwrap_or_default();
        let equity = Decimal::from_str(&acct.total_margin_balance).unwrap_or_default();
        let unrealized = Decimal::from_str(&acct.total_unrealized_profit).unwrap_or_default();

        Ok(Account {
            balance,
            equity,
            margin_used: equity - balance,
            margin_available: balance,
            unrealized_pnl: unrealized,
            currency: "USDT".to_string(),
        })
    }

    async fn submit_order(&self, order: &OrderRequest) -> GatewayResult<OrderHandle> {
        self.check_circuit_breaker().await?;
        let oco_group_id = order.oco_group_id.clone();
        let side = match order.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let otype = match order.order_type {
            OrderType::Market => "MARKET",
            OrderType::Limit => "LIMIT",
            OrderType::Stop => "STOP_MARKET",
            OrderType::StopLimit => "STOP",
            OrderType::TrailingStop => "TRAILING_STOP_MARKET",
        };
        let mut qp = vec![
            format!("symbol={}", order.symbol),
            format!("side={side}"),
            format!("type={otype}"),
            format!("quantity={}", order.quantity),
        ];
        // Trailing stop parameters
        if otype == "TRAILING_STOP_MARKET" {
            if let Some(ref trailing_type) = order.trailing_type {
                match trailing_type {
                    tektii_gateway_core::models::TrailingType::Percent => {
                        if let Some(distance) = order.trailing_distance {
                            qp.push(format!("callbackRate={distance}"));
                        }
                    }
                    tektii_gateway_core::models::TrailingType::Absolute => {
                        return Err(GatewayError::unsupported(
                            "trailing_stop_absolute (Binance only supports percentage)",
                            "binance-futures",
                        ));
                    }
                }
            }
            if let Some(sp) = order.stop_price {
                qp.push(format!("activationPrice={sp}"));
            }
        }
        if otype == "LIMIT" || otype == "STOP" {
            let tif = match order.time_in_force {
                TimeInForce::Gtc | TimeInForce::Day => "GTC",
                TimeInForce::Ioc => "IOC",
                TimeInForce::Fok => "FOK",
            };
            qp.push(format!("timeInForce={tif}"));
        }
        if let Some(p) = order.limit_price {
            qp.push(format!("price={p}"));
        }
        if let Some(sp) = order.stop_price {
            qp.push(format!("stopPrice={sp}"));
        }
        if order.reduce_only {
            qp.push("reduceOnly=true".to_string());
        }
        if let Some(ref cid) = order.client_order_id {
            qp.push(format!("newClientOrderId={cid}"));
        }
        let params = qp.join("&");
        let result: GatewayResult<BinanceFuturesOrder> =
            self.send_signed_post("fapi/v1/order", &params).await;
        if let Err(ref e) = result {
            self.record_if_outage(e).await;
        }
        let o = result?;
        let order_id = o.order_id.to_string();

        if let Some(ref group_id) = oco_group_id {
            self.state_manager.add_to_oco_group(&order_id, group_id);
        }

        Ok(OrderHandle {
            id: order_id,
            client_order_id: Some(o.client_order_id),
            correlation_id: None,
            status: crate::map_binance_order_status(&o.status)?,
        })
    }

    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        let oid: i64 = order_id.parse().map_err(|_| GatewayError::InvalidRequest {
            message: format!("Invalid order ID: {order_id}"),
            field: Some("order_id".to_string()),
        })?;
        let sym = if let Some(cached) = self.state_manager.get_order(order_id) {
            cached.symbol
        } else {
            let open: Vec<BinanceFuturesOrder> =
                self.send_signed_get("fapi/v1/openOrders", "").await?;
            open.iter()
                .find(|o| o.order_id == oid)
                .map(|o| o.symbol.clone())
                .ok_or_else(|| GatewayError::OrderNotFound {
                    id: order_id.to_string(),
                })?
        };
        let o: BinanceFuturesOrder = self
            .send_signed_get("fapi/v1/order", &format!("symbol={sym}&orderId={oid}"))
            .await?;
        self.convert_order(&o)
    }

    async fn get_orders(&self, _params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        let orders: Vec<BinanceFuturesOrder> =
            self.send_signed_get("fapi/v1/openOrders", "").await?;
        orders.iter().map(|o| self.convert_order(o)).collect()
    }

    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.get_orders(params).await
    }

    async fn modify_order(
        &self,
        _order_id: &str,
        _request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        Err(GatewayError::unsupported("modify_order", "binance-futures"))
    }

    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        let oid: i64 = order_id.parse().map_err(|_| GatewayError::InvalidRequest {
            message: format!("Invalid order ID: {order_id}"),
            field: Some("order_id".to_string()),
        })?;
        let sym = if let Some(cached) = self.state_manager.get_order(order_id) {
            cached.symbol
        } else {
            let open: Vec<BinanceFuturesOrder> =
                self.send_signed_get("fapi/v1/openOrders", "").await?;
            open.iter()
                .find(|o| o.order_id == oid)
                .map(|o| o.symbol.clone())
                .ok_or_else(|| GatewayError::OrderNotFound {
                    id: order_id.to_string(),
                })?
        };
        self.send_signed_delete("fapi/v1/order", &format!("symbol={sym}&orderId={oid}"))
            .await?;
        let cancelled: BinanceFuturesOrder = self
            .send_signed_get("fapi/v1/order", &format!("symbol={sym}&orderId={oid}"))
            .await?;
        Ok(CancelOrderResult {
            success: true,
            order: self.convert_order(&cancelled)?,
        })
    }

    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        let qp = params
            .symbol
            .as_ref()
            .map(|s| format!("symbol={s}"))
            .unwrap_or_default();
        let trades: Vec<BinanceFuturesTrade> =
            self.send_signed_get("fapi/v1/userTrades", &qp).await?;
        Ok(trades
            .into_iter()
            .map(|t| {
                let side = if t.side == "BUY" {
                    Side::Buy
                } else {
                    Side::Sell
                };
                Trade {
                    id: t.id.to_string(),
                    order_id: t.order_id.to_string(),
                    symbol: t.symbol,
                    side,
                    quantity: Decimal::from_str(&t.qty).unwrap_or_default(),
                    price: Decimal::from_str(&t.price).unwrap_or_default(),
                    commission: Decimal::from_str(&t.commission).unwrap_or_default(),
                    commission_currency: t.commission_asset,
                    is_maker: Some(t.maker),
                    timestamp: chrono::DateTime::from_timestamp_millis(t.time)
                        .unwrap_or_else(chrono::Utc::now),
                }
            })
            .collect())
    }

    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
        let qp = symbol.map(|s| format!("symbol={s}")).unwrap_or_default();
        let positions: Vec<BinanceFuturesPosition> =
            self.send_signed_get("fapi/v2/positionRisk", &qp).await?;
        let now = chrono::Utc::now();
        Ok(positions
            .into_iter()
            .filter_map(|p| {
                let qty = Decimal::from_str(&p.position_amt).unwrap_or_default();
                if qty.abs().is_zero() {
                    return None;
                }
                let side = if qty.is_sign_positive() {
                    PositionSide::Long
                } else {
                    PositionSide::Short
                };
                Some(Position {
                    id: p.symbol.clone(),
                    symbol: p.symbol,
                    side,
                    quantity: qty.abs(),
                    average_entry_price: Decimal::from_str(&p.entry_price).unwrap_or_default(),
                    current_price: Decimal::from_str(&p.mark_price).unwrap_or_default(),
                    unrealized_pnl: Decimal::from_str(&p.un_realized_profit).unwrap_or_default(),
                    realized_pnl: Decimal::ZERO,
                    margin_mode: Some(if p.margin_type == "cross" {
                        tektii_gateway_core::models::MarginMode::Cross
                    } else {
                        tektii_gateway_core::models::MarginMode::Isolated
                    }),
                    leverage: p
                        .leverage
                        .parse::<u32>()
                        .ok()
                        .map(rust_decimal::Decimal::from),
                    liquidation_price: Decimal::from_str(&p.liquidation_price)
                        .ok()
                        .filter(|d| !d.is_zero()),
                    opened_at: now,
                    updated_at: now,
                })
            })
            .collect())
    }

    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        self.get_positions(Some(position_id))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| GatewayError::PositionNotFound {
                id: position_id.to_string(),
            })
    }

    async fn close_position(
        &self,
        position_id: &str,
        request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        let pos = self.get_position(position_id).await?;
        let close_side = if pos.side == PositionSide::Long {
            Side::Sell
        } else {
            Side::Buy
        };
        let qty = request.quantity.unwrap_or(pos.quantity);
        self.submit_order(&OrderRequest {
            symbol: pos.symbol,
            side: close_side,
            order_type: request.order_type.unwrap_or(OrderType::Market),
            quantity: qty,
            limit_price: request.limit_price,
            stop_price: None,
            time_in_force: TimeInForce::Gtc,
            client_order_id: None,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            position_id: None,
            reduce_only: true,
            post_only: false,
            hidden: false,
            display_quantity: None,
            margin_mode: None,
            leverage: None,
            oco_group_id: None,
        })
        .await
    }

    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        let url = format!(
            "{}/fapi/v1/ticker/bookTicker?symbol={symbol}",
            self.base_url
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: e.to_string(),
                provider: Some("binance-futures".to_string()),
                source: None,
            })?;
        let q: BinanceFuturesQuote = resp
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Parse error: {e}")))?;
        let bid = Decimal::from_str(&q.bid_price).unwrap_or_default();
        let ask = Decimal::from_str(&q.ask_price).unwrap_or_default();
        Ok(Quote {
            symbol: q.symbol,
            provider: "binance-futures".to_string(),
            bid,
            bid_size: Some(Decimal::from_str(&q.bid_qty).unwrap_or_default()),
            ask,
            ask_size: Some(Decimal::from_str(&q.ask_qty).unwrap_or_default()),
            last: (bid + ask) / Decimal::from(2),
            volume: None,
            timestamp: chrono::Utc::now(),
        })
    }

    async fn get_bars(&self, symbol: &str, params: &BarParams) -> GatewayResult<Vec<Bar>> {
        let interval = match params.timeframe {
            tektii_gateway_core::models::Timeframe::OneMinute => "1m",
            tektii_gateway_core::models::Timeframe::FiveMinutes => "5m",
            tektii_gateway_core::models::Timeframe::FifteenMinutes
            | tektii_gateway_core::models::Timeframe::TenMinutes => "15m",
            tektii_gateway_core::models::Timeframe::ThirtyMinutes => "30m",
            tektii_gateway_core::models::Timeframe::OneHour => "1h",
            tektii_gateway_core::models::Timeframe::FourHours => "4h",
            tektii_gateway_core::models::Timeframe::OneDay => "1d",
            tektii_gateway_core::models::Timeframe::TwoMinutes => "3m",
            tektii_gateway_core::models::Timeframe::TwoHours => "2h",
            tektii_gateway_core::models::Timeframe::TwelveHours => "12h",
            tektii_gateway_core::models::Timeframe::OneWeek => "1w",
        };
        let mut qp = vec![
            format!("symbol={symbol}"),
            format!("interval={interval}"),
            format!("limit={}", params.limit.unwrap_or(100)),
        ];
        if let Some(s) = params.start {
            qp.push(format!("startTime={}", s.timestamp_millis()));
        }
        if let Some(e) = params.end {
            qp.push(format!("endTime={}", e.timestamp_millis()));
        }
        let url = format!("{}/fapi/v1/klines?{}", self.base_url, qp.join("&"));
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: e.to_string(),
                provider: Some("binance-futures".to_string()),
                source: None,
            })?;
        let klines: Vec<BinanceFuturesKline> = resp
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Parse error: {e}")))?;
        Ok(klines
            .into_iter()
            .filter_map(|k| {
                let ts = chrono::DateTime::from_timestamp_millis(k.open_time)?;
                Some(Bar {
                    symbol: symbol.to_string(),
                    provider: "binance-futures".to_string(),
                    timeframe: params.timeframe,
                    timestamp: ts,
                    open: Decimal::from_str(&k.open).unwrap_or_default(),
                    high: Decimal::from_str(&k.high).unwrap_or_default(),
                    low: Decimal::from_str(&k.low).unwrap_or_default(),
                    close: Decimal::from_str(&k.close).unwrap_or_default(),
                    volume: Decimal::from_str(&k.volume).unwrap_or_default(),
                })
            })
            .collect())
    }

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Ok(BinanceFuturesCapabilities.capabilities())
    }
    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        let url = format!("{}/fapi/v1/ping", self.base_url);
        match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => Ok(ConnectionStatus {
                connected: true,
                latency_ms: 0,
                last_heartbeat: chrono::Utc::now(),
            }),
            Ok(_r) => Ok(ConnectionStatus {
                connected: false,
                latency_ms: 0,
                last_heartbeat: chrono::Utc::now(),
            }),
            Err(_e) => Ok(ConnectionStatus {
                connected: false,
                latency_ms: 0,
                last_heartbeat: chrono::Utc::now(),
            }),
        }
    }

    async fn circuit_breaker_status(&self) -> Option<CircuitBreakerSnapshot> {
        let guard = self.circuit_breaker.read().await;
        Some(CircuitBreakerSnapshot::from_adapter(&guard))
    }

    async fn reset_adapter_circuit_breaker(&self) -> GatewayResult<()> {
        let mut guard = self.circuit_breaker.write().await;
        guard.reset().map_err(|msg| GatewayError::ResetCooldown {
            message: msg.to_string(),
        })
    }
}

impl BinanceFuturesAdapter {
    fn convert_order(&self, o: &BinanceFuturesOrder) -> GatewayResult<Order> {
        let status = crate::map_binance_order_status(&o.status)?;
        let otype = match o.order_type.as_str() {
            "LIMIT" => OrderType::Limit,
            "STOP" | "STOP_MARKET" => OrderType::Stop,
            // MARKET and anything unrecognised
            _ => OrderType::Market,
        };
        let side = if o.side == "BUY" {
            Side::Buy
        } else {
            Side::Sell
        };
        let tif = match o.time_in_force.as_str() {
            "IOC" => TimeInForce::Ioc,
            "FOK" => TimeInForce::Fok,
            _ => TimeInForce::Gtc,
        };
        let qty = Decimal::from_str(&o.orig_qty).unwrap_or_default();
        let filled = Decimal::from_str(&o.executed_qty).unwrap_or_default();
        let avg = Decimal::from_str(&o.avg_price)
            .ok()
            .filter(|d| !d.is_zero());
        let limit_price = Decimal::from_str(&o.price).ok().filter(|d| !d.is_zero());
        let stop_price = o
            .stop_price
            .as_ref()
            .and_then(|sp| Decimal::from_str(sp).ok())
            .filter(|d| !d.is_zero());
        let ts = chrono::DateTime::from_timestamp_millis(o.time).unwrap_or_else(chrono::Utc::now);
        let ut = chrono::DateTime::from_timestamp_millis(o.update_time).unwrap_or(ts);

        Ok(Order {
            id: o.order_id.to_string(),
            client_order_id: Some(o.client_order_id.clone()),
            symbol: o.symbol.clone(),
            side,
            order_type: otype,
            quantity: qty,
            filled_quantity: filled,
            remaining_quantity: qty - filled,
            limit_price,
            stop_price,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            average_fill_price: avg,
            status,
            reject_reason: None,
            position_id: None,
            reduce_only: Some(o.reduce_only),
            post_only: None,
            hidden: None,
            display_quantity: None,
            oco_group_id: self.state_manager.get_oco_group_id(&o.order_id.to_string()),
            correlation_id: None,
            time_in_force: tif,
            created_at: ts,
            updated_at: ut,
        })
    }
}
