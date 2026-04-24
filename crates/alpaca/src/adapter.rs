//! Alpaca adapter for stock and crypto trading.
//!
//! This module implements the `TradingAdapter` trait for Alpaca's Trading API,
//! providing unified access to stock and cryptocurrency markets.

use super::types::{
    AlpacaAccount, AlpacaActivity, AlpacaBar, AlpacaBarsResponse, AlpacaCryptoBarsResponse,
    AlpacaCryptoQuotesResponse, AlpacaModifyOrderRequest, AlpacaOrder, AlpacaOrderRequest,
    AlpacaPosition, AlpacaQuoteResponse, AlpacaStopLoss, AlpacaTakeProfit,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, error, info, instrument, warn};

use secrecy::{ExposeSecret, SecretBox};
use tektii_gateway_core::models::OrderRequest;

use tektii_gateway_core::circuit_breaker::{
    AdapterCircuitBreaker, CircuitBreakerSnapshot, is_outage_error,
};
use tektii_gateway_core::error::{GatewayError, GatewayResult, reject_codes};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::{ExitHandler, ExitHandlerConfig, ExitHandling};
use tektii_gateway_core::http::{RetryConfig, execute_with_retry, retry_with_backoff};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::WsMessage;

use crate::capabilities::{AlpacaCapabilities, is_crypto_symbol};
use crate::credentials::AlpacaCredentials;

// Import trading models
use tektii_gateway_core::adapter::{ProviderCapabilities, TradingAdapter};
use tektii_gateway_core::models::{
    Account, Bar, BarParams, CancelAllResult, CancelOrderResult, Capabilities,
    ClosePositionRequest, ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order,
    OrderHandle, OrderQueryParams, Position, Quote, Trade, TradeQueryParams, TradingPlatform,
};

/// Alpaca data API base URL.
const ALPACA_DATA_URL: &str = "https://data.alpaca.markets";

/// Alpaca adapter implementation
pub struct AlpacaAdapter {
    /// HTTP client with connection pooling
    client: Client,
    /// Base URL for trading endpoints (account, orders, etc.)
    base_url: String,
    /// Base URL for market data endpoints (quotes, bars)
    data_url: String,
    /// Provider capabilities for determining order type support
    capabilities: AlpacaCapabilities,
    /// API key for authentication
    api_key: Arc<SecretBox<String>>,
    /// API secret for authentication
    api_secret: Arc<SecretBox<String>>,
    /// Retry configuration for transient failure handling
    retry_config: RetryConfig,

    // === Event Infrastructure ===
    /// State Manager for caching orders and positions.
    state_manager: Arc<StateManager>,

    /// Exit Handler for managing stop-loss and take-profit orders.
    exit_handler: Arc<ExitHandler>,

    /// Event Router for processing WebSocket events.
    event_router: Arc<EventRouter>,

    /// Provider identifier for this adapter instance.
    platform: TradingPlatform,

    /// Circuit breaker for detecting provider outages.
    circuit_breaker: Arc<RwLock<AdapterCircuitBreaker>>,

    /// Data feed for market data requests (`"iex"` free tier, `"sip"` paid).
    feed: String,
}

impl AlpacaAdapter {
    /// Create a new Alpaca adapter with full event infrastructure.
    ///
    /// # Arguments
    ///
    /// * `credentials` - Alpaca API credentials
    /// * `broadcaster` - Broadcast channel for sending events to strategies
    /// * `platform` - Provider identifier (e.g., `AlpacaPaper`, `AlpacaLive`)
    ///
    /// # Errors
    ///
    /// Returns `reqwest::Error` if the HTTP client fails to build.
    pub fn new(
        credentials: &AlpacaCredentials,
        broadcaster: broadcast::Sender<WsMessage>,
        platform: TradingPlatform,
    ) -> Result<Self, reqwest::Error> {
        // Resolve base URL: credentials override > provider-derived
        let base_url = credentials
            .base_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::AlpacaLive => "https://api.alpaca.markets".to_string(),
                _ => "https://paper-api.alpaca.markets".to_string(),
            });

        // For testing (custom base_url), use same URL for data
        // For production, use the dedicated data URL
        let data_url = if credentials.base_url.is_some() {
            base_url.clone()
        } else {
            ALPACA_DATA_URL.to_string()
        };

        // Configure client with connection pooling for optimal performance
        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;

        // Create shared StateManager
        let state_manager = Arc::new(StateManager::new());

        // Create ExitHandler (customize via with_exit_handler if needed)
        let exit_handler = Arc::new(ExitHandler::with_defaults(
            Arc::clone(&state_manager),
            platform,
        ));

        // Create EventRouter with references to other components
        let event_router = Arc::new(EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler) as Arc<dyn ExitHandling>,
            broadcaster,
            platform,
        ));

        // Create circuit breaker (3 failures in 5 minutes trips the breaker)
        let circuit_breaker = Arc::new(RwLock::new(AdapterCircuitBreaker::new(
            3,
            Duration::from_secs(300),
            "alpaca",
        )));

        let feed = credentials
            .feed
            .clone()
            .unwrap_or_else(|| "iex".to_string());

        Ok(Self {
            client,
            base_url,
            data_url,
            capabilities: AlpacaCapabilities::new(),
            api_key: tektii_gateway_core::arc_secret(&credentials.api_key),
            api_secret: tektii_gateway_core::arc_secret(&credentials.api_secret),
            retry_config: RetryConfig::default(),
            state_manager,
            exit_handler,
            event_router,
            platform,
            circuit_breaker,
            feed,
        })
    }

    /// Override the retry configuration (useful for tests).
    #[must_use]
    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Enable custom `ExitHandler` configuration.
    #[must_use]
    #[allow(dead_code)] // Public API - not yet used by callers
    pub fn with_exit_handler(mut self, config: ExitHandlerConfig) -> Self {
        // Create new ExitHandler with custom config
        let exit_handler = Arc::new(ExitHandler::new(
            Arc::clone(&self.state_manager),
            self.platform,
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
    #[allow(dead_code)] // Public API
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

    /// Returns the Provider identifier for this adapter.
    #[must_use]
    #[allow(dead_code)] // Public API
    pub const fn platform_id(&self) -> TradingPlatform {
        self.platform
    }

    // =========================================================================
    // Circuit Breaker Methods
    // =========================================================================

    /// Check if the circuit breaker is open and return an error if so.
    async fn check_circuit_breaker(&self) -> GatewayResult<()> {
        let breaker = self.circuit_breaker.read().await;
        if breaker.is_open() {
            return Err(breaker.open_error());
        }
        Ok(())
    }

    /// Record a failure if the error indicates a provider outage.
    async fn record_if_outage(&self, error: &GatewayError) {
        if is_outage_error(error) {
            let mut breaker = self.circuit_breaker.write().await;
            breaker.record_failure();
        }
    }

    // =========================================================================
    // Original Methods
    // =========================================================================

    /// Returns a reference to the provider capabilities.
    #[must_use]
    #[allow(dead_code)] // Used in tests; trait method used in production
    pub const fn capabilities_concrete(&self) -> &AlpacaCapabilities {
        &self.capabilities
    }

    /// Get credentials stored in this adapter.
    fn credentials(&self) -> (String, String) {
        (
            self.api_key.expose_secret().clone(),
            self.api_secret.expose_secret().clone(),
        )
    }

    /// Format a crypto symbol for Alpaca's API (BTCUSD -> BTC/USD)
    fn format_crypto_symbol(symbol: &str) -> String {
        for quote in ["USD", "USDT", "USDC", "BTC", "ETH"] {
            if let Some(base) = symbol.strip_suffix(quote)
                && !base.is_empty()
            {
                return format!("{base}/{quote}");
            }
        }
        symbol.to_string()
    }

    /// Slippage percentage for crypto stop-limit orders (2%).
    const CRYPTO_STOP_LIMIT_SLIPPAGE: Decimal = Decimal::from_parts(2, 0, 0, false, 2);

    /// Transform a stop order to `stop_limit` for Alpaca crypto.
    fn transform_crypto_stop_order(
        order_type: &str,
        stop_price: Option<Decimal>,
        limit_price: Option<Decimal>,
        side: &str,
        is_crypto: bool,
    ) -> (String, Option<Decimal>) {
        if !is_crypto {
            return (order_type.to_string(), limit_price);
        }

        // Transform take_profit to limit (it already has limit_price set)
        if order_type == "take_profit" {
            debug!(
                original_type = order_type,
                limit_price = ?limit_price,
                "Transforming crypto take_profit order to limit"
            );
            return ("limit".to_string(), limit_price);
        }

        // Transform stop/stop_loss orders to stop_limit for crypto
        let is_stop_order = order_type == "stop" || order_type == "stop_loss";
        if is_stop_order
            && limit_price.is_none()
            && let Some(trigger) = stop_price
        {
            let derived_limit = if side == "sell" {
                trigger * (Decimal::ONE - Self::CRYPTO_STOP_LIMIT_SLIPPAGE)
            } else {
                trigger * (Decimal::ONE + Self::CRYPTO_STOP_LIMIT_SLIPPAGE)
            };

            debug!(
                original_type = order_type,
                trigger_price = %trigger,
                derived_limit = %derived_limit,
                slippage_pct = %Self::CRYPTO_STOP_LIMIT_SLIPPAGE,
                "Transforming crypto stop order to stop_limit"
            );

            return ("stop_limit".to_string(), Some(derived_limit));
        }

        (order_type.to_string(), limit_price)
    }

    /// Transform a `market_close` order to market for Alpaca crypto.
    fn transform_crypto_market_close_order(order_type: &str, is_crypto: bool) -> String {
        if is_crypto && order_type == "market_close" {
            debug!(
                original_type = order_type,
                "Transforming crypto market_close order to market"
            );
            return "market".to_string();
        }
        order_type.to_string()
    }

    /// Translate an Alpaca position to gateway Position type.
    fn translate_alpaca_position(pos: AlpacaPosition) -> Position {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let quantity = Decimal::from_str(&pos.qty).unwrap_or_default();
        let avg_entry_price = Decimal::from_str(&pos.avg_entry_price).unwrap_or_default();
        let current_price = Decimal::from_str(&pos.current_price).unwrap_or_default();
        let unrealized_pnl = Decimal::from_str(&pos.unrealized_pl).unwrap_or_default();

        // Determine position side based on quantity
        let side = if quantity >= Decimal::ZERO {
            tektii_gateway_core::models::PositionSide::Long
        } else {
            tektii_gateway_core::models::PositionSide::Short
        };

        Position {
            // Alpaca doesn't have position IDs, use symbol as synthetic ID
            id: format!("{}_position", pos.symbol),
            symbol: pos.symbol,
            side,
            quantity: quantity.abs(), // Always positive, side indicates direction
            average_entry_price: avg_entry_price,
            current_price,
            unrealized_pnl,
            realized_pnl: Decimal::ZERO,
            margin_mode: None,
            leverage: None,
            liquidation_price: None,
            opened_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// Translate Alpaca order status string to `OrderStatus`.
    fn translate_order_status(
        status: &str,
    ) -> Result<tektii_gateway_core::models::OrderStatus, GatewayError> {
        match status.to_lowercase().as_str() {
            "new" | "accepted" | "pending_new" => {
                Ok(tektii_gateway_core::models::OrderStatus::Open)
            }
            "partially_filled" => Ok(tektii_gateway_core::models::OrderStatus::PartiallyFilled),
            "filled" => Ok(tektii_gateway_core::models::OrderStatus::Filled),
            "canceled" | "cancelled" => Ok(tektii_gateway_core::models::OrderStatus::Cancelled),
            "expired" => Ok(tektii_gateway_core::models::OrderStatus::Expired),
            "rejected" | "pending_cancel" | "pending_replace" => {
                Ok(tektii_gateway_core::models::OrderStatus::Rejected)
            }
            other => Err(GatewayError::InvalidValue {
                field: "status".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown order status from Alpaca: {other}"),
            }),
        }
    }

    /// Parse a string side to Side enum.
    fn parse_side(side_str: &str) -> Result<tektii_gateway_core::models::Side, GatewayError> {
        match side_str.to_lowercase().as_str() {
            "buy" => Ok(tektii_gateway_core::models::Side::Buy),
            "sell" => Ok(tektii_gateway_core::models::Side::Sell),
            other => Err(GatewayError::InvalidValue {
                field: "side".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown order side from Alpaca: {other}"),
            }),
        }
    }

    /// Parse a string order type to `OrderType` enum.
    fn parse_order_type(
        type_str: &str,
    ) -> Result<tektii_gateway_core::models::OrderType, GatewayError> {
        match type_str.to_lowercase().as_str() {
            "market" => Ok(tektii_gateway_core::models::OrderType::Market),
            "limit" => Ok(tektii_gateway_core::models::OrderType::Limit),
            "stop" => Ok(tektii_gateway_core::models::OrderType::Stop),
            "stop_limit" => Ok(tektii_gateway_core::models::OrderType::StopLimit),
            "trailing_stop" => Ok(tektii_gateway_core::models::OrderType::TrailingStop),
            other => Err(GatewayError::InvalidValue {
                field: "order_type".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown order type from Alpaca: {other}"),
            }),
        }
    }

    /// Convert Side to string.
    fn side_to_string(side: tektii_gateway_core::models::Side) -> String {
        match side {
            tektii_gateway_core::models::Side::Buy => "buy".to_string(),
            tektii_gateway_core::models::Side::Sell => "sell".to_string(),
        }
    }

    /// Convert `OrderType` to string.
    fn order_type_to_string(order_type: tektii_gateway_core::models::OrderType) -> String {
        match order_type {
            tektii_gateway_core::models::OrderType::Market => "market".to_string(),
            tektii_gateway_core::models::OrderType::Limit => "limit".to_string(),
            tektii_gateway_core::models::OrderType::Stop => "stop".to_string(),
            tektii_gateway_core::models::OrderType::StopLimit => "stop_limit".to_string(),
            tektii_gateway_core::models::OrderType::TrailingStop => "trailing_stop".to_string(),
        }
    }

    /// Convert `TimeInForce` to string.
    fn time_in_force_to_string(tif: tektii_gateway_core::models::TimeInForce) -> String {
        match tif {
            tektii_gateway_core::models::TimeInForce::Gtc => "gtc".to_string(),
            tektii_gateway_core::models::TimeInForce::Day => "day".to_string(),
            tektii_gateway_core::models::TimeInForce::Ioc => "ioc".to_string(),
            tektii_gateway_core::models::TimeInForce::Fok => "fok".to_string(),
        }
    }

    /// Convert `OrderStatus` enum to Alpaca's string representation.
    #[allow(dead_code)]
    fn order_status_to_string(status: tektii_gateway_core::models::OrderStatus) -> String {
        match status {
            tektii_gateway_core::models::OrderStatus::Pending => "pending_new".to_string(),
            tektii_gateway_core::models::OrderStatus::PendingCancel => "pending_cancel".to_string(),
            tektii_gateway_core::models::OrderStatus::Open => "new".to_string(),
            tektii_gateway_core::models::OrderStatus::PartiallyFilled => {
                "partially_filled".to_string()
            }
            tektii_gateway_core::models::OrderStatus::Filled => "filled".to_string(),
            tektii_gateway_core::models::OrderStatus::Cancelled => "canceled".to_string(),
            tektii_gateway_core::models::OrderStatus::Rejected => "rejected".to_string(),
            tektii_gateway_core::models::OrderStatus::Expired => "expired".to_string(),
        }
    }

    /// Parse string `TimeInForce` to `TimeInForce` enum.
    fn parse_time_in_force(
        tif_str: &str,
    ) -> Result<tektii_gateway_core::models::TimeInForce, GatewayError> {
        match tif_str.to_lowercase().as_str() {
            "gtc" => Ok(tektii_gateway_core::models::TimeInForce::Gtc),
            "day" => Ok(tektii_gateway_core::models::TimeInForce::Day),
            "ioc" => Ok(tektii_gateway_core::models::TimeInForce::Ioc),
            "fok" => Ok(tektii_gateway_core::models::TimeInForce::Fok),
            other => Err(GatewayError::InvalidValue {
                field: "time_in_force".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown time in force from Alpaca: {other}"),
            }),
        }
    }

    /// Translate an `AlpacaOrder` to a gateway Order.
    fn translate_alpaca_order(alpaca_order: AlpacaOrder) -> Result<Order, GatewayError> {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let qty = Decimal::from_str(&alpaca_order.qty).unwrap_or_default();
        let filled_qty = Decimal::from_str(&alpaca_order.filled_qty).unwrap_or_default();

        let created_at = DateTime::parse_from_rfc3339(&alpaca_order.created_at)
            .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));
        let updated_at = DateTime::parse_from_rfc3339(&alpaca_order.updated_at)
            .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

        // Parse limit and stop prices
        let limit_price = alpaca_order
            .limit_price
            .as_ref()
            .and_then(|s| Decimal::from_str(s).ok());
        let stop_price = alpaca_order
            .stop_price
            .as_ref()
            .and_then(|s| Decimal::from_str(s).ok());

        Ok(Order {
            id: alpaca_order.id,
            client_order_id: alpaca_order.client_order_id,
            symbol: alpaca_order.symbol,
            side: Self::parse_side(&alpaca_order.side)?,
            order_type: Self::parse_order_type(&alpaca_order.order_type)?,
            time_in_force: Self::parse_time_in_force(&alpaca_order.time_in_force)?,
            quantity: qty,
            filled_quantity: filled_qty,
            remaining_quantity: qty - filled_qty,
            limit_price,
            stop_price,
            average_fill_price: alpaca_order
                .filled_avg_price
                .and_then(|s| Decimal::from_str(&s).ok()),
            status: Self::translate_order_status(&alpaca_order.status)?,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            reject_reason: None,
            position_id: None,
            reduce_only: None,
            post_only: None,
            hidden: None,
            display_quantity: None,
            oco_group_id: None,
            correlation_id: None,
            created_at,
            updated_at,
        })
    }

    /// Map HTTP status code and body to `GatewayError`.
    fn map_http_error(
        status: reqwest::StatusCode,
        body: &serde_json::Value,
        retry_after_header: Option<u64>,
    ) -> GatewayError {
        let message = body
            .get("message")
            .or_else(|| body.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();

        match status.as_u16() {
            429 => GatewayError::RateLimited {
                retry_after_seconds: retry_after_header
                    .or_else(|| body.get("retry_after").and_then(serde_json::Value::as_u64)),
                reset_at: None,
            },

            503 => GatewayError::ProviderUnavailable { message },

            500 | 502 | 504 => GatewayError::ProviderError {
                message,
                provider: Some("alpaca".to_string()),
                source: None,
            },

            400 => GatewayError::InvalidRequest {
                message,
                field: None,
            },

            401 | 403 => GatewayError::Unauthorized {
                reason: message,
                code: "AUTH_FAILED".to_string(),
            },

            // 404 from Alpaca data endpoints means the resource (symbol, etc.) wasn't found.
            // Order/position 404s are handled before map_http_error is called.
            // This must NOT be ProviderError — client errors shouldn't trip the circuit breaker.
            404 => GatewayError::InvalidRequest {
                message: format!("Not found: {message}"),
                field: None,
            },

            422 => {
                let raw_code = body.get("code").and_then(|v| v.as_str()).unwrap_or("");
                let reject_code = Some(map_alpaca_reject_code(raw_code).to_string());
                GatewayError::OrderRejected {
                    reason: message,
                    reject_code,
                    details: Some(body.clone()),
                }
            }

            // Server errors → ProviderError (counts toward circuit breaker)
            s if s >= 500 => GatewayError::ProviderError {
                message: format!("HTTP {s}: {message}"),
                provider: Some("alpaca".to_string()),
                source: None,
            },

            // Remaining client errors → InvalidRequest (does NOT trip circuit breaker)
            s => GatewayError::InvalidRequest {
                message: format!("HTTP {s}: {message}"),
                field: None,
            },
        }
    }
}

/// Check if a string looks like a valid UUID (Alpaca order IDs are UUIDs).
///
/// Rejects non-UUID strings early so Alpaca doesn't return misleading 422 errors.
fn is_valid_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.bytes().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// Map an Alpaca error `code` field to a canonical reject code.
///
/// Known Alpaca codes are mapped to `reject_codes::*` constants.
/// Unknown codes pass through as-is so clients still see the raw value.
fn map_alpaca_reject_code(raw: &str) -> &str {
    match raw {
        "insufficient_buying_power" | "forbidden" => reject_codes::INSUFFICIENT_FUNDS,
        "market_closed_error" => reject_codes::MARKET_CLOSED,
        "invalid_qty" => reject_codes::INVALID_QUANTITY,
        "symbol_not_found" => reject_codes::INVALID_SYMBOL,
        other => {
            if other.is_empty() {
                "ORDER_REJECTED"
            } else {
                other
            }
        }
    }
}

#[async_trait]
impl TradingAdapter for AlpacaAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &self.capabilities
    }

    fn platform(&self) -> TradingPlatform {
        self.platform
    }

    fn provider_name(&self) -> &'static str {
        "alpaca"
    }

    #[instrument(skip(self), name = "alpaca_get_account")]
    async fn get_account(&self) -> GatewayResult<Account> {
        self.check_circuit_breaker().await?;

        let (api_key, api_secret) = self.credentials();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let result: GatewayResult<AlpacaAccount> = retry_with_backoff(&self.retry_config, || {
            let client = client.clone();
            let base_url = base_url.clone();
            let api_key = api_key.clone();
            let api_secret = api_secret.clone();

            async move {
                let downstream_start = Instant::now();
                let response = client
                    .get(format!("{base_url}/v2/account"))
                    .header("APCA-API-KEY-ID", api_key)
                    .header("APCA-API-SECRET-KEY", api_secret)
                    .send()
                    .await
                    .map_err(|e| GatewayError::ProviderError {
                        message: format!("Request failed: {e}"),
                        provider: Some("alpaca".to_string()),
                        source: None,
                    })?;
                let downstream_time = downstream_start.elapsed();
                debug!(
                    downstream_ms = downstream_time.as_secs_f64() * 1000.0,
                    "Downstream API call completed"
                );

                let status = response.status();
                if !status.is_success() {
                    let retry_after_header = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());

                    let body = response
                        .json::<serde_json::Value>()
                        .await
                        .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

                    return Err(Self::map_http_error(status, &body, retry_after_header));
                }

                response
                    .json::<AlpacaAccount>()
                    .await
                    .map_err(|e| GatewayError::internal(format!("Failed to parse response: {e}")))
            }
        })
        .await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let alpaca_account = result?;

        use rust_decimal::Decimal;
        use std::str::FromStr;

        let account = Account {
            balance: Decimal::from_str(&alpaca_account.cash).unwrap_or_default(),
            equity: Decimal::from_str(&alpaca_account.portfolio_value).unwrap_or_default(),
            margin_used: Decimal::ZERO,
            margin_available: Decimal::from_str(&alpaca_account.buying_power).unwrap_or_default(),
            unrealized_pnl: Decimal::ZERO,
            currency: alpaca_account.currency,
        };

        Ok(account)
    }

    async fn submit_order(&self, request: &OrderRequest) -> GatewayResult<OrderHandle> {
        self.check_circuit_breaker().await?;

        let oco_group_id = request.oco_group_id.clone();

        let result = self.place_order_internal(request).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let mut order = result?;

        order.oco_group_id.clone_from(&oco_group_id);

        if let Some(group_id) = &oco_group_id {
            self.state_manager.add_to_oco_group(&order.id, group_id);
        }

        Ok(OrderHandle {
            id: order.id,
            client_order_id: order.client_order_id,
            correlation_id: None,
            status: order.status,
        })
    }

    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        if !is_valid_uuid(order_id) {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }

        self.check_circuit_breaker().await?;

        let (api_key, api_secret) = self.credentials();

        let downstream_start = Instant::now();
        let result: GatewayResult<_> = async {
            let response = self
                .client
                .get(format!("{}/v2/orders/{}", self.base_url, order_id))
                .header("APCA-API-KEY-ID", api_key)
                .header("APCA-API-SECRET-KEY", api_secret)
                .send()
                .await
                .map_err(|e| GatewayError::ProviderError {
                    message: format!("Request failed: {e}"),
                    provider: Some("alpaca".to_string()),
                    source: None,
                })?;
            let downstream_time = downstream_start.elapsed();
            debug!(
                downstream_ms = downstream_time.as_secs_f64() * 1000.0,
                "Downstream API call completed"
            );

            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(GatewayError::OrderNotFound {
                    id: order_id.to_string(),
                });
            }
            if !status.is_success() {
                let retry_after_header = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());

                let body = response
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

                return Err(Self::map_http_error(status, &body, retry_after_header));
            }

            response
                .json::<AlpacaOrder>()
                .await
                .map_err(|e| GatewayError::internal(format!("Failed to parse response: {e}")))
        }
        .await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let alpaca_order = result?;

        let mut order = Self::translate_alpaca_order(alpaca_order)?;

        // Enrich with oco_group_id from StateManager
        order.oco_group_id = self.state_manager.get_oco_group_id(&order.id);

        Ok(order)
    }

    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.check_circuit_breaker().await?;

        let result = self.get_orders_internal(params).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        result
    }

    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.get_order_history_internal(params).await
    }

    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        self.modify_order_internal(order_id, request).await
    }

    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        self.cancel_order_internal(order_id).await?;
        let order = self.get_order(order_id).await?;
        Ok(CancelOrderResult {
            success: true,
            order,
        })
    }

    async fn cancel_all_orders(&self, symbol: Option<&str>) -> GatewayResult<CancelAllResult> {
        self.cancel_all_orders_internal(symbol).await
    }

    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        let response = self.get_trades_internal(params.symbol.as_deref()).await?;

        Ok(response)
    }

    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
        self.check_circuit_breaker().await?;

        if let Some(sym) = symbol {
            let pos = self.get_position(sym).await?;
            Ok(vec![pos])
        } else {
            let (api_key, api_secret) = self.credentials();

            let downstream_start = Instant::now();
            let result: GatewayResult<Vec<AlpacaPosition>> = async {
                let response = self
                    .client
                    .get(format!("{}/v2/positions", self.base_url))
                    .header("APCA-API-KEY-ID", api_key)
                    .header("APCA-API-SECRET-KEY", api_secret)
                    .send()
                    .await
                    .map_err(|e| GatewayError::ProviderError {
                        message: format!("Request failed: {e}"),
                        provider: Some("alpaca".to_string()),
                        source: None,
                    })?;
                let downstream_time = downstream_start.elapsed();
                debug!(
                    downstream_ms = downstream_time.as_secs_f64() * 1000.0,
                    "Downstream API call completed"
                );

                let status = response.status();
                if !status.is_success() {
                    let retry_after_header = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());

                    let body = response
                        .json::<serde_json::Value>()
                        .await
                        .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

                    return Err(Self::map_http_error(status, &body, retry_after_header));
                }

                response
                    .json()
                    .await
                    .map_err(|e| GatewayError::internal(format!("Failed to parse positions: {e}")))
            }
            .await;

            if let Err(ref error) = result {
                self.record_if_outage(error).await;
            }

            let alpaca_positions = result?;

            let positions: Vec<Position> = alpaca_positions
                .into_iter()
                .map(Self::translate_alpaca_position)
                .collect();

            Ok(positions)
        }
    }

    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        self.check_circuit_breaker().await?;

        // Strip synthetic _position suffix if present (list_positions returns "{symbol}_position")
        let alpaca_symbol = position_id.strip_suffix("_position").unwrap_or(position_id);
        let (api_key, api_secret) = self.credentials();

        let downstream_start = Instant::now();
        let result: GatewayResult<AlpacaPosition> = async {
            let response = self
                .client
                .get(format!("{}/v2/positions/{}", self.base_url, alpaca_symbol))
                .header("APCA-API-KEY-ID", api_key)
                .header("APCA-API-SECRET-KEY", api_secret)
                .send()
                .await
                .map_err(|e| GatewayError::ProviderError {
                    message: format!("Request failed: {e}"),
                    provider: Some("alpaca".to_string()),
                    source: None,
                })?;
            let downstream_time = downstream_start.elapsed();
            debug!(
                downstream_ms = downstream_time.as_secs_f64() * 1000.0,
                "Downstream API call completed"
            );

            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(GatewayError::PositionNotFound {
                    id: position_id.to_string(),
                });
            }
            if !status.is_success() {
                let retry_after_header = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());

                let body = response
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

                return Err(Self::map_http_error(status, &body, retry_after_header));
            }

            response
                .json()
                .await
                .map_err(|e| GatewayError::internal(format!("Failed to parse position: {e}")))
        }
        .await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        Ok(Self::translate_alpaca_position(result?))
    }

    async fn close_position(
        &self,
        position_id: &str,
        request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        let (api_key, api_secret) = self.credentials();

        // Strip synthetic _position suffix if present (list_positions returns "{symbol}_position")
        let alpaca_symbol = position_id.strip_suffix("_position").unwrap_or(position_id);

        let url = if let Some(qty) = request.quantity {
            format!(
                "{}/v2/positions/{}?qty={}",
                self.base_url, alpaca_symbol, qty
            )
        } else {
            format!("{}/v2/positions/{}", self.base_url, alpaca_symbol)
        };

        let downstream_start = Instant::now();
        let response = self
            .client
            .delete(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::PositionNotFound {
                id: position_id.to_string(),
            });
        }
        if !status.is_success() {
            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));
            return Err(GatewayError::ProviderError {
                message: body.to_string(),
                provider: Some("alpaca".to_string()),
                source: None,
            });
        }

        let alpaca_order: AlpacaOrder = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse order: {e}")))?;

        Ok(OrderHandle {
            id: alpaca_order.id,
            client_order_id: None,
            correlation_id: None,
            status: Self::translate_order_status(&alpaca_order.status)?,
        })
    }

    async fn close_all_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<OrderHandle>> {
        let (api_key, api_secret) = self.credentials();

        if let Some(sym) = symbol {
            let handle = self
                .close_position(
                    sym,
                    &ClosePositionRequest {
                        quantity: None,
                        order_type: None,
                        limit_price: None,
                        cancel_associated_orders: true,
                    },
                )
                .await?;
            return Ok(vec![handle]);
        }

        let downstream_start = Instant::now();
        let response = self
            .client
            .delete(format!("{}/v2/positions", self.base_url))
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if !status.is_success() {
            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));
            return Err(GatewayError::ProviderError {
                message: body.to_string(),
                provider: Some("alpaca".to_string()),
                source: None,
            });
        }

        let alpaca_orders: Vec<AlpacaOrder> = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse orders: {e}")))?;

        let orders: Vec<OrderHandle> = alpaca_orders
            .into_iter()
            .map(|order| {
                Ok(OrderHandle {
                    id: order.id,
                    client_order_id: None,
                    correlation_id: None,
                    status: Self::translate_order_status(&order.status)?,
                })
            })
            .collect::<GatewayResult<Vec<_>>>()?;

        info!(closed_count = orders.len(), "Closed all positions");

        Ok(orders)
    }

    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        self.check_circuit_breaker().await?;

        let result = self.get_quote_internal(symbol).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        result
    }

    async fn get_bars(&self, symbol: &str, params: &BarParams) -> GatewayResult<Vec<Bar>> {
        self.get_bars_internal(symbol, params).await
    }

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Ok(ProviderCapabilities::capabilities(&self.capabilities))
    }

    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        let start = Instant::now();
        let result = self.get_account().await;
        let latency = start.elapsed();

        match result {
            Ok(_) => Ok(ConnectionStatus {
                connected: true,
                latency_ms: u32::try_from(latency.as_millis()).unwrap_or(u32::MAX),
                last_heartbeat: chrono::Utc::now(),
            }),
            Err(ref e) => {
                tracing::warn!(error = %e, "Alpaca connection check failed");
                Ok(ConnectionStatus {
                    connected: false,
                    latency_ms: u32::try_from(latency.as_millis()).unwrap_or(u32::MAX),
                    last_heartbeat: chrono::Utc::now(),
                })
            }
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

/// Pre-translated parameters for building an Alpaca order request.
struct AlpacaTranslatedParams {
    symbol: String,
    side: String,
    /// The (possibly crypto-transformed) order type.
    order_type: String,
    /// The original order type string (before crypto transformation).
    original_order_type: String,
    time_in_force: String,
    limit_price: Option<Decimal>,
    stop_price: Option<Decimal>,
    use_pending_system: bool,
    has_sl_tp: bool,
}

// Internal methods that maintain existing proxy logic
impl AlpacaAdapter {
    #[instrument(skip(self, order), fields(symbol = %order.symbol, side = ?order.side, quantity = ?order.quantity), name = "alpaca_place_order")]
    async fn place_order_internal(&self, order: &OrderRequest) -> GatewayResult<Order> {
        let (api_key, api_secret) = self.credentials();

        let translation_start = Instant::now();

        let order_type_str = Self::order_type_to_string(order.order_type);
        let side_str = Self::side_to_string(order.side);
        let time_in_force_str = Self::time_in_force_to_string(order.time_in_force);

        // Pass symbol through directly (no SymbolMapper)
        let alpaca_symbol = order.symbol.clone();

        let is_crypto = is_crypto_symbol(&order.symbol);

        let (order_type, limit_price) = Self::transform_crypto_stop_order(
            &order_type_str,
            order.stop_price,
            order.limit_price,
            &side_str,
            is_crypto,
        );

        let order_type = Self::transform_crypto_market_close_order(&order_type, is_crypto);

        let has_sl_tp = order.stop_loss.is_some() || order.take_profit.is_some();
        let supports_bracket = self.capabilities.supports_bracket_orders(&order.symbol);
        let use_pending_system = has_sl_tp && !supports_bracket;

        if use_pending_system {
            info!(
                symbol = %order.symbol,
                has_sl = order.stop_loss.is_some(),
                has_tp = order.take_profit.is_some(),
                "Crypto order with SL/TP detected - using pending system instead of native bracket"
            );
        }

        let translated = AlpacaTranslatedParams {
            symbol: alpaca_symbol,
            side: side_str.clone(),
            order_type,
            original_order_type: order_type_str.clone(),
            time_in_force: time_in_force_str.clone(),
            limit_price,
            stop_price: order.stop_price,
            use_pending_system,
            has_sl_tp,
        };
        let alpaca_order_request = Self::build_alpaca_order_request(order, &translated);

        let translation_time = translation_start.elapsed();
        debug!(
            translation_ms = translation_time.as_secs_f64() * 1000.0,
            has_sl_tp = has_sl_tp,
            supports_bracket = supports_bracket,
            use_pending_system = use_pending_system,
            "Request translation completed"
        );

        let client = self.client.clone();
        let url = format!("{}/v2/orders", self.base_url);
        let response = execute_with_retry(
            || {
                let client = client.clone();
                let url = url.clone();
                let api_key = api_key.clone();
                let api_secret = api_secret.clone();
                let body = alpaca_order_request.clone();
                async move {
                    client
                        .post(&url)
                        .header("APCA-API-KEY-ID", &api_key)
                        .header("APCA-API-SECRET-KEY", &api_secret)
                        .json(&body)
                }
            },
            "alpaca",
            Some(&self.retry_config),
            None,
        )
        .await?;

        let alpaca_order: AlpacaOrder = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse response: {e}")))?;

        Self::translate_alpaca_order(alpaca_order)
            .map_err(|e| GatewayError::internal(format!("Failed to translate order: {e}")))
    }

    /// Build an `AlpacaOrderRequest` from gateway order parameters.
    ///
    /// Selects the appropriate order class (bracket vs plain) based on whether
    /// SL/TP legs are present and whether the symbol supports native brackets.
    fn build_alpaca_order_request(
        order: &OrderRequest,
        translated: &AlpacaTranslatedParams,
    ) -> AlpacaOrderRequest {
        if translated.use_pending_system {
            AlpacaOrderRequest {
                symbol: translated.symbol.clone(),
                qty: order.quantity.to_string(),
                side: translated.side.clone(),
                order_type: translated.order_type.clone(),
                time_in_force: translated.time_in_force.clone(),
                limit_price: translated.limit_price.map(|p| p.to_string()),
                stop_price: translated.stop_price.map(|p| p.to_string()),
                order_class: None,
                stop_loss: None,
                take_profit: None,
                client_order_id: order.client_order_id.clone(),
            }
        } else if translated.has_sl_tp {
            AlpacaOrderRequest {
                symbol: translated.symbol.clone(),
                qty: order.quantity.to_string(),
                side: translated.side.clone(),
                order_type: translated.original_order_type.clone(),
                time_in_force: translated.time_in_force.clone(),
                limit_price: order.limit_price.map(|p| p.to_string()),
                stop_price: order.stop_price.map(|p| p.to_string()),
                order_class: Some("bracket".to_string()),
                stop_loss: order.stop_loss.map(|price| AlpacaStopLoss {
                    stop_price: price.to_string(),
                }),
                take_profit: order.take_profit.map(|price| AlpacaTakeProfit {
                    limit_price: price.to_string(),
                }),
                client_order_id: order.client_order_id.clone(),
            }
        } else {
            AlpacaOrderRequest {
                symbol: translated.symbol.clone(),
                qty: order.quantity.to_string(),
                side: translated.side.clone(),
                order_type: translated.order_type.clone(),
                time_in_force: translated.time_in_force.clone(),
                limit_price: translated.limit_price.map(|p| p.to_string()),
                stop_price: translated.stop_price.map(|p| p.to_string()),
                order_class: None,
                stop_loss: None,
                take_profit: None,
                client_order_id: order.client_order_id.clone(),
            }
        }
    }
    async fn get_orders_internal(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        let (api_key, api_secret) = self.credentials();

        let translation_start = Instant::now();
        let mut url = String::with_capacity(self.base_url.len() + 128);
        url.push_str(&self.base_url);
        url.push_str("/v2/orders");

        let mut first_param = true;

        if let Some(statuses) = &params.status {
            let alpaca_status = if statuses.iter().any(|s| {
                matches!(
                    s,
                    tektii_gateway_core::models::OrderStatus::Pending
                        | tektii_gateway_core::models::OrderStatus::Open
                        | tektii_gateway_core::models::OrderStatus::PartiallyFilled
                )
            }) {
                "open"
            } else {
                "closed"
            };
            url.push(if first_param { '?' } else { '&' });
            url.push_str("status=");
            url.push_str(alpaca_status);
            first_param = false;
        }
        if let Some(symbol) = &params.symbol {
            url.push(if first_param { '?' } else { '&' });
            url.push_str("symbols=");
            url.push_str(symbol);
            first_param = false;
        }
        if let Some(limit) = params.limit {
            url.push(if first_param { '?' } else { '&' });
            url.push_str(&format!("limit={limit}"));
            first_param = false;
        }
        if let Some(since) = &params.since {
            url.push(if first_param { '?' } else { '&' });
            url.push_str(&format!("after={}", since.to_rfc3339()));
            first_param = false;
        }
        if let Some(until) = &params.until {
            url.push(if first_param { '?' } else { '&' });
            url.push_str(&format!("until={}", until.to_rfc3339()));
        }

        let translation_time = translation_start.elapsed();
        debug!(
            translation_ms = translation_time.as_secs_f64() * 1000.0,
            "Request translation completed"
        );

        let downstream_start = Instant::now();
        let response = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if !status.is_success() {
            let retry_after_header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(status, &body, retry_after_header));
        }

        let alpaca_orders: Vec<AlpacaOrder> = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse response: {e}")))?;

        let mut orders = Vec::with_capacity(alpaca_orders.len());
        for alpaca_order in alpaca_orders {
            match Self::translate_alpaca_order(alpaca_order) {
                Ok(order) => orders.push(order),
                Err(e) => {
                    warn!(error = ?e, "Failed to translate order, skipping");
                }
            }
        }

        Ok(orders)
    }

    async fn cancel_order_internal(&self, order_id: &str) -> GatewayResult<()> {
        if !is_valid_uuid(order_id) {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }

        let (api_key, api_secret) = self.credentials();

        let downstream_start = Instant::now();
        let response = self
            .client
            .delete(format!("{}/v2/orders/{}", self.base_url, order_id))
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if !status.is_success() {
            let retry_after_header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(status, &body, retry_after_header));
        }

        Ok(())
    }

    #[instrument(skip(self, request), fields(order_id = %order_id), name = "alpaca_modify_order")]
    async fn modify_order_internal(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        if !is_valid_uuid(order_id) {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }

        let (api_key, api_secret) = self.credentials();

        let alpaca_request = AlpacaModifyOrderRequest {
            qty: request.quantity.map(|q| q.to_string()),
            time_in_force: None,
            limit_price: request.limit_price.map(|p| p.to_string()),
            stop_price: request.stop_price.map(|p| p.to_string()),
            client_order_id: None,
        };

        debug!(?alpaca_request, "Sending PATCH request to modify order");

        let downstream_start = Instant::now();
        let response = self
            .client
            .patch(format!("{}/v2/orders/{}", self.base_url, order_id))
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .json(&alpaca_request)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }
        if !status.is_success() {
            let retry_after_header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(status, &body, retry_after_header));
        }

        let alpaca_order: AlpacaOrder = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse response: {e}")))?;

        let order = Self::translate_alpaca_order(alpaca_order)
            .map_err(|e| GatewayError::internal(format!("Failed to translate order: {e}")))?;

        Ok(ModifyOrderResult {
            order,
            previous_order_id: Some(order_id.to_string()),
        })
    }

    async fn get_quote_internal(&self, symbol: &str) -> GatewayResult<Quote> {
        let (api_key, api_secret) = self.credentials();

        let is_crypto = is_crypto_symbol(symbol);

        // Pass symbol through directly (no SymbolMapper)
        let alpaca_symbol = symbol;

        let url = if is_crypto {
            let crypto_symbol = Self::format_crypto_symbol(alpaca_symbol);
            format!(
                "{}/v1beta3/crypto/us/latest/quotes?symbols={}",
                self.data_url, crypto_symbol
            )
        } else {
            format!(
                "{}/v2/stocks/{}/quotes/latest?feed={}",
                self.data_url, alpaca_symbol, self.feed
            )
        };

        let downstream_start = Instant::now();
        let response = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if !status.is_success() {
            let retry_after_header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(status, &body, retry_after_header));
        }

        let alpaca_quote = if is_crypto {
            let crypto_response: AlpacaCryptoQuotesResponse =
                response.json().await.map_err(|e| {
                    GatewayError::internal(format!("Failed to parse crypto response: {e}"))
                })?;

            crypto_response
                .quotes
                .into_values()
                .next()
                .ok_or_else(|| GatewayError::internal("No quote data in response"))?
        } else {
            let stocks_response: AlpacaQuoteResponse = response.json().await.map_err(|e| {
                GatewayError::internal(format!("Failed to parse stocks response: {e}"))
            })?;
            stocks_response.quote
        };

        use rust_decimal::Decimal;

        let timestamp = DateTime::parse_from_rfc3339(&alpaca_quote.timestamp)
            .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

        let bid = Decimal::from_f64_retain(alpaca_quote.bid_price).unwrap_or_default();
        let ask = Decimal::from_f64_retain(alpaca_quote.ask_price).unwrap_or_default();
        let last = (bid + ask) / Decimal::from(2);

        Ok(Quote {
            symbol: symbol.to_string(),
            provider: "alpaca".to_string(),
            bid,
            bid_size: Decimal::from_f64_retain(alpaca_quote.bid_size),
            ask,
            ask_size: Decimal::from_f64_retain(alpaca_quote.ask_size),
            last,
            volume: None,
            timestamp,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn get_bars_internal(&self, symbol: &str, params: &BarParams) -> GatewayResult<Vec<Bar>> {
        let (api_key, api_secret) = self.credentials();

        let is_crypto = is_crypto_symbol(symbol);

        // Pass symbol through directly (no SymbolMapper)
        let alpaca_symbol = symbol;

        let translation_start = Instant::now();
        let mut url = if is_crypto {
            let crypto_symbol = Self::format_crypto_symbol(alpaca_symbol);
            format!(
                "{}/v1beta3/crypto/us/bars?symbols={}",
                self.data_url, crypto_symbol
            )
        } else {
            format!("{}/v2/stocks/{}/bars", self.data_url, alpaca_symbol)
        };

        // Translate timeframe to Alpaca format
        let alpaca_timeframe = match params.timeframe.as_str() {
            "1m" => "1Min",
            "2m" => "2Min",
            "5m" => "5Min",
            "10m" => "10Min",
            "15m" => "15Min",
            "30m" => "30Min",
            "1h" => "1Hour",
            "2h" => "2Hour",
            "4h" => "4Hour",
            "1d" => "1Day",
            "1w" => "1Week",
            "12h" => {
                return Err(GatewayError::InvalidRequest {
                    message: "Alpaca does not support 12h timeframe".to_string(),
                    field: Some("timeframe".to_string()),
                });
            }
            other => {
                return Err(GatewayError::InvalidRequest {
                    message: format!("Unsupported timeframe: {other}"),
                    field: Some("timeframe".to_string()),
                });
            }
        };

        let separator = if is_crypto { "&" } else { "?" };
        url.push_str(&format!("{separator}timeframe={alpaca_timeframe}"));

        // For stocks, specify the data feed (iex = free tier, sip = paid)
        if !is_crypto {
            url.push_str(&format!("&feed={}", self.feed));
        }

        let limit = params.limit.unwrap_or(100);
        url.push_str(&format!("&limit={limit}"));

        if let Some(start) = params.start {
            url.push_str(&format!("&start={}", start.format("%Y-%m-%dT%H:%M:%SZ")));
        }

        if let Some(end) = params.end {
            url.push_str(&format!("&end={}", end.format("%Y-%m-%dT%H:%M:%SZ")));
        }

        let translation_time = translation_start.elapsed();
        debug!(
            translation_ms = translation_time.as_secs_f64() * 1000.0,
            "Request translation completed"
        );

        let downstream_start = Instant::now();
        let response = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if !status.is_success() {
            let retry_after_header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(status, &body, retry_after_header));
        }

        let text = response.text().await.map_err(|e| {
            GatewayError::internal(format!("Failed to read bars response body: {e}"))
        })?;

        let alpaca_bars: Vec<AlpacaBar> = if is_crypto {
            let crypto_response: AlpacaCryptoBarsResponse =
                serde_json::from_str(&text).map_err(|e| {
                    error!(body = %text, "Failed to parse crypto bars response");
                    GatewayError::internal(format!("Failed to parse crypto response: {e}"))
                })?;

            crypto_response
                .bars
                .into_values()
                .next()
                .unwrap_or_default()
        } else {
            let stocks_response: AlpacaBarsResponse = serde_json::from_str(&text).map_err(|e| {
                error!(body = %text, "Failed to parse stocks bars response");
                GatewayError::internal(format!("Failed to parse stocks response: {e}"))
            })?;
            stocks_response.bars
        };

        Ok(Self::translate_alpaca_bars(
            alpaca_bars,
            symbol,
            params.timeframe,
        ))
    }

    /// Translate a list of Alpaca bars into gateway `Bar` models.
    fn translate_alpaca_bars(
        alpaca_bars: Vec<AlpacaBar>,
        symbol: &str,
        timeframe: tektii_gateway_core::models::Timeframe,
    ) -> Vec<Bar> {
        use rust_decimal::Decimal;

        alpaca_bars
            .into_iter()
            .map(|ab| {
                let timestamp = DateTime::parse_from_rfc3339(&ab.timestamp)
                    .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

                Bar {
                    symbol: symbol.to_string(),
                    provider: "alpaca".to_string(),
                    timeframe,
                    timestamp,
                    open: Decimal::from_f64_retain(ab.open).unwrap_or_default(),
                    high: Decimal::from_f64_retain(ab.high).unwrap_or_default(),
                    low: Decimal::from_f64_retain(ab.low).unwrap_or_default(),
                    close: Decimal::from_f64_retain(ab.close).unwrap_or_default(),
                    volume: Decimal::from_f64_retain(ab.volume).unwrap_or_default(),
                }
            })
            .collect()
    }
    async fn get_trades_internal(&self, symbol: Option<&str>) -> GatewayResult<Vec<Trade>> {
        let (api_key, api_secret) = self.credentials();

        // Get trade activities (fills)
        let downstream_start = Instant::now();
        let activities_response = self
            .client
            .get(format!("{}/v2/account/activities/FILL", self.base_url))
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let activities_time = downstream_start.elapsed();
        debug!(
            downstream_activities_ms = activities_time.as_secs_f64() * 1000.0,
            "Downstream API call for activities completed"
        );

        let activities_status = activities_response.status();
        if !activities_status.is_success() {
            let retry_after_header = activities_response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = activities_response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(
                activities_status,
                &body,
                retry_after_header,
            ));
        }

        let activities: Vec<AlpacaActivity> = activities_response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse activities: {e}")))?;

        let filtered_activities: Vec<_> = if let Some(sym) = symbol {
            activities.into_iter().filter(|a| a.symbol == sym).collect()
        } else {
            activities
        };

        use rust_decimal::Decimal;
        use std::str::FromStr;

        let mut trades = Vec::with_capacity(filtered_activities.len());
        for activity in filtered_activities {
            let timestamp = DateTime::parse_from_rfc3339(&activity.transaction_time)
                .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

            let side =
                Self::parse_side(&activity.side).unwrap_or(tektii_gateway_core::models::Side::Buy);

            trades.push(Trade {
                id: activity.id,
                order_id: activity.order_id,
                symbol: activity.symbol,
                side,
                quantity: Decimal::from_str(&activity.qty).unwrap_or_default(),
                price: Decimal::from_str(&activity.price).unwrap_or_default(),
                commission: Decimal::ZERO,
                commission_currency: "USD".to_string(),
                is_maker: None,
                timestamp,
            });
        }

        Ok(trades)
    }

    /// Get order history - historical orders that are no longer open.
    #[instrument(skip(self), name = "alpaca_get_order_history")]
    async fn get_order_history_internal(
        &self,
        params: &OrderQueryParams,
    ) -> GatewayResult<Vec<Order>> {
        let (api_key, api_secret) = self.credentials();

        let translation_start = Instant::now();
        let mut url = String::with_capacity(self.base_url.len() + 128);
        url.push_str(&self.base_url);
        url.push_str("/v2/orders?status=all");

        if let Some(symbol) = &params.symbol {
            url.push_str(&format!("&symbols={symbol}"));
        }

        if let Some(limit) = params.limit {
            url.push_str(&format!("&limit={limit}"));
        }

        if let Some(since) = &params.since {
            url.push_str(&format!("&after={}", since.to_rfc3339()));
        }

        if let Some(until) = &params.until {
            url.push_str(&format!("&until={}", until.to_rfc3339()));
        }

        let translation_time = translation_start.elapsed();
        debug!(
            translation_ms = translation_time.as_secs_f64() * 1000.0,
            "Request translation completed"
        );

        let downstream_start = Instant::now();
        let response = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if !status.is_success() {
            let retry_after_header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(status, &body, retry_after_header));
        }

        let alpaca_orders: Vec<AlpacaOrder> = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse orders: {e}")))?;

        let mut orders = Vec::with_capacity(alpaca_orders.len());
        for alpaca_order in alpaca_orders {
            match Self::translate_alpaca_order(alpaca_order) {
                Ok(order) => orders.push(order),
                Err(e) => {
                    warn!(error = ?e, "Failed to translate order in history, skipping");
                }
            }
        }

        Ok(orders)
    }

    /// Cancel all open orders, optionally filtered by symbol.
    #[instrument(skip(self), fields(symbol = ?symbol), name = "alpaca_cancel_all_orders")]
    async fn cancel_all_orders_internal(
        &self,
        symbol: Option<&str>,
    ) -> GatewayResult<CancelAllResult> {
        let (api_key, api_secret) = self.credentials();

        let mut url = format!("{}/v2/orders", self.base_url);

        if let Some(sym) = symbol {
            url.push_str(&format!("?symbols={sym}"));
        }

        let downstream_start = Instant::now();
        let response = self
            .client
            .delete(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("alpaca".to_string()),
                source: None,
            })?;
        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Downstream API call completed"
        );

        let status = response.status();
        if status.as_u16() != 207 && !status.is_success() {
            let retry_after_header = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"error": "Unknown error"}));

            return Err(Self::map_http_error(status, &body, retry_after_header));
        }

        #[derive(Deserialize)]
        struct CancelResult {
            id: String,
            status: u16,
        }

        let results: Vec<CancelResult> = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse cancel results: {e}")))?;

        let mut cancelled_count = 0u32;
        let mut failed_count = 0u32;
        let mut failed_order_ids = Vec::new();

        for result in results {
            if result.status == 200 {
                cancelled_count += 1;
            } else {
                failed_count += 1;
                failed_order_ids.push(result.id);
            }
        }

        info!(
            cancelled = cancelled_count,
            failed = failed_count,
            "Cancel all orders completed"
        );

        Ok(CancelAllResult {
            cancelled_count,
            failed_count,
            failed_order_ids: if failed_order_ids.is_empty() {
                None
            } else {
                Some(failed_order_ids)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create test credentials
    fn test_credentials() -> AlpacaCredentials {
        AlpacaCredentials::new("test-key", "test-secret")
    }

    #[test]
    fn test_adapter_with_event_infrastructure_creates_components() {
        let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
        let platform = TradingPlatform::AlpacaPaper;

        let adapter = AlpacaAdapter::new(&test_credentials(), broadcaster, platform)
            .expect("Failed to build HTTP client");

        let _state_manager = adapter.state_manager();
        let _event_router = adapter.event_router();
        let _exit_handler = adapter.exit_handler();
    }

    #[test]
    fn test_state_manager_is_functional() {
        let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
        let platform = TradingPlatform::AlpacaPaper;

        let adapter = AlpacaAdapter::new(&test_credentials(), broadcaster, platform)
            .expect("Failed to build HTTP client");

        let state_manager = adapter.state_manager();

        assert_eq!(state_manager.order_count(), 0);
        assert_eq!(state_manager.position_count(), 0);
    }

    #[test]
    fn test_event_router_is_functional() {
        let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
        let platform = TradingPlatform::AlpacaPaper;

        let adapter = AlpacaAdapter::new(&test_credentials(), broadcaster, platform)
            .expect("Failed to build HTTP client");

        let event_router = adapter.event_router();

        assert_eq!(event_router.order_events_processed(), 0);
        assert_eq!(event_router.position_events_processed(), 0);
    }

    #[test]
    fn test_exit_handler_can_be_customized() {
        let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);

        let adapter = AlpacaAdapter::new(
            &test_credentials(),
            broadcaster,
            TradingPlatform::AlpacaPaper,
        )
        .expect("Failed to build HTTP client")
        .with_exit_handler(ExitHandlerConfig::default());

        let exit_handler = adapter.exit_handler();

        assert_eq!(exit_handler.pending_count(), 0);
    }

    #[test]
    fn test_base_url_override_from_credentials() {
        let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);
        let credentials = AlpacaCredentials::new("test-key", "test-secret")
            .with_base_url("http://localhost:8080");

        let adapter = AlpacaAdapter::new(&credentials, broadcaster, TradingPlatform::AlpacaPaper)
            .expect("Failed to build HTTP client");

        let _state_manager = adapter.state_manager();
        let _event_router = adapter.event_router();
    }

    #[test]
    fn test_components_share_state_manager() {
        let (broadcaster, _rx) = broadcast::channel::<WsMessage>(16);

        let adapter = AlpacaAdapter::new(
            &test_credentials(),
            broadcaster,
            TradingPlatform::AlpacaPaper,
        )
        .expect("Failed to build HTTP client")
        .with_exit_handler(ExitHandlerConfig::default());

        let state_manager = adapter.state_manager();
        let _event_router = adapter.event_router();
        let _exit_handler = adapter.exit_handler();

        assert_eq!(state_manager.order_count(), 0);
    }

    #[test]
    fn maps_alpaca_insufficient_buying_power() {
        assert_eq!(
            map_alpaca_reject_code("insufficient_buying_power"),
            reject_codes::INSUFFICIENT_FUNDS
        );
    }

    #[test]
    fn maps_alpaca_forbidden_to_insufficient_funds() {
        assert_eq!(
            map_alpaca_reject_code("forbidden"),
            reject_codes::INSUFFICIENT_FUNDS
        );
    }

    #[test]
    fn maps_alpaca_market_closed() {
        assert_eq!(
            map_alpaca_reject_code("market_closed_error"),
            reject_codes::MARKET_CLOSED
        );
    }

    #[test]
    fn maps_alpaca_invalid_qty() {
        assert_eq!(
            map_alpaca_reject_code("invalid_qty"),
            reject_codes::INVALID_QUANTITY
        );
    }

    #[test]
    fn maps_alpaca_symbol_not_found() {
        assert_eq!(
            map_alpaca_reject_code("symbol_not_found"),
            reject_codes::INVALID_SYMBOL
        );
    }

    #[test]
    fn passes_through_unknown_alpaca_code() {
        assert_eq!(map_alpaca_reject_code("some_other_code"), "some_other_code");
    }

    #[test]
    fn empty_alpaca_code_returns_order_rejected() {
        assert_eq!(map_alpaca_reject_code(""), "ORDER_REJECTED");
    }

    // =====================================================================
    // map_http_error
    // =====================================================================

    #[test]
    fn http_error_429_rate_limited_header_priority() {
        let body = serde_json::json!({"message": "Rate limited", "retry_after": 10});
        let err =
            AlpacaAdapter::map_http_error(reqwest::StatusCode::TOO_MANY_REQUESTS, &body, Some(5));
        match err {
            GatewayError::RateLimited {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, Some(5)),
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_503_provider_unavailable() {
        let body = serde_json::json!({"message": "Maintenance"});
        let err =
            AlpacaAdapter::map_http_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, &body, None);
        assert!(matches!(err, GatewayError::ProviderUnavailable { .. }));
    }

    #[test]
    fn http_error_500_provider_error_with_alpaca() {
        let body = serde_json::json!({"message": "Crash"});
        let err =
            AlpacaAdapter::map_http_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &body, None);
        match err {
            GatewayError::ProviderError { provider, .. } => {
                assert_eq!(provider.as_deref(), Some("alpaca"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_400_invalid_request() {
        let body = serde_json::json!({"message": "Bad params"});
        let err = AlpacaAdapter::map_http_error(reqwest::StatusCode::BAD_REQUEST, &body, None);
        assert!(matches!(err, GatewayError::InvalidRequest { .. }));
    }

    #[test]
    fn http_error_401_unauthorized() {
        let body = serde_json::json!({"message": "Bad key"});
        let err = AlpacaAdapter::map_http_error(reqwest::StatusCode::UNAUTHORIZED, &body, None);
        match err {
            GatewayError::Unauthorized { code, .. } => {
                assert_eq!(code, "AUTH_FAILED");
            }
            other => panic!("Expected Unauthorized, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_404_invalid_request() {
        // 404 from data endpoints (symbol unknown, etc.) is a client error, not a provider
        // outage — must NOT trip the circuit breaker. Order/position 404s are translated to
        // OrderNotFound / PositionNotFound before this map function is reached.
        let body = serde_json::json!({"message": "Not found"});
        let err = AlpacaAdapter::map_http_error(reqwest::StatusCode::NOT_FOUND, &body, None);
        match err {
            GatewayError::InvalidRequest { message, .. } => {
                assert!(message.contains("Not found"), "got: {message}");
            }
            other => panic!("Expected InvalidRequest, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_422_order_rejected_with_mapped_code() {
        let body = serde_json::json!({"message": "Market closed", "code": "market_closed_error"});
        let err =
            AlpacaAdapter::map_http_error(reqwest::StatusCode::UNPROCESSABLE_ENTITY, &body, None);
        match err {
            GatewayError::OrderRejected { reject_code, .. } => {
                assert_eq!(reject_code.as_deref(), Some("MARKET_CLOSED"));
            }
            other => panic!("Expected OrderRejected, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_unknown_4xx_invalid_request() {
        // Unmapped 4xx → InvalidRequest (does not trip circuit breaker).
        let body = serde_json::json!({"message": "Teapot"});
        let err = AlpacaAdapter::map_http_error(reqwest::StatusCode::IM_A_TEAPOT, &body, None);
        match err {
            GatewayError::InvalidRequest { message, .. } => {
                assert!(message.contains("HTTP 418"), "got: {message}");
            }
            other => panic!("Expected InvalidRequest, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_unknown_5xx_provider_error() {
        // Unmapped 5xx (not 500/502/503/504) → ProviderError (counts toward circuit breaker).
        let body = serde_json::json!({"message": "Out of disk"});
        let err =
            AlpacaAdapter::map_http_error(reqwest::StatusCode::INSUFFICIENT_STORAGE, &body, None);
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("HTTP 507"), "got: {message}");
                assert_eq!(provider.as_deref(), Some("alpaca"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }
}
