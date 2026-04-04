//! Tektii adapter for the trading engine.
//!
//! This adapter connects to the Tektii Engine via HTTP/WebSocket, treating it as
//! another "exchange" like Alpaca or Binance.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use rust_decimal::prelude::FromPrimitive;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;
use tracing::{debug, instrument, warn};

use tektii_gateway_core::adapter::{ProviderCapabilities, TradingAdapter};
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::{ExitHandler, ExitHandlerConfig};
use tektii_gateway_core::models::{
    Account, BarParams, CancelAllResult, CancelOrderResult, Capabilities, ClosePositionRequest,
    ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams,
    OrderRequest, OrderStatus, Position, Quote, Trade, TradeQueryParams, TradingPlatform,
};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_protocol::rest as engine_types;

use crate::auth::TektiiCredentials;
use crate::capabilities::TektiiCapabilities;
use crate::conversions;

/// Response from engine's cancel all endpoint (for deserialization).
/// Engine's `CancelAllResult` doesn't derive `Deserialize`, so we define our own.
#[derive(Debug, Deserialize)]
struct EngineCancelAllResponse {
    cancelled_count: u32,
    failed_count: u32,
}

/// Tektii adapter implementing `TradingAdapter` for the engine.
pub struct TektiiAdapter {
    /// HTTP client for REST API calls
    client: Client,
    /// Base URL for engine REST API (e.g., "<http://localhost:8080>")
    rest_url: String,
    /// WebSocket URL for engine events (e.g., "<ws://localhost:8081>")
    ws_url: String,
    /// Provider capabilities
    capabilities: TektiiCapabilities,

    // === Event Infrastructure ===
    /// State Manager for caching orders and positions
    state_manager: Arc<StateManager>,
    /// Exit Handler for managing stop-loss and take-profit orders
    exit_handler: Arc<ExitHandler>,
    /// Event Router for processing WebSocket events
    event_router: Arc<EventRouter>,
}

impl TektiiAdapter {
    /// Create a new `TektiiAdapter`.
    ///
    /// # Arguments
    ///
    /// * `credentials` - Engine connection URLs
    /// * `broadcaster` - Broadcast channel for sending events to strategies
    /// # Errors
    ///
    /// Returns `reqwest::Error` if the HTTP client fails to build.
    pub fn new(
        credentials: &TektiiCredentials,
        broadcaster: broadcast::Sender<WsMessage>,
    ) -> Result<Self, reqwest::Error> {
        // Configure client with connection pooling
        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        let platform = TradingPlatform::Tektii;

        // Create shared StateManager
        let state_manager = Arc::new(StateManager::new());

        // Create ExitHandler
        let exit_handler = Arc::new(ExitHandler::with_defaults(
            Arc::clone(&state_manager),
            platform,
        ));

        // Create EventRouter
        let event_router = Arc::new(EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler) as Arc<_>,
            broadcaster,
            platform,
        ));

        Ok(Self {
            client,
            rest_url: credentials.rest_url.clone(),
            ws_url: credentials.ws_url.clone(),
            capabilities: TektiiCapabilities::new(),
            state_manager,
            exit_handler,
            event_router,
        })
    }

    /// Enable custom `ExitHandler` configuration.
    ///
    /// By default, `new()` creates an `ExitHandler` with default configuration.
    /// Use this method to customize the configuration.
    #[must_use]
    pub fn with_exit_handler(mut self, config: ExitHandlerConfig) -> Self {
        let platform = TradingPlatform::Tektii;
        let exit_handler = Arc::new(ExitHandler::new(
            Arc::clone(&self.state_manager),
            platform,
            config,
        ));
        self.exit_handler = exit_handler;
        self
    }

    // =========================================================================
    // Event Infrastructure Accessors
    // =========================================================================

    /// Returns a reference to the `StateManager`.
    #[must_use]
    pub fn state_manager(&self) -> Arc<StateManager> {
        Arc::clone(&self.state_manager)
    }

    /// Returns a reference to the `ExitHandler`.
    #[must_use]
    pub fn exit_handler(&self) -> Arc<ExitHandler> {
        Arc::clone(&self.exit_handler)
    }

    /// Returns a reference to the `EventRouter`.
    #[must_use]
    pub fn event_router(&self) -> Arc<EventRouter> {
        Arc::clone(&self.event_router)
    }

    /// Returns the WebSocket URL for the engine.
    #[must_use]
    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    // =========================================================================
    // HTTP Helpers
    // =========================================================================

    /// Build provider error from message.
    fn provider_error(message: String) -> GatewayError {
        GatewayError::ProviderError {
            message,
            provider: Some("tektii".to_string()),
            source: None,
        }
    }

    /// Handle HTTP response, checking status and parsing JSON.
    async fn handle_response<T: DeserializeOwned>(response: reqwest::Response) -> GatewayResult<T> {
        let status = response.status();
        if !status.is_success() {
            // Parse Retry-After header before consuming the body
            let retry_after_seconds = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

            // Map specific HTTP status codes to appropriate error types
            return match status.as_u16() {
                429 => Err(GatewayError::RateLimited {
                    retry_after_seconds,
                    reset_at: None,
                }),
                422 => {
                    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
                    let reject_code = parsed
                        .as_ref()
                        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(String::from));
                    Err(GatewayError::OrderRejected {
                        reason: body,
                        reject_code,
                        details: parsed,
                    })
                }
                503 => Err(GatewayError::ProviderUnavailable { message: body }),
                _ => Err(Self::provider_error(body)),
            };
        }
        response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse response: {e}")))
    }

    /// Handle HTTP response with 404 check for orders.
    async fn handle_order_response<T: DeserializeOwned>(
        response: reqwest::Response,
        order_id: &str,
    ) -> GatewayResult<T> {
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }
        Self::handle_response(response).await
    }
}

#[async_trait]
impl TradingAdapter for TektiiAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &self.capabilities
    }

    fn platform(&self) -> TradingPlatform {
        TradingPlatform::Tektii
    }

    fn provider_name(&self) -> &'static str {
        "tektii"
    }

    #[instrument(skip(self), name = "tektii_get_account")]
    async fn get_account(&self) -> GatewayResult<Account> {
        let start = Instant::now();
        let response = self
            .client
            .get(format!("{}/api/v1/account", self.rest_url))
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let engine_account: engine_types::Account = Self::handle_response(response).await?;
        Ok(conversions::engine_account_to_api(&engine_account))
    }

    #[instrument(skip(self, request), name = "tektii_submit_order")]
    async fn submit_order(&self, request: &OrderRequest) -> GatewayResult<OrderHandle> {
        // Preserve oco_group_id from request (engine doesn't track this)
        let oco_group_id = request.oco_group_id.clone();

        // Convert request to engine format (validates unsupported features)
        let engine_request = conversions::order_request_to_engine(request)?;

        let start = Instant::now();
        let response = self
            .client
            .post(format!("{}/api/v1/orders", self.rest_url))
            .json(&engine_request)
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let engine_handle: engine_types::OrderHandle = Self::handle_response(response).await?;
        let handle = conversions::engine_order_handle_to_api(&engine_handle);

        // Register in OCO group tracking if oco_group_id is set
        if let Some(ref group_id) = oco_group_id {
            self.state_manager.add_to_oco_group(&handle.id, group_id);
        }

        Ok(handle)
    }

    #[instrument(skip(self), name = "tektii_get_order")]
    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        let start = Instant::now();
        let response = self
            .client
            .get(format!("{}/api/v1/orders/{}", self.rest_url, order_id))
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let engine_order: engine_types::Order =
            Self::handle_order_response(response, order_id).await?;
        let mut order = conversions::engine_order_to_api(&engine_order);

        // Enrich with oco_group_id from StateManager (engine doesn't track this)
        order.oco_group_id = self.state_manager.get_oco_group_id(&order.id);

        Ok(order)
    }

    #[instrument(skip(self), name = "tektii_get_orders")]
    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        let mut request = self.client.get(format!("{}/api/v1/orders", self.rest_url));

        if let Some(symbol) = &params.symbol {
            request = request.query(&[("symbol", symbol)]);
        }
        // Engine takes a single status, gateway takes Vec<OrderStatus>.
        // Send the first element if present.
        if let Some(statuses) = &params.status
            && let Some(first_status) = statuses.first()
        {
            request = request.query(&[("status", format!("{first_status:?}").to_lowercase())]);
        }

        let start = Instant::now();
        let response = request
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let engine_response: engine_types::OrdersResponse = Self::handle_response(response).await?;
        let orders: Vec<Order> = engine_response
            .orders
            .iter()
            .map(|engine_order| {
                let mut order = conversions::engine_order_to_api(engine_order);
                // Enrich with oco_group_id from StateManager (engine doesn't track this)
                order.oco_group_id = self.state_manager.get_oco_group_id(&order.id);
                order
            })
            .collect();
        Ok(orders)
    }

    #[instrument(skip(self), name = "tektii_get_order_history")]
    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        // Engine doesn't have a separate history endpoint - just filter by status
        self.get_orders(params).await
    }

    #[instrument(skip(self, request), name = "tektii_modify_order")]
    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        // Engine doesn't support native modify - implement as cancel + replace.
        // WARNING: This is non-atomic. If the replacement order fails, the original
        // is already cancelled with no rollback.

        // Get the original order to preserve fields not being modified
        let original_order = self.get_order(order_id).await?;

        // Check if order is in a modifiable state
        match original_order.status {
            OrderStatus::Filled
            | OrderStatus::Cancelled
            | OrderStatus::Rejected
            | OrderStatus::Expired => {
                return Err(GatewayError::OrderNotModifiable {
                    order_id: order_id.to_string(),
                    reason: format!("Order is {:?}", original_order.status),
                });
            }
            OrderStatus::Open
            | OrderStatus::PartiallyFilled
            | OrderStatus::Pending
            | OrderStatus::PendingCancel => {
                // Order is modifiable, continue
            }
        }

        // Cancel the existing order
        self.cancel_order(order_id).await?;

        // Create new order with modifications applied
        let new_request = OrderRequest {
            symbol: original_order.symbol.clone(),
            side: original_order.side,
            order_type: original_order.order_type,
            quantity: request.quantity.unwrap_or(original_order.quantity),
            limit_price: request.limit_price.or(original_order.limit_price),
            stop_price: request.stop_price.or(original_order.stop_price),
            stop_loss: request.stop_loss.or(original_order.stop_loss),
            take_profit: request.take_profit.or(original_order.take_profit),
            trailing_distance: request
                .trailing_distance
                .or(original_order.trailing_distance),
            trailing_type: original_order.trailing_type,
            client_order_id: original_order.client_order_id.clone(),
            position_id: original_order.position_id.clone(),
            reduce_only: original_order.reduce_only.unwrap_or(false),
            post_only: original_order.post_only.unwrap_or(false),
            hidden: original_order.hidden.unwrap_or(false),
            display_quantity: original_order.display_quantity,
            oco_group_id: original_order.oco_group_id.clone(),
            time_in_force: original_order.time_in_force,
            margin_mode: None,
            leverage: None,
        };

        // Attempt replacement - log warning if it fails after cancel
        let handle = match self.submit_order(&new_request).await {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    original_order_id = order_id,
                    original_symbol = original_order.symbol,
                    error = ?e,
                    "Replacement order failed after cancel - order is now cancelled with no replacement"
                );
                return Err(e);
            }
        };

        let new_order = self.get_order(&handle.id).await?;
        Ok(ModifyOrderResult {
            order: new_order,
            previous_order_id: Some(order_id.to_string()),
        })
    }

    #[instrument(skip(self), name = "tektii_cancel_order")]
    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        // Delete first, then fetch the updated order
        let start = Instant::now();
        let response = self
            .client
            .delete(format!("{}/api/v1/orders/{}", self.rest_url, order_id))
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(Self::provider_error(body));
        }

        // Fetch the cancelled order to return in result
        let cancelled_order = self.get_order(order_id).await?;

        Ok(CancelOrderResult {
            success: true,
            order: cancelled_order,
        })
    }

    #[instrument(skip(self), name = "tektii_cancel_all_orders")]
    async fn cancel_all_orders(&self, symbol: Option<&str>) -> GatewayResult<CancelAllResult> {
        let mut request = self
            .client
            .delete(format!("{}/api/v1/orders", self.rest_url));

        if let Some(sym) = symbol {
            request = request.query(&[("symbol", sym)]);
        }

        let start = Instant::now();
        let response = request
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let engine_result: EngineCancelAllResponse = Self::handle_response(response).await?;

        Ok(CancelAllResult {
            cancelled_count: engine_result.cancelled_count,
            failed_count: engine_result.failed_count,
            failed_order_ids: None,
        })
    }

    #[instrument(skip(self), name = "tektii_get_trades")]
    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        let mut request = self.client.get(format!("{}/api/v1/trades", self.rest_url));

        if let Some(symbol) = &params.symbol {
            request = request.query(&[("symbol", symbol)]);
        }
        if let Some(order_id) = &params.order_id {
            request = request.query(&[("order_id", order_id)]);
        }

        let start = Instant::now();
        let response = request
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let engine_response: engine_types::TradesResponse = Self::handle_response(response).await?;
        Ok(engine_response
            .trades
            .iter()
            .map(conversions::engine_trade_to_api)
            .collect())
    }

    #[instrument(skip(self), name = "tektii_get_positions")]
    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
        let mut request = self
            .client
            .get(format!("{}/api/v1/positions", self.rest_url));

        if let Some(sym) = symbol {
            request = request.query(&[("symbol", sym)]);
        }

        let start = Instant::now();
        let response = request
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        let engine_response: engine_types::PositionsResponse =
            Self::handle_response(response).await?;
        Ok(engine_response
            .positions
            .iter()
            .map(conversions::engine_position_to_api)
            .collect())
    }

    #[instrument(skip(self), name = "tektii_get_position")]
    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        // Engine doesn't have get-by-id - get all and filter
        let positions = self.get_positions(None).await?;
        positions
            .into_iter()
            .find(|p| p.id == position_id)
            .ok_or_else(|| GatewayError::PositionNotFound {
                id: position_id.to_string(),
            })
    }

    #[instrument(skip(self, request), name = "tektii_close_position")]
    async fn close_position(
        &self,
        position_id: &str,
        request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        // Get position to determine close order details
        let position = self.get_position(position_id).await?;

        // Create a closing order (opposite side, market order)
        use tektii_gateway_core::models::{OrderType, PositionSide, Side, TimeInForce};

        let close_side = match position.side {
            PositionSide::Long => Side::Sell,
            PositionSide::Short => Side::Buy,
        };

        let quantity = request.quantity.unwrap_or(position.quantity);

        let close_request = OrderRequest {
            symbol: position.symbol,
            side: close_side,
            order_type: OrderType::Market,
            quantity,
            limit_price: None,
            stop_price: None,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            client_order_id: None,
            position_id: Some(position_id.to_string()),
            reduce_only: true,
            post_only: false,
            hidden: false,
            display_quantity: None,
            oco_group_id: None,
            time_in_force: TimeInForce::Gtc,
            margin_mode: None,
            leverage: None,
        };

        self.submit_order(&close_request).await
    }

    // === Market Data ===

    #[instrument(skip(self), name = "tektii_get_quote")]
    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        let start = Instant::now();
        let response = self
            .client
            .get(format!("{}/api/v1/quote", self.rest_url))
            .query(&[("symbol", symbol)])
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        // Handle specific error responses
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::SymbolNotFound {
                symbol: symbol.to_string(),
            });
        }

        let engine_quote: engine_types::Quote = Self::handle_response(response).await?;

        // Convert engine quote to gateway quote
        // Fail loudly on invalid timestamps to avoid time-skew bugs in simulation
        let timestamp = i64::try_from(engine_quote.timestamp)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .ok_or_else(|| {
                warn!(
                    timestamp = engine_quote.timestamp,
                    symbol = engine_quote.symbol,
                    "Invalid quote timestamp from engine"
                );
                GatewayError::internal(format!(
                    "Invalid quote timestamp: {}",
                    engine_quote.timestamp
                ))
            })?;

        Ok(Quote {
            symbol: engine_quote.symbol,
            provider: "tektii".to_string(),
            bid: engine_quote.bid,
            bid_size: None, // Engine doesn't provide size
            ask: engine_quote.ask,
            ask_size: None,         // Engine doesn't provide size
            last: engine_quote.bid, // Engine doesn't have last price, use bid as approximation
            volume: None,           // Engine doesn't provide volume
            timestamp,
        })
    }

    #[instrument(skip(self), name = "tektii_get_bars")]
    async fn get_bars(
        &self,
        symbol: &str,
        params: &BarParams,
    ) -> GatewayResult<Vec<tektii_gateway_core::models::Bar>> {
        // Build query parameters for engine request
        let count = params.limit.unwrap_or(100);
        let timeframe_str = format!("{}", params.timeframe);

        let start = Instant::now();
        let response = self
            .client
            .get(format!("{}/api/v1/bars", self.rest_url))
            .query(&[
                ("symbol", symbol),
                ("timeframe", &timeframe_str),
                ("count", &count.to_string()),
            ])
            .send()
            .await
            .map_err(|e| Self::provider_error(format!("Request failed: {e}")))?;

        debug!(
            downstream_ms = start.elapsed().as_secs_f64() * 1000.0,
            "Engine API call completed"
        );

        // Handle specific error responses
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::SymbolNotFound {
                symbol: symbol.to_string(),
            });
        }

        let engine_response: engine_types::BarsResponse = Self::handle_response(response).await?;

        // Convert engine bars to gateway bars
        let bars: GatewayResult<Vec<tektii_gateway_core::models::Bar>> = engine_response
            .bars
            .into_iter()
            .map(|engine_bar| {
                let timestamp = i64::try_from(engine_bar.timestamp)
                    .ok()
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .ok_or_else(|| {
                        warn!(
                            timestamp = engine_bar.timestamp,
                            "Invalid bar timestamp from engine"
                        );
                        GatewayError::internal(format!(
                            "Invalid bar timestamp: {}",
                            engine_bar.timestamp
                        ))
                    })?;

                Ok(tektii_gateway_core::models::Bar {
                    symbol: symbol.to_string(),
                    provider: "tektii".to_string(),
                    timeframe: params.timeframe,
                    timestamp,
                    open: engine_bar.open,
                    high: engine_bar.high,
                    low: engine_bar.low,
                    close: engine_bar.close,
                    volume: rust_decimal::Decimal::from_f64(engine_bar.volume).unwrap_or_default(),
                })
            })
            .collect();

        bars
    }

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Ok(self.capabilities.capabilities())
    }

    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        // Simple health check - try to reach the engine
        let start = Instant::now();
        let response = self
            .client
            .get(format!("{}/health", self.rest_url))
            .send()
            .await;

        let latency = u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
        let is_connected = response.is_ok();

        Ok(ConnectionStatus {
            connected: is_connected,
            last_heartbeat: chrono::Utc::now(),
            latency_ms: if is_connected { latency } else { 0 },
        })
    }
}
