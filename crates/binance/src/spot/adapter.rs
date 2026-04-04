//! Binance Spot adapter for the trading gateway.
//!
//! This module provides integration with the Binance API for spot trading.
//! Uses shared utilities from [`super::super::common`] for authentication, HTTP requests,
//! and error mapping.

use super::capabilities::BinanceSpotCapabilities;
use super::types::{
    BINANCE_BASE_URL, BINANCE_TESTNET_URL, BinanceAccount, BinanceCancelReplaceResponse,
    BinanceKline, BinanceOcoResponse, BinanceOrder, BinanceQuote, BinanceTrade,
};
use crate::common::BinanceHttpClient;

use async_trait::async_trait;
use rust_decimal::Decimal;
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
use tektii_gateway_core::exit_management::{ExitHandler, ExitHandlerConfig, ExitHandling};
use tektii_gateway_core::models::{
    Account, Bar, BarParams, CancelAllResult, CancelOrderResult, Capabilities,
    ClosePositionRequest, ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order,
    OrderHandle, OrderQueryParams, OrderRequest, OrderType, PlaceOcoOrderRequest,
    PlaceOcoOrderResponse, Position, PositionSide, Quote, Side, TimeInForce, Trade,
    TradeQueryParams, TradingPlatform,
};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::WsMessage;

use crate::credentials::BinanceCredentials;

/// Binance Spot adapter implementation.
pub struct BinanceSpotAdapter {
    /// HTTP client for authenticated Binance API requests
    http_client: BinanceHttpClient,
    /// Provider capabilities for determining order type support
    capabilities: BinanceSpotCapabilities,

    // === Event Infrastructure ===
    state_manager: Arc<StateManager>,
    exit_handler: Arc<ExitHandler>,
    event_router: Arc<EventRouter>,
    platform: TradingPlatform,
    circuit_breaker: Arc<RwLock<AdapterCircuitBreaker>>,
}

impl BinanceSpotAdapter {
    /// Quote currencies treated as "cash", not positions.
    const QUOTE_CURRENCIES: &'static [&'static str] =
        &["USDT", "BUSD", "USDC", "TUSD", "USD", "EUR", "GBP"];

    /// Dust threshold - balances at or below this are considered dust.
    const DUST_THRESHOLD: Decimal = Decimal::from_parts(1, 0, 0, false, 8); // 0.00000001

    fn is_quote_currency(asset: &str) -> bool {
        Self::QUOTE_CURRENCIES.contains(&asset)
    }

    /// Create a new Binance Spot adapter.
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
                TradingPlatform::BinanceSpotTestnet => BINANCE_TESTNET_URL.to_string(),
                _ => BINANCE_BASE_URL.to_string(),
            });

        let http_client = BinanceHttpClient::new(
            &base_url,
            tektii_gateway_core::arc_secret(&credentials.api_key),
            tektii_gateway_core::arc_secret(&credentials.api_secret),
            "binance",
        )?;

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
            "binance",
        )));

        Ok(Self {
            http_client,
            capabilities: BinanceSpotCapabilities::new(),
            state_manager,
            exit_handler,
            event_router,
            platform,
            circuit_breaker,
        })
    }

    /// Override the retry configuration (useful for tests).
    #[must_use]
    pub fn with_retry_config(mut self, config: tektii_gateway_core::http::RetryConfig) -> Self {
        self.http_client = self.http_client.with_retry_config(config);
        self
    }

    /// Enable custom ExitHandler configuration.
    #[must_use]
    #[allow(dead_code)]
    pub fn with_exit_handler(mut self, config: ExitHandlerConfig) -> Self {
        let exit_handler = Arc::new(ExitHandler::new(
            Arc::clone(&self.state_manager),
            self.platform,
            config,
        ));
        self.exit_handler = exit_handler;
        self
    }

    #[must_use]
    #[allow(dead_code)]
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

    #[must_use]
    #[allow(dead_code)]
    pub fn platform_id(&self) -> TradingPlatform {
        self.platform
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

    /// Convert a Binance order to the gateway Order model.
    fn convert_binance_order(&self, o: &BinanceOrder) -> GatewayResult<Order> {
        let status = crate::map_binance_order_status(&o.status)?;

        let limit_price = if o.price == "0.00000000" {
            None
        } else {
            Decimal::from_str(&o.price).ok()
        };

        let stop_price = o
            .stop_price
            .as_ref()
            .and_then(|sp| Decimal::from_str(sp).ok());

        let quantity = Decimal::from_str(&o.orig_qty).unwrap_or_default();

        let filled_quantity = Decimal::from_str(&o.executed_qty).unwrap_or_default();

        let remaining_quantity = quantity - filled_quantity;

        let average_fill_price = if filled_quantity > Decimal::ZERO {
            let cumulative_quote = Decimal::from_str(&o.cummulative_quote_qty).unwrap_or_default();
            Some(cumulative_quote / filled_quantity)
        } else {
            None
        };

        let order_type = match o.order_type.as_str() {
            "LIMIT" => OrderType::Limit,
            "STOP_LOSS" => OrderType::Stop,
            "STOP_LOSS_LIMIT" => OrderType::StopLimit,
            "TRAILING_STOP_MARKET" => OrderType::TrailingStop,
            // MARKET and anything unrecognised
            _ => OrderType::Market,
        };

        let side = match o.side.as_str() {
            "BUY" => Side::Buy,
            _ => Side::Sell,
        };

        let time_in_force = match o.time_in_force.as_str() {
            "IOC" => TimeInForce::Ioc,
            "FOK" => TimeInForce::Fok,
            // GTC and anything unrecognised
            _ => TimeInForce::Gtc,
        };

        let timestamp =
            chrono::DateTime::from_timestamp_millis(o.time).unwrap_or_else(chrono::Utc::now);

        let updated_at =
            chrono::DateTime::from_timestamp_millis(o.update_time).unwrap_or(timestamp);

        Ok(Order {
            id: o.order_id.to_string(),
            client_order_id: Some(o.client_order_id.clone()),
            symbol: o.symbol.clone(),
            side,
            order_type,
            quantity,
            filled_quantity,
            remaining_quantity,
            limit_price,
            stop_price,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            average_fill_price,
            status,
            reject_reason: None,
            position_id: None,
            reduce_only: None,
            post_only: None,
            hidden: None,
            display_quantity: None,
            oco_group_id: self.state_manager.get_oco_group_id(&o.order_id.to_string()),
            correlation_id: None,
            time_in_force,
            created_at: timestamp,
            updated_at,
        })
    }
}

#[async_trait]
impl TradingAdapter for BinanceSpotAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &self.capabilities
    }

    fn platform(&self) -> TradingPlatform {
        self.platform
    }

    fn provider_name(&self) -> &'static str {
        "Binance"
    }

    async fn get_account(&self) -> GatewayResult<Account> {
        self.check_circuit_breaker().await?;

        let result: GatewayResult<BinanceAccount> =
            self.http_client.signed_get("api/v3/account", "").await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let binance_account = result?;

        let mut total_cash = Decimal::ZERO;
        let mut total_locked = Decimal::ZERO;

        for balance in &binance_account.balances {
            if balance.asset == "USDT" {
                let free = balance
                    .free
                    .parse::<Decimal>()
                    .map_err(|e| GatewayError::internal(format!("Failed to parse balance: {e}")))?;
                let locked = balance
                    .locked
                    .parse::<Decimal>()
                    .map_err(|e| GatewayError::internal(format!("Failed to parse balance: {e}")))?;
                total_cash += free;
                total_locked += locked;
            }
        }

        let equity = total_cash + total_locked;

        Ok(Account {
            balance: total_cash,
            equity,
            margin_used: total_locked,
            margin_available: total_cash,
            unrealized_pnl: Decimal::ZERO,
            currency: "USDT".to_string(),
        })
    }

    async fn submit_order(&self, order: &OrderRequest) -> GatewayResult<OrderHandle> {
        self.check_circuit_breaker().await?;

        let oco_group_id = order.oco_group_id.clone();

        let binance_side = match order.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };

        let binance_order_type = match order.order_type {
            OrderType::Market => "MARKET",
            OrderType::Limit => "LIMIT",
            OrderType::Stop => "STOP_LOSS",
            OrderType::StopLimit => "STOP_LOSS_LIMIT",
            OrderType::TrailingStop => {
                // Binance Spot does not support native trailing stops.
                // Use Futures for TRAILING_STOP_MARKET, or implement
                // PriceSource-based emulation (deferred item #2).
                return Err(GatewayError::UnsupportedOperation {
                    operation: "trailing_stop (not supported on Binance Spot)".to_string(),
                    provider: "binance-spot".to_string(),
                });
            }
        };

        let binance_tif = match order.time_in_force {
            TimeInForce::Gtc | TimeInForce::Day => "GTC",
            TimeInForce::Ioc => "IOC",
            TimeInForce::Fok => "FOK",
        };

        let mut query_params = vec![
            format!("symbol={}", order.symbol),
            format!("side={binance_side}"),
            format!("type={binance_order_type}"),
            format!("quantity={}", order.quantity),
        ];

        if binance_order_type == "LIMIT" || binance_order_type == "STOP_LOSS_LIMIT" {
            query_params.push(format!("timeInForce={binance_tif}"));
        }

        if let Some(price) = order.limit_price {
            query_params.push(format!("price={price}"));
        }

        if let Some(stop_price) = order.stop_price {
            query_params.push(format!("stopPrice={stop_price}"));
        }

        if let Some(ref client_id) = order.client_order_id {
            query_params.push(format!("newClientOrderId={client_id}"));
        }

        let params = query_params.join("&");

        let result: GatewayResult<BinanceOrder> =
            self.http_client.signed_post("api/v3/order", &params).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let binance_order = result?;
        let order_id = binance_order.order_id.to_string();

        if let Some(ref group_id) = oco_group_id {
            self.state_manager.add_to_oco_group(&order_id, group_id);
        }

        Ok(OrderHandle {
            id: order_id,
            client_order_id: Some(binance_order.client_order_id),
            correlation_id: None,
            status: crate::map_binance_order_status(&binance_order.status)?,
        })
    }

    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        let binance_order_id =
            order_id
                .parse::<i64>()
                .map_err(|_| GatewayError::InvalidRequest {
                    message: format!("Invalid order ID format: {order_id}"),
                    field: Some("order_id".to_string()),
                })?;

        let symbol = if let Some(cached) = self.state_manager.get_order(order_id) {
            cached.symbol
        } else {
            let open_orders: Vec<BinanceOrder> =
                self.http_client.signed_get("api/v3/openOrders", "").await?;
            open_orders
                .iter()
                .find(|o| o.order_id == binance_order_id)
                .map(|o| o.symbol.clone())
                .ok_or_else(|| GatewayError::OrderNotFound {
                    id: order_id.to_string(),
                })?
        };

        let params = format!("symbol={symbol}&orderId={binance_order_id}");
        let binance_order: BinanceOrder =
            self.http_client.signed_get("api/v3/order", &params).await?;

        self.convert_binance_order(&binance_order)
    }

    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        let use_open_orders = params.status.as_ref().is_some_and(|statuses| {
            statuses.contains(&tektii_gateway_core::models::OrderStatus::Open)
        });

        // Binance's allOrders endpoint requires a symbol parameter.
        // openOrders works without symbol (returns all open orders across all symbols).
        if !use_open_orders && params.symbol.is_none() {
            return Err(GatewayError::InvalidRequest {
                message: "Binance requires a symbol to query order history. \
                          Use a status filter for open orders to list across all symbols."
                    .to_string(),
                field: Some("symbol".to_string()),
            });
        }

        let mut query_params = Vec::new();

        if let Some(ref sym) = params.symbol {
            query_params.push(format!("symbol={sym}"));
        }

        if let Some(limit) = params.limit {
            query_params.push(format!("limit={limit}"));
        }

        if let Some(after) = params.since {
            query_params.push(format!("startTime={}", after.timestamp_millis()));
        }

        if let Some(until) = params.until {
            query_params.push(format!("endTime={}", until.timestamp_millis()));
        }

        let base_params = query_params.join("&");
        let endpoint = if use_open_orders {
            "api/v3/openOrders"
        } else {
            "api/v3/allOrders"
        };

        let binance_orders: Vec<BinanceOrder> =
            self.http_client.signed_get(endpoint, &base_params).await?;

        binance_orders
            .iter()
            .map(|o| self.convert_binance_order(o))
            .collect()
    }

    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.get_orders(params).await
    }

    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        let binance_order_id =
            order_id
                .parse::<i64>()
                .map_err(|_| GatewayError::InvalidRequest {
                    message: format!("Invalid order ID format: {order_id}"),
                    field: Some("order_id".to_string()),
                })?;

        let params = format!("orderId={binance_order_id}");
        let existing_order: BinanceOrder =
            self.http_client.signed_get("api/v3/order", &params).await?;

        if existing_order.status != "NEW" && existing_order.status != "PARTIALLY_FILLED" {
            return Err(GatewayError::OrderNotModifiable {
                order_id: order_id.to_string(),
                reason: format!("Order status is {}", existing_order.status),
            });
        }

        let new_quantity = request
            .quantity
            .map_or_else(|| existing_order.orig_qty.clone(), |q| q.to_string());
        let new_price = request.limit_price.map(|p| p.to_string()).or_else(|| {
            if existing_order.price == "0.00000000" {
                None
            } else {
                Some(existing_order.price.clone())
            }
        });

        let mut query_params = vec![
            format!("symbol={}", existing_order.symbol),
            format!("side={}", existing_order.side),
            format!("type={}", existing_order.order_type),
            "cancelReplaceMode=STOP_ON_FAILURE".to_string(),
            format!("cancelOrderId={binance_order_id}"),
            format!("quantity={new_quantity}"),
            format!("timeInForce={}", existing_order.time_in_force),
        ];

        if let Some(price) = new_price {
            query_params.push(format!("price={price}"));
        }

        if let Some(stop_price) = request.stop_price {
            query_params.push(format!("stopPrice={stop_price}"));
        }

        let params = query_params.join("&");

        let response: BinanceCancelReplaceResponse = self
            .http_client
            .signed_post("api/v3/order/cancelReplace", &params)
            .await?;

        if response.cancel_result != "SUCCESS" {
            return Err(GatewayError::OrderNotModifiable {
                order_id: order_id.to_string(),
                reason: format!("Cancel failed: {}", response.cancel_result),
            });
        }

        if response.new_order_result != "SUCCESS" {
            return Err(GatewayError::InvalidRequest {
                message: format!("New order placement failed: {}", response.new_order_result),
                field: None,
            });
        }

        let new_order = response.new_order_response.ok_or_else(|| {
            GatewayError::internal("Cancel-replace succeeded but no new order in response")
        })?;

        let order = self.convert_binance_order(&new_order)?;
        Ok(ModifyOrderResult {
            order,
            previous_order_id: Some(order_id.to_string()),
        })
    }

    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        let binance_order_id =
            order_id
                .parse::<i64>()
                .map_err(|_| GatewayError::InvalidRequest {
                    message: format!("Invalid order ID format: {order_id}"),
                    field: Some("order_id".to_string()),
                })?;

        let symbol = if let Some(cached) = self.state_manager.get_order(order_id) {
            cached.symbol
        } else {
            let open_orders: Vec<BinanceOrder> =
                self.http_client.signed_get("api/v3/openOrders", "").await?;
            open_orders
                .iter()
                .find(|o| o.order_id == binance_order_id)
                .map(|o| o.symbol.clone())
                .ok_or_else(|| GatewayError::OrderNotFound {
                    id: order_id.to_string(),
                })?
        };

        let params = format!("symbol={symbol}&orderId={binance_order_id}");
        self.http_client
            .signed_delete("api/v3/order", &params)
            .await?;

        let get_params = format!("symbol={symbol}&orderId={binance_order_id}");
        let cancelled: BinanceOrder = self
            .http_client
            .signed_get("api/v3/order", &get_params)
            .await?;

        let order = self.convert_binance_order(&cancelled)?;
        Ok(CancelOrderResult {
            success: true,
            order,
        })
    }

    async fn cancel_all_orders(&self, symbol: Option<&str>) -> GatewayResult<CancelAllResult> {
        if let Some(sym) = symbol {
            let params = format!("symbol={sym}");
            let cancelled_orders: Vec<serde_json::Value> = self
                .http_client
                .signed_delete_with_response("api/v3/openOrders", &params)
                .await?;

            Ok(CancelAllResult {
                cancelled_count: u32::try_from(cancelled_orders.len()).unwrap_or(u32::MAX),
                failed_order_ids: None,
                failed_count: 0,
            })
        } else {
            Err(GatewayError::InvalidRequest {
                message: "Binance requires a symbol to cancel all orders".to_string(),
                field: None,
            })
        }
    }

    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        if params.symbol.is_none() {
            return Err(GatewayError::InvalidRequest {
                message: "Binance requires a symbol to query trades".to_string(),
                field: Some("symbol".to_string()),
            });
        }

        let query_params = params
            .symbol
            .as_ref()
            .map(|sym| format!("symbol={sym}"))
            .unwrap_or_default();

        let binance_trades: Vec<BinanceTrade> = self
            .http_client
            .signed_get("api/v3/myTrades", &query_params)
            .await?;

        let mut trades = Vec::new();
        for bt in binance_trades {
            let timestamp =
                chrono::DateTime::from_timestamp_millis(bt.time).unwrap_or_else(chrono::Utc::now);

            let side = if bt.is_buyer { Side::Buy } else { Side::Sell };

            trades.push(Trade {
                id: bt.id.to_string(),
                order_id: bt.order_id.to_string(),
                symbol: bt.symbol,
                side,
                quantity: Decimal::from_str(&bt.qty).unwrap_or_default(),
                price: Decimal::from_str(&bt.price).unwrap_or_default(),
                commission: Decimal::from_str(&bt.commission).unwrap_or_default(),
                commission_currency: bt.commission_asset,
                is_maker: Some(bt.is_maker),
                timestamp,
            });
        }

        Ok(trades)
    }

    async fn get_positions(&self, symbol_filter: Option<&str>) -> GatewayResult<Vec<Position>> {
        let binance_account: BinanceAccount =
            self.http_client.signed_get("api/v3/account", "").await?;

        let now = chrono::Utc::now();
        let mut positions = Vec::new();

        for balance in binance_account.balances {
            if Self::is_quote_currency(&balance.asset) {
                continue;
            }

            let free = Decimal::from_str(&balance.free).unwrap_or_default();
            let locked = Decimal::from_str(&balance.locked).unwrap_or_default();
            let quantity = free + locked;

            if quantity <= Self::DUST_THRESHOLD {
                continue;
            }

            let trading_symbol = format!("{}USDT", balance.asset);

            if let Some(filter) = symbol_filter
                && trading_symbol != filter
            {
                continue;
            }

            positions.push(Position {
                id: trading_symbol.clone(),
                symbol: trading_symbol,
                side: PositionSide::Long,
                quantity,
                average_entry_price: Decimal::ZERO,
                current_price: Decimal::ZERO,
                unrealized_pnl: Decimal::ZERO,
                realized_pnl: Decimal::ZERO,
                margin_mode: None,
                leverage: None,
                liquidation_price: None,
                opened_at: now,
                updated_at: now,
            });
        }

        Ok(positions)
    }

    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        let positions = self.get_positions(Some(position_id)).await?;
        positions
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
        let position = self.get_position(position_id).await?;
        let close_quantity = request.quantity.unwrap_or(position.quantity);

        let order_request = OrderRequest {
            symbol: position.symbol,
            side: Side::Sell,
            order_type: request.order_type.unwrap_or(OrderType::Market),
            quantity: close_quantity,
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
        };

        self.submit_order(&order_request).await
    }

    async fn place_oco_order(
        &self,
        request: &PlaceOcoOrderRequest,
    ) -> GatewayResult<PlaceOcoOrderResponse> {
        let binance_side = match request.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };

        let quantity_str = format!("{:.8}", request.quantity);
        let tp_price_str = format!("{:.8}", request.take_profit_price);
        let sl_trigger_str = format!("{:.8}", request.stop_loss_trigger);
        let sl_limit_str = format!(
            "{:.8}",
            request.stop_loss_limit.unwrap_or(request.stop_loss_trigger)
        );

        let query_params = [
            format!("symbol={}", request.symbol),
            format!("side={binance_side}"),
            format!("quantity={quantity_str}"),
            format!("price={tp_price_str}"),
            format!("stopPrice={sl_trigger_str}"),
            format!("stopLimitPrice={sl_limit_str}"),
            "stopLimitTimeInForce=GTC".to_string(),
        ];

        let params = query_params.join("&");

        let oco_response: BinanceOcoResponse = self
            .http_client
            .signed_post("api/v3/order/oco", &params)
            .await?;

        let oco_group_id = oco_response.order_list_id.to_string();

        let mut order_handles = Vec::new();
        for leg in &oco_response.order_reports {
            let handle = OrderHandle {
                id: leg.order_id.to_string(),
                client_order_id: Some(leg.client_order_id.clone()),
                correlation_id: None,
                status: crate::map_binance_order_status(&leg.status)?,
            };
            self.state_manager
                .add_to_oco_group(&handle.id, &oco_group_id);
            order_handles.push(handle);
        }

        let sl_id = order_handles
            .first()
            .map(|h| h.id.clone())
            .unwrap_or_default();
        let tp_id = order_handles
            .get(1)
            .map(|h| h.id.clone())
            .unwrap_or_default();

        Ok(PlaceOcoOrderResponse {
            list_order_id: oco_group_id,
            stop_loss_order_id: sl_id,
            take_profit_order_id: tp_id,
            symbol: request.symbol.clone(),
            status: "active".to_string(),
        })
    }

    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        let url = format!(
            "{}/api/v3/ticker/bookTicker?symbol={}",
            self.http_client.base_url(),
            symbol
        );

        let response = self
            .http_client
            .client()
            .get(&url)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: e.to_string(),
                provider: Some("binance".to_string()),
                source: None,
            })?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderError {
                message: error_text,
                provider: Some("binance".to_string()),
                source: None,
            });
        }

        let binance_quote: BinanceQuote =
            response
                .json()
                .await
                .map_err(|e| GatewayError::ProviderError {
                    message: e.to_string(),
                    provider: Some("binance".to_string()),
                    source: None,
                })?;

        let bid = Decimal::from_str(&binance_quote.bid_price).unwrap_or_default();
        let ask = Decimal::from_str(&binance_quote.ask_price).unwrap_or_default();
        let bid_size = Decimal::from_str(&binance_quote.bid_qty).unwrap_or_default();
        let ask_size = Decimal::from_str(&binance_quote.ask_qty).unwrap_or_default();
        let two = Decimal::from(2);
        let mid = (bid + ask) / two;

        Ok(Quote {
            symbol: binance_quote.symbol,
            provider: "binance".to_string(),
            bid,
            bid_size: Some(bid_size),
            ask,
            ask_size: Some(ask_size),
            last: mid,
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
            tektii_gateway_core::models::Timeframe::OneWeek => "1w",
            tektii_gateway_core::models::Timeframe::TwoMinutes => "3m",
            tektii_gateway_core::models::Timeframe::TwoHours => "2h",
            tektii_gateway_core::models::Timeframe::TwelveHours => "12h",
        };

        let mut query_params = vec![
            format!("symbol={symbol}"),
            format!("interval={interval}"),
            format!("limit={}", params.limit.unwrap_or(100)),
        ];

        if let Some(start) = params.start {
            query_params.push(format!("startTime={}", start.timestamp_millis()));
        }

        if let Some(end) = params.end {
            query_params.push(format!("endTime={}", end.timestamp_millis()));
        }

        let query = query_params.join("&");
        let url = format!("{}/api/v3/klines?{query}", self.http_client.base_url());

        let response = self
            .http_client
            .client()
            .get(&url)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: e.to_string(),
                provider: Some("binance".to_string()),
                source: None,
            })?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderError {
                message: error_text,
                provider: Some("binance".to_string()),
                source: None,
            });
        }

        let binance_klines: Vec<BinanceKline> =
            response
                .json()
                .await
                .map_err(|e| GatewayError::ProviderError {
                    message: e.to_string(),
                    provider: Some("binance".to_string()),
                    source: None,
                })?;

        let mut bars = Vec::new();
        for kline in binance_klines {
            let timestamp = chrono::DateTime::from_timestamp_millis(kline.open_time)
                .ok_or_else(|| GatewayError::internal("Invalid timestamp"))?;

            bars.push(Bar {
                symbol: symbol.to_string(),
                provider: "binance".to_string(),
                timeframe: params.timeframe,
                timestamp,
                open: Decimal::from_str(&kline.open).unwrap_or_default(),
                high: Decimal::from_str(&kline.high).unwrap_or_default(),
                low: Decimal::from_str(&kline.low).unwrap_or_default(),
                close: Decimal::from_str(&kline.close).unwrap_or_default(),
                volume: Decimal::from_str(&kline.volume).unwrap_or_default(),
            });
        }

        Ok(bars)
    }

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Ok(self.capabilities.capabilities())
    }

    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        let url = format!("{}/api/v3/ping", self.http_client.base_url());
        let result = self.http_client.client().get(&url).send().await;

        match result {
            Ok(resp) if resp.status().is_success() => Ok(ConnectionStatus {
                connected: true,
                latency_ms: 0,
                last_heartbeat: chrono::Utc::now(),
            }),
            _ => Ok(ConnectionStatus {
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
