//! Oanda adapter for forex and CFD trading via the v20 REST API.
//!
//! This module implements the `TradingAdapter` trait for Oanda's v20 API,
//! providing access to forex, metals, indices, and commodity CFD markets.

use super::types::{
    OandaAccountResponse, OandaCandlesResponse, OandaClientExtensions, OandaClosePositionRequest,
    OandaCreateOrderResponse, OandaOrder, OandaOrderRequest, OandaOrderRequestWrapper,
    OandaOrdersResponse, OandaPosition, OandaPositionResponse, OandaPositionsResponse,
    OandaPricingResponse, OandaStopLossOnFill, OandaTakeProfitOnFill, OandaTrade,
    OandaTradesResponse,
};

use async_trait::async_trait;
use reqwest::Client;
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretBox};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, RwLock, broadcast};
use tracing::{debug, instrument, warn};

use tektii_gateway_core::circuit_breaker::{
    AdapterCircuitBreaker, CircuitBreakerSnapshot, is_outage_error,
};
use tektii_gateway_core::error::{GatewayError, GatewayResult, reject_codes};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::{ExitHandler, ExitHandlerConfig, ExitHandling};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::WsMessage;

use crate::capabilities::OandaCapabilities;
use crate::credentials::OandaCredentials;

// Import trading models
use tektii_gateway_core::adapter::{ProviderCapabilities, TradingAdapter};
use tektii_gateway_core::models::{
    Account, Bar, BarParams, CancelOrderResult, Capabilities, ClosePositionRequest,
    ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams,
    Position, PositionSide, Quote, Side, Timeframe, Trade, TradeQueryParams, TradingPlatform,
};

// ============================================================================
// Constants
// ============================================================================

/// Base URL for Oanda practice (demo) accounts.
const OANDA_PRACTICE_REST_URL: &str = "https://api-fxpractice.oanda.com";
/// Base URL for Oanda live (real money) accounts.
const OANDA_LIVE_REST_URL: &str = "https://api-fxtrade.oanda.com";

// ============================================================================
// Adapter
// ============================================================================

/// Oanda REST API adapter implementing `TradingAdapter`.
pub struct OandaAdapter {
    /// HTTP client with connection pooling.
    client: Client,
    /// Base URL for REST API endpoints.
    base_url: String,
    /// Oanda account ID (included in every request URL).
    account_id: String,
    /// Bearer API token.
    api_token: Arc<SecretBox<String>>,
    /// Provider capabilities (defaults to netting until hedging detection runs).
    capabilities: OandaCapabilities,
    /// Lazily detected hedging mode from the account API.
    hedging_mode: OnceCell<bool>,

    // === Event Infrastructure ===
    /// State Manager for caching orders and positions.
    state_manager: Arc<StateManager>,
    /// Exit Handler for managing stop-loss and take-profit orders.
    exit_handler: Arc<ExitHandler>,
    /// Event Router for processing events.
    event_router: Arc<EventRouter>,
    /// Platform identifier for this adapter instance.
    platform: TradingPlatform,
    /// Circuit breaker for detecting provider outages.
    circuit_breaker: Arc<RwLock<AdapterCircuitBreaker>>,
}

impl OandaAdapter {
    /// Create a new Oanda adapter with full event infrastructure.
    ///
    /// # Arguments
    ///
    /// * `credentials` - Oanda API credentials (token, account ID, optional URL overrides)
    /// * `broadcaster` - Broadcast channel for sending events to strategies
    /// * `platform` - Platform identifier (`OandaPractice` or `OandaLive`)
    ///
    /// # Base URL Resolution
    ///
    /// Priority order:
    /// 1. `credentials.rest_url` if set (for testing with mock servers)
    /// 2. `https://api-fxtrade.oanda.com` for `OandaLive`
    /// 3. `https://api-fxpractice.oanda.com` for `OandaPractice` and other variants
    /// # Errors
    ///
    /// Returns `reqwest::Error` if the HTTP client fails to build.
    pub fn new(
        credentials: &OandaCredentials,
        broadcaster: broadcast::Sender<WsMessage>,
        platform: TradingPlatform,
    ) -> Result<Self, reqwest::Error> {
        let base_url = credentials
            .rest_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::OandaLive => OANDA_LIVE_REST_URL.to_string(),
                _ => OANDA_PRACTICE_REST_URL.to_string(),
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
            "oanda",
        )));

        Ok(Self {
            client,
            base_url,
            account_id: credentials.account_id.clone(),
            api_token: tektii_gateway_core::arc_secret(&credentials.api_token),
            capabilities: OandaCapabilities::new(),
            hedging_mode: OnceCell::new(),
            state_manager,
            exit_handler,
            event_router,
            platform,
            circuit_breaker,
        })
    }

    /// Enable custom `ExitHandler` configuration.
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

    // =========================================================================
    // Event Infrastructure Accessors
    // =========================================================================

    /// Returns a reference to the `StateManager`.
    #[must_use]
    #[allow(dead_code)]
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

    /// Returns the platform identifier for this adapter.
    #[must_use]
    #[allow(dead_code)]
    pub fn platform_id(&self) -> TradingPlatform {
        self.platform
    }

    // =========================================================================
    // Circuit Breaker
    // =========================================================================

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

    // =========================================================================
    // URL Helpers
    // =========================================================================

    /// Build an account-scoped URL: `{base}/v3/accounts/{id}{path}`.
    fn account_url(&self, path: &str) -> String {
        format!("{}/v3/accounts/{}{}", self.base_url, self.account_id, path)
    }

    /// Build an instrument URL: `{base}/v3/instruments/{instrument}{path}`.
    fn instrument_url(&self, instrument: &str, path: &str) -> String {
        format!("{}/v3/instruments/{}{}", self.base_url, instrument, path)
    }

    /// Returns the `Authorization: Bearer <token>` header value.
    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_token.expose_secret())
    }

    // =========================================================================
    // Symbol Mapping
    // =========================================================================

    // =========================================================================
    // Unit Conversion (signed units <-> side + quantity)
    // =========================================================================

    /// Convert gateway `Side` + `Decimal` quantity to Oanda signed units string.
    fn to_oanda_units(side: Side, quantity: Decimal) -> String {
        match side {
            Side::Buy => quantity.to_string(),
            Side::Sell => format!("-{quantity}"),
        }
    }

    /// Parse Oanda signed units string into `(Side, Decimal)`.
    pub(crate) fn from_oanda_units(units: &str) -> GatewayResult<(Side, Decimal)> {
        let d = Decimal::from_str(units).map_err(|_| GatewayError::InvalidValue {
            field: "units".to_string(),
            provided: Some(units.to_string()),
            message: format!("Cannot parse Oanda units '{units}' as decimal"),
        })?;

        if d.is_sign_negative() {
            Ok((Side::Sell, d.abs()))
        } else {
            Ok((Side::Buy, d))
        }
    }

    // =========================================================================
    // Order Type Mapping
    // =========================================================================

    /// Convert gateway `OrderType` to Oanda order type string.
    fn to_oanda_order_type(
        order_type: tektii_gateway_core::models::OrderType,
    ) -> GatewayResult<&'static str> {
        match order_type {
            tektii_gateway_core::models::OrderType::Market => Ok("MARKET"),
            tektii_gateway_core::models::OrderType::Limit => Ok("LIMIT"),
            tektii_gateway_core::models::OrderType::Stop => Ok("STOP"),
            tektii_gateway_core::models::OrderType::StopLimit => Ok("MARKET_IF_TOUCHED"),
            tektii_gateway_core::models::OrderType::TrailingStop => {
                Err(GatewayError::UnsupportedOperation {
                    operation: "standalone trailing stop order".to_string(),
                    provider: "oanda".to_string(),
                })
            }
        }
    }

    /// Parse Oanda order type string to gateway `OrderType`.
    pub(crate) fn from_oanda_order_type(
        oanda_type: &str,
    ) -> GatewayResult<tektii_gateway_core::models::OrderType> {
        match oanda_type {
            "MARKET" => Ok(tektii_gateway_core::models::OrderType::Market),
            "LIMIT" | "TAKE_PROFIT" => Ok(tektii_gateway_core::models::OrderType::Limit),
            "STOP" | "STOP_LOSS" => Ok(tektii_gateway_core::models::OrderType::Stop),
            "MARKET_IF_TOUCHED" => Ok(tektii_gateway_core::models::OrderType::StopLimit),
            "TRAILING_STOP_LOSS" => Ok(tektii_gateway_core::models::OrderType::TrailingStop),
            other => Err(GatewayError::InvalidValue {
                field: "order_type".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown order type from Oanda: {other}"),
            }),
        }
    }

    /// Convert gateway `TimeInForce` to Oanda string.
    fn to_oanda_tif(tif: tektii_gateway_core::models::TimeInForce) -> &'static str {
        match tif {
            tektii_gateway_core::models::TimeInForce::Gtc => "GTC",
            tektii_gateway_core::models::TimeInForce::Day => "GFD",
            tektii_gateway_core::models::TimeInForce::Ioc => "IOC",
            tektii_gateway_core::models::TimeInForce::Fok => "FOK",
        }
    }

    /// Parse Oanda time-in-force string to gateway `TimeInForce`.
    fn from_oanda_tif(tif: &str) -> GatewayResult<tektii_gateway_core::models::TimeInForce> {
        match tif {
            "GTC" | "GTD" => Ok(tektii_gateway_core::models::TimeInForce::Gtc),
            "GFD" => Ok(tektii_gateway_core::models::TimeInForce::Day),
            "IOC" => Ok(tektii_gateway_core::models::TimeInForce::Ioc),
            "FOK" => Ok(tektii_gateway_core::models::TimeInForce::Fok),
            other => Err(GatewayError::InvalidValue {
                field: "time_in_force".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown time in force from Oanda: {other}"),
            }),
        }
    }

    /// Parse Oanda order state to gateway `OrderStatus`.
    pub(crate) fn from_oanda_order_state(
        state: &str,
    ) -> GatewayResult<tektii_gateway_core::models::OrderStatus> {
        match state {
            "PENDING" => Ok(tektii_gateway_core::models::OrderStatus::Open),
            "FILLED" | "TRIGGERED" => Ok(tektii_gateway_core::models::OrderStatus::Filled),
            "CANCELLED" => Ok(tektii_gateway_core::models::OrderStatus::Cancelled),
            other => Err(GatewayError::InvalidValue {
                field: "state".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown order state from Oanda: {other}"),
            }),
        }
    }

    // =========================================================================
    // Translation Helpers
    // =========================================================================

    /// Translate an `OandaOrder` to a gateway `Order`.
    fn translate_oanda_order(oanda_order: OandaOrder) -> GatewayResult<Order> {
        let (side, quantity) = if let Some(ref units) = oanda_order.units {
            Self::from_oanda_units(units)?
        } else {
            (Side::Buy, Decimal::ZERO)
        };

        let price = oanda_order
            .price
            .as_ref()
            .and_then(|s| Decimal::from_str(s).ok());

        let order_type = Self::from_oanda_order_type(&oanda_order.order_type)?;

        // For limit orders, the price is the limit price
        // For stop orders, the price is the stop price
        let (limit_price, stop_price) = match order_type {
            tektii_gateway_core::models::OrderType::Limit => (price, None),
            tektii_gateway_core::models::OrderType::Stop => (None, price),
            tektii_gateway_core::models::OrderType::StopLimit => (price, price),
            _ => (None, None),
        };

        let tif = oanda_order
            .time_in_force
            .as_deref()
            .map(Self::from_oanda_tif)
            .transpose()?
            .unwrap_or(tektii_gateway_core::models::TimeInForce::Gtc);

        let status = Self::from_oanda_order_state(&oanda_order.state)?;

        let symbol = oanda_order
            .instrument
            .as_deref()
            .unwrap_or_default()
            .to_string();

        let created_at = chrono::DateTime::parse_from_rfc3339(&oanda_order.create_time)
            .map_or_else(|_| chrono::Utc::now(), |dt| dt.with_timezone(&chrono::Utc));

        Ok(Order {
            id: oanda_order.id,
            client_order_id: oanda_order
                .client_extensions
                .as_ref()
                .and_then(|ext| ext.id.clone()),
            symbol,
            side,
            order_type,
            time_in_force: tif,
            quantity,
            filled_quantity: Decimal::ZERO, // Oanda doesn't expose partial fill qty on orders
            remaining_quantity: quantity,
            limit_price,
            stop_price,
            average_fill_price: None,
            status,
            stop_loss: oanda_order
                .stop_loss_on_fill
                .as_ref()
                .and_then(|sl| Decimal::from_str(&sl.price).ok()),
            take_profit: oanda_order
                .take_profit_on_fill
                .as_ref()
                .and_then(|tp| Decimal::from_str(&tp.price).ok()),
            trailing_distance: oanda_order
                .trailing_stop_loss_on_fill
                .as_ref()
                .and_then(|ts| Decimal::from_str(&ts.distance).ok()),
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
            updated_at: created_at, // Oanda doesn't have a separate updated_at
        })
    }

    /// Translate an `OandaPosition` into zero, one, or two gateway `Position`s.
    fn translate_oanda_position(oanda_pos: &OandaPosition) -> Vec<Position> {
        let mut positions = Vec::new();
        let symbol = oanda_pos.instrument.clone();

        // Long side
        if oanda_pos.long.units != "0" {
            let quantity = Decimal::from_str(&oanda_pos.long.units).unwrap_or_default();
            let avg_price = oanda_pos
                .long
                .average_price
                .as_deref()
                .and_then(|s| Decimal::from_str(s).ok())
                .unwrap_or_default();
            let unrealized_pnl = oanda_pos
                .long
                .unrealized_pl
                .as_deref()
                .and_then(|s| Decimal::from_str(s).ok())
                .unwrap_or_default();

            positions.push(Position {
                id: format!("{}_LONG", oanda_pos.instrument),
                symbol: symbol.clone(),
                side: PositionSide::Long,
                quantity: quantity.abs(),
                average_entry_price: avg_price,
                current_price: Decimal::ZERO,
                unrealized_pnl,
                realized_pnl: Decimal::ZERO,
                margin_mode: None,
                leverage: None,
                liquidation_price: None,
                opened_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            });
        }

        // Short side
        if oanda_pos.short.units != "0" {
            let quantity = Decimal::from_str(&oanda_pos.short.units).unwrap_or_default();
            let avg_price = oanda_pos
                .short
                .average_price
                .as_deref()
                .and_then(|s| Decimal::from_str(s).ok())
                .unwrap_or_default();
            let unrealized_pnl = oanda_pos
                .short
                .unrealized_pl
                .as_deref()
                .and_then(|s| Decimal::from_str(s).ok())
                .unwrap_or_default();

            positions.push(Position {
                id: format!("{}_SHORT", oanda_pos.instrument),
                symbol,
                side: PositionSide::Short,
                quantity: quantity.abs(),
                average_entry_price: avg_price,
                current_price: Decimal::ZERO,
                unrealized_pnl,
                realized_pnl: Decimal::ZERO,
                margin_mode: None,
                leverage: None,
                liquidation_price: None,
                opened_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            });
        }

        positions
    }

    /// Map Oanda trade to gateway `Trade`.
    fn translate_oanda_trade(oanda_trade: &OandaTrade) -> GatewayResult<Trade> {
        let (side, quantity) = Self::from_oanda_units(&oanda_trade.current_units)?;
        let price = Decimal::from_str(&oanda_trade.price).unwrap_or_default();
        let symbol = oanda_trade.instrument.clone();

        let timestamp = chrono::DateTime::parse_from_rfc3339(&oanda_trade.open_time)
            .map_or_else(|_| chrono::Utc::now(), |dt| dt.with_timezone(&chrono::Utc));

        Ok(Trade {
            id: oanda_trade.id.clone(),
            order_id: String::new(), // Oanda trades don't directly expose the originating order ID
            symbol,
            side,
            quantity,
            price,
            commission: Decimal::ZERO, // Oanda is spread-based, no explicit commissions
            commission_currency: String::new(),
            is_maker: None,
            timestamp,
        })
    }

    // =========================================================================
    // HTTP Error Mapping
    // =========================================================================

    /// Map HTTP status + response body to `GatewayError`.
    fn map_http_error(status: reqwest::StatusCode, body: &serde_json::Value) -> GatewayError {
        let message = body
            .get("errorMessage")
            .or_else(|| body.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();

        match status.as_u16() {
            429 => GatewayError::RateLimited {
                retry_after_seconds: None,
                reset_at: None,
            },

            503 => GatewayError::ProviderUnavailable { message },

            500 | 502 | 504 => GatewayError::ProviderError {
                message,
                provider: Some("oanda".to_string()),
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

            404 => GatewayError::ProviderError {
                message: format!("Upstream returned 404: {message}"),
                provider: Some("oanda".to_string()),
                source: None,
            },

            422 => {
                let raw_reason = body
                    .get("rejectReason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                GatewayError::OrderRejected {
                    reason: message,
                    reject_code: Some(map_oanda_reject_code(raw_reason).to_string()),
                    details: Some(body.clone()),
                }
            }

            _ => GatewayError::ProviderError {
                message: format!("HTTP {}: {message}", status.as_u16()),
                provider: Some("oanda".to_string()),
                source: None,
            },
        }
    }

    /// Send a GET request and handle errors.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> GatewayResult<T> {
        let downstream_start = Instant::now();

        let response = self
            .client
            .get(url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("oanda".to_string()),
                source: None,
            })?;

        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            url, "Oanda GET completed"
        );

        let status = response.status();
        if !status.is_success() {
            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"errorMessage": "Unknown error"}));
            return Err(Self::map_http_error(status, &body));
        }

        response
            .json::<T>()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse Oanda response: {e}")))
    }

    /// Convert `BarParams` timeframe to Oanda granularity string.
    fn to_oanda_granularity(params: &BarParams) -> &'static str {
        match params.timeframe {
            Timeframe::OneMinute => "M1",
            Timeframe::TwoMinutes => "M2",
            Timeframe::FiveMinutes => "M5",
            Timeframe::TenMinutes => "M10",
            Timeframe::FifteenMinutes => "M15",
            Timeframe::ThirtyMinutes => "M30",
            Timeframe::OneHour => "H1",
            Timeframe::TwoHours => "H2",
            Timeframe::FourHours => "H4",
            Timeframe::TwelveHours => "H12",
            Timeframe::OneDay => "D",
            Timeframe::OneWeek => "W",
        }
    }
}

/// Map an Oanda reject reason string to a canonical reject code.
///
/// Known Oanda reasons are mapped to `reject_codes::*` constants.
/// Unknown reasons pass through as-is.
fn map_oanda_reject_code(reason: &str) -> &str {
    match reason {
        "INSUFFICIENT_MARGIN" | "INSUFFICIENT_FUNDS" | "INSUFFICIENT_LIQUIDITY" => {
            reject_codes::INSUFFICIENT_FUNDS
        }
        "MARKET_HALTED" | "INSTRUMENT_NOT_TRADEABLE" => reject_codes::MARKET_CLOSED,
        other => {
            if other.is_empty() {
                "ORDER_REJECTED"
            } else {
                other
            }
        }
    }
}

// ============================================================================
// TradingAdapter Implementation
// ============================================================================

#[async_trait]
impl TradingAdapter for OandaAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &self.capabilities
    }

    fn platform(&self) -> TradingPlatform {
        self.platform
    }

    fn provider_name(&self) -> &'static str {
        "oanda"
    }

    // =========================================================================
    // Account
    // =========================================================================

    #[instrument(skip(self), name = "oanda_get_account")]
    async fn get_account(&self) -> GatewayResult<Account> {
        self.check_circuit_breaker().await?;

        let url = self.account_url("/summary");
        let result: GatewayResult<OandaAccountResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let resp = result?;
        let acct = resp.account;

        Ok(Account {
            balance: Decimal::from_str(&acct.balance).unwrap_or_default(),
            equity: Decimal::from_str(&acct.nav).unwrap_or_default(),
            margin_used: Decimal::from_str(&acct.margin_used).unwrap_or_default(),
            margin_available: Decimal::from_str(&acct.margin_available).unwrap_or_default(),
            unrealized_pnl: Decimal::from_str(&acct.unrealized_pl).unwrap_or_default(),
            currency: acct.currency,
        })
    }

    // =========================================================================
    // Orders
    // =========================================================================

    #[instrument(skip(self, request), name = "oanda_submit_order")]
    async fn submit_order(
        &self,
        request: &tektii_gateway_core::models::OrderRequest,
    ) -> GatewayResult<OrderHandle> {
        self.check_circuit_breaker().await?;

        let oanda_type = Self::to_oanda_order_type(request.order_type)?;
        let oanda_units = Self::to_oanda_units(request.side, request.quantity);

        // Market orders must use FOK for Oanda
        let tif = if request.order_type == tektii_gateway_core::models::OrderType::Market {
            "FOK"
        } else {
            Self::to_oanda_tif(request.time_in_force)
        };

        // Build price field (for limit/stop orders)
        let price = request
            .limit_price
            .or(request.stop_price)
            .map(|d| d.to_string());

        // Build bracket (SL/TP on fill)
        let stop_loss_on_fill = request.stop_loss.map(|sl| OandaStopLossOnFill {
            price: sl.to_string(),
            time_in_force: "GTC".to_string(),
        });

        let take_profit_on_fill = request.take_profit.map(|tp| OandaTakeProfitOnFill {
            price: tp.to_string(),
            time_in_force: "GTC".to_string(),
        });

        let client_extensions = request
            .client_order_id
            .as_ref()
            .map(|id| OandaClientExtensions {
                id: Some(id.clone()),
                tag: None,
                comment: None,
            });

        let oanda_request = OandaOrderRequestWrapper {
            order: OandaOrderRequest {
                order_type: oanda_type.to_string(),
                instrument: request.symbol.clone(),
                units: oanda_units,
                time_in_force: tif.to_string(),
                price,
                stop_loss_on_fill,
                take_profit_on_fill,
                trailing_stop_loss_on_fill: None,
                client_extensions,
            },
        };

        let url = self.account_url("/orders");
        let downstream_start = Instant::now();

        let response = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&oanda_request)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("oanda".to_string()),
                source: None,
            })?;

        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Oanda POST /orders completed"
        );

        let status = response.status();
        if !status.is_success() {
            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"errorMessage": "Unknown error"}));

            let error = Self::map_http_error(status, &body);
            self.record_if_outage(&error).await;
            return Err(error);
        }

        let resp: OandaCreateOrderResponse = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse order response: {e}")))?;

        // Check for rejection
        if let Some(ref reject) = resp.order_reject_transaction {
            let reason = reject
                .reject_reason
                .as_deref()
                .unwrap_or("Unknown rejection reason");
            return Err(GatewayError::OrderRejected {
                reason: reason.to_string(),
                reject_code: Some(map_oanda_reject_code(reason).to_string()),
                details: None,
            });
        }

        // Get order ID from create or fill transaction
        let (order_id, order_status) = if let Some(ref fill) = resp.order_fill_transaction {
            // Market order filled immediately
            (
                fill.id.clone(),
                tektii_gateway_core::models::OrderStatus::Filled,
            )
        } else if let Some(ref create) = resp.order_create_transaction {
            (
                create.id.clone(),
                tektii_gateway_core::models::OrderStatus::Open,
            )
        } else {
            return Err(GatewayError::internal(
                "Oanda order response contained no create or fill transaction".to_string(),
            ));
        };

        Ok(OrderHandle {
            id: order_id,
            client_order_id: request.client_order_id.clone(),
            correlation_id: None,
            status: order_status,
        })
    }

    #[instrument(skip(self), name = "oanda_get_order")]
    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        self.check_circuit_breaker().await?;

        let url = self.account_url(&format!("/orders/{order_id}"));

        // Oanda wraps single order in {"order": {...}}
        #[derive(serde::Deserialize)]
        struct SingleOrderResponse {
            order: OandaOrder,
        }

        let result: GatewayResult<SingleOrderResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
            if matches!(error, GatewayError::ProviderError { message, .. } if message.contains("404"))
            {
                return Err(GatewayError::OrderNotFound {
                    id: order_id.to_string(),
                });
            }
        }

        let resp = result?;
        Self::translate_oanda_order(resp.order)
    }

    #[instrument(skip(self, params), name = "oanda_get_orders")]
    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.check_circuit_breaker().await?;

        let mut url = self.account_url("/orders");

        // Add symbol filter if provided
        if let Some(ref symbol) = params.symbol {
            url = format!("{url}?instrument={symbol}");
        }

        let result: GatewayResult<OandaOrdersResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let resp = result?;
        resp.orders
            .into_iter()
            .map(Self::translate_oanda_order)
            .collect()
    }

    #[instrument(skip(self, params), name = "oanda_get_order_history")]
    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.check_circuit_breaker().await?;

        // Oanda: use state=ALL to get historical orders
        let mut url = self.account_url("/orders?state=ALL");

        if let Some(ref symbol) = params.symbol {
            url = format!("{url}&instrument={symbol}");
        }

        if let Some(limit) = params.limit {
            url = format!("{url}&count={limit}");
        }

        let result: GatewayResult<OandaOrdersResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let resp = result?;
        resp.orders
            .into_iter()
            .map(Self::translate_oanda_order)
            .collect()
    }

    #[instrument(skip(self, request), name = "oanda_modify_order")]
    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        self.check_circuit_breaker().await?;

        // Oanda modify is a full replacement via PUT.
        // First, fetch the current order to merge changes.
        let current = self.get_order(order_id).await?;

        let oanda_type = Self::to_oanda_order_type(current.order_type)?;

        let quantity = request.quantity.unwrap_or(current.quantity);
        let side = current.side;
        let oanda_units = Self::to_oanda_units(side, quantity);

        // ModifyOrderRequest doesn't include time_in_force; preserve the current order's TIF
        let tif = Self::to_oanda_tif(current.time_in_force);

        let price = request
            .limit_price
            .or(request.stop_price)
            .or(current.limit_price)
            .or(current.stop_price)
            .map(|d| d.to_string());

        let replacement = OandaOrderRequestWrapper {
            order: OandaOrderRequest {
                order_type: oanda_type.to_string(),
                instrument: current.symbol.clone(),
                units: oanda_units,
                time_in_force: tif.to_string(),
                price,
                stop_loss_on_fill: None,
                take_profit_on_fill: None,
                trailing_stop_loss_on_fill: None,
                client_extensions: None,
            },
        };

        let url = self.account_url(&format!("/orders/{order_id}"));
        let downstream_start = Instant::now();

        let response = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&replacement)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("oanda".to_string()),
                source: None,
            })?;

        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Oanda PUT /orders/{} completed", order_id
        );

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }
        if !status.is_success() {
            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"errorMessage": "Unknown error"}));
            let error = Self::map_http_error(status, &body);
            self.record_if_outage(&error).await;
            return Err(error);
        }

        // After successful modification, Oanda returns a new order (the replacement).
        // The old order is cancelled. Fetch the replacement.
        // The response contains orderCancelTransaction and replacementOrderCreateTransaction.
        let resp_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| GatewayError::internal(format!("Failed to parse modify response: {e}")))?;

        // Get the replacement order ID from the create transaction
        let order = if let Some(create_tx) = resp_body.get("orderCreateTransaction") {
            if let Some(new_id) = create_tx.get("id").and_then(|v| v.as_str()) {
                self.get_order(new_id).await?
            } else {
                self.get_order(order_id).await?
            }
        } else {
            // Fallback: return the order we built from the modification
            self.get_order(order_id).await?
        };

        Ok(ModifyOrderResult {
            order,
            previous_order_id: Some(order_id.to_string()),
        })
    }

    #[instrument(skip(self), name = "oanda_cancel_order")]
    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        self.check_circuit_breaker().await?;

        let url = self.account_url(&format!("/orders/{order_id}/cancel"));
        let downstream_start = Instant::now();

        let response = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("oanda".to_string()),
                source: None,
            })?;

        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Oanda PUT /orders/{}/cancel completed", order_id
        );

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::OrderNotFound {
                id: order_id.to_string(),
            });
        }
        if !status.is_success() {
            let body = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"errorMessage": "Unknown error"}));
            let error = Self::map_http_error(status, &body);
            self.record_if_outage(&error).await;
            return Err(error);
        }

        // Build a minimal cancelled order response
        let order = Order {
            id: order_id.to_string(),
            client_order_id: None,
            symbol: String::new(),
            side: Side::Buy,
            order_type: tektii_gateway_core::models::OrderType::Market,
            time_in_force: tektii_gateway_core::models::TimeInForce::Gtc,
            quantity: Decimal::ZERO,
            filled_quantity: Decimal::ZERO,
            remaining_quantity: Decimal::ZERO,
            limit_price: None,
            stop_price: None,
            average_fill_price: None,
            status: tektii_gateway_core::models::OrderStatus::Cancelled,
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        Ok(CancelOrderResult {
            success: true,
            order,
        })
    }

    // =========================================================================
    // Trades
    // =========================================================================

    #[instrument(skip(self, params), name = "oanda_get_trades")]
    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        self.check_circuit_breaker().await?;

        let mut url = self.account_url("/trades");

        if let Some(ref symbol) = params.symbol {
            url = format!("{url}?instrument={symbol}");
        }

        let result: GatewayResult<OandaTradesResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let resp = result?;
        resp.trades
            .iter()
            .map(Self::translate_oanda_trade)
            .collect()
    }

    // =========================================================================
    // Positions
    // =========================================================================

    #[instrument(skip(self), name = "oanda_get_positions")]
    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
        self.check_circuit_breaker().await?;

        if let Some(sym) = symbol {
            // Get single position by instrument
            let pos = self.get_position(sym).await?;
            return Ok(vec![pos]);
        }

        let url = self.account_url("/openPositions");
        let result: GatewayResult<OandaPositionsResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let resp = result?;
        Ok(resp
            .positions
            .iter()
            .flat_map(Self::translate_oanda_position)
            .collect())
    }

    #[instrument(skip(self), name = "oanda_get_position")]
    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        self.check_circuit_breaker().await?;

        // Position IDs are in format `{INSTRUMENT}_{LONG|SHORT}` (e.g., `EUR_USD_LONG`)
        // Or the caller might pass a normalized symbol like `F:EURUSD`
        let (oanda_instrument, requested_side) = Self::parse_position_id(position_id);

        let url = self.account_url(&format!("/positions/{oanda_instrument}"));
        let result: GatewayResult<OandaPositionResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
            if matches!(error, GatewayError::ProviderError { message, .. } if message.contains("404"))
            {
                return Err(GatewayError::PositionNotFound {
                    id: position_id.to_string(),
                });
            }
        }

        let resp = result?;
        let positions = Self::translate_oanda_position(&resp.position);

        // Filter to requested side if specified
        if let Some(side) = requested_side {
            positions
                .into_iter()
                .find(|p| p.side == side)
                .ok_or_else(|| GatewayError::PositionNotFound {
                    id: position_id.to_string(),
                })
        } else {
            // Return first position (long preferred)
            positions
                .into_iter()
                .next()
                .ok_or_else(|| GatewayError::PositionNotFound {
                    id: position_id.to_string(),
                })
        }
    }

    #[instrument(skip(self, request), name = "oanda_close_position")]
    async fn close_position(
        &self,
        position_id: &str,
        request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        self.check_circuit_breaker().await?;

        let (oanda_instrument, requested_side) = Self::parse_position_id(position_id);
        let side = requested_side.unwrap_or(PositionSide::Long);

        let close_amount = request
            .quantity
            .map_or_else(|| "ALL".to_string(), |q| q.to_string());

        let close_request = match side {
            PositionSide::Long => OandaClosePositionRequest {
                long_units: Some(close_amount),
                short_units: None,
            },
            PositionSide::Short => OandaClosePositionRequest {
                long_units: None,
                short_units: Some(close_amount),
            },
        };

        let url = self.account_url(&format!("/positions/{oanda_instrument}/close"));
        let downstream_start = Instant::now();

        let response = self
            .client
            .put(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&close_request)
            .send()
            .await
            .map_err(|e| GatewayError::ProviderError {
                message: format!("Request failed: {e}"),
                provider: Some("oanda".to_string()),
                source: None,
            })?;

        let downstream_time = downstream_start.elapsed();
        debug!(
            downstream_ms = downstream_time.as_secs_f64() * 1000.0,
            "Oanda PUT /positions/{}/close completed", oanda_instrument
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
                .unwrap_or_else(|_| serde_json::json!({"errorMessage": "Unknown error"}));
            let error = Self::map_http_error(status, &body);
            self.record_if_outage(&error).await;
            return Err(error);
        }

        // Parse response to get transaction IDs
        let resp_body: serde_json::Value = response.json().await.map_err(|e| {
            GatewayError::internal(format!("Failed to parse close position response: {e}"))
        })?;

        // Oanda returns `longOrderFillTransaction` or `shortOrderFillTransaction`
        let fill_key = match side {
            PositionSide::Long => "longOrderFillTransaction",
            PositionSide::Short => "shortOrderFillTransaction",
        };

        let order_id = resp_body
            .get(fill_key)
            .and_then(|tx| tx.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(OrderHandle {
            id: order_id,
            client_order_id: None,
            correlation_id: None,
            status: tektii_gateway_core::models::OrderStatus::Filled,
        })
    }

    // =========================================================================
    // Market Data
    // =========================================================================

    #[instrument(skip(self), name = "oanda_get_quote")]
    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        self.check_circuit_breaker().await?;

        let url = self.account_url(&format!("/pricing?instruments={symbol}"));

        let result: GatewayResult<OandaPricingResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let resp = result?;
        let price = resp
            .prices
            .into_iter()
            .next()
            .ok_or_else(|| GatewayError::SymbolNotFound {
                symbol: symbol.to_string(),
            })?;

        let bid = price
            .bids
            .first()
            .and_then(|b| Decimal::from_str(&b.price).ok())
            .unwrap_or_default();

        let ask = price
            .asks
            .first()
            .and_then(|a| Decimal::from_str(&a.price).ok())
            .unwrap_or_default();

        // Mid price as "last" since Oanda doesn't provide a separate last trade price
        let mid = (bid + ask) / Decimal::from(2);

        let bid_size = price.bids.first().map(|b| Decimal::from(b.liquidity));
        let ask_size = price.asks.first().map(|a| Decimal::from(a.liquidity));

        let timestamp = chrono::DateTime::parse_from_rfc3339(&price.time)
            .map_or_else(|_| chrono::Utc::now(), |dt| dt.with_timezone(&chrono::Utc));

        Ok(Quote {
            symbol: price.instrument,
            provider: "oanda".to_string(),
            bid,
            bid_size,
            ask,
            ask_size,
            last: mid,
            volume: None, // Oanda doesn't provide volume in pricing
            timestamp,
        })
    }

    #[instrument(skip(self, params), name = "oanda_get_bars")]
    async fn get_bars(&self, symbol: &str, params: &BarParams) -> GatewayResult<Vec<Bar>> {
        self.check_circuit_breaker().await?;

        let granularity = Self::to_oanda_granularity(params);

        let mut url = self.instrument_url(symbol, "/candles");
        url = format!("{url}?granularity={granularity}");

        if let Some(limit) = params.limit {
            url = format!("{url}&count={limit}");
        }

        if let Some(start) = params.start {
            url = format!("{url}&from={}", start.to_rfc3339());
        }

        if let Some(end) = params.end {
            url = format!("{url}&to={}", end.to_rfc3339());
        }

        let result: GatewayResult<OandaCandlesResponse> = self.get_json(&url).await;

        if let Err(ref error) = result {
            self.record_if_outage(error).await;
        }

        let resp = result?;

        let bars = resp
            .candles
            .into_iter()
            .filter_map(|candle| {
                // Use mid prices (most common for forex)
                let mid = candle.mid?;

                let timestamp = chrono::DateTime::parse_from_rfc3339(&candle.time)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .ok()?;

                Some(Bar {
                    symbol: symbol.to_string(),
                    provider: "oanda".to_string(),
                    timeframe: params.timeframe,
                    open: Decimal::from_str(&mid.o).unwrap_or_default(),
                    high: Decimal::from_str(&mid.h).unwrap_or_default(),
                    low: Decimal::from_str(&mid.l).unwrap_or_default(),
                    close: Decimal::from_str(&mid.c).unwrap_or_default(),
                    volume: Decimal::from(candle.volume),
                    timestamp,
                })
            })
            .collect();

        Ok(bars)
    }

    // =========================================================================
    // Capabilities & Status
    // =========================================================================

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        // Lazy-detect hedging mode from the account API on first call.
        let hedging = *self
            .hedging_mode
            .get_or_try_init(|| async {
                let url = self.account_url("/summary");
                let resp: OandaAccountResponse = self.get_json(&url).await?;
                debug!(
                    hedging_enabled = resp.account.hedging_enabled,
                    "Detected Oanda account position mode"
                );
                Ok::<bool, GatewayError>(resp.account.hedging_enabled)
            })
            .await
            .unwrap_or(&false);

        Ok(OandaCapabilities::new_with_hedging(hedging).capabilities())
    }

    #[instrument(skip(self), name = "oanda_get_connection_status")]
    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        // Use account summary as a health check
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
                warn!(error = %e, "Oanda connection check failed");
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

impl OandaAdapter {
    /// Parse a position ID into `(oanda_instrument, optional_side)`.
    ///
    /// Accepts formats:
    /// - `EUR_USD_LONG` -> `("EUR_USD", Some(Long))`
    /// - `EUR_USD_SHORT` -> `("EUR_USD", Some(Short))`
    /// - `EUR_USD` -> `("EUR_USD", None)`
    fn parse_position_id(position_id: &str) -> (String, Option<PositionSide>) {
        if let Some(base) = position_id.strip_suffix("_LONG") {
            return (base.to_string(), Some(PositionSide::Long));
        }
        if let Some(base) = position_id.strip_suffix("_SHORT") {
            return (base.to_string(), Some(PositionSide::Short));
        }

        (position_id.to_string(), None)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OandaPositionSide;

    #[test]
    fn to_oanda_units_buy_positive() {
        assert_eq!(
            OandaAdapter::to_oanda_units(Side::Buy, Decimal::from(10000)),
            "10000"
        );
    }

    #[test]
    fn to_oanda_units_sell_negative() {
        assert_eq!(
            OandaAdapter::to_oanda_units(Side::Sell, Decimal::from(5000)),
            "-5000"
        );
    }

    #[test]
    fn from_oanda_units_positive_is_buy() {
        let (side, qty) = OandaAdapter::from_oanda_units("10000").unwrap();
        assert_eq!(side, Side::Buy);
        assert_eq!(qty, Decimal::from(10000));
    }

    #[test]
    fn from_oanda_units_negative_is_sell() {
        let (side, qty) = OandaAdapter::from_oanda_units("-5000").unwrap();
        assert_eq!(side, Side::Sell);
        assert_eq!(qty, Decimal::from(5000));
    }

    #[test]
    fn from_oanda_units_invalid_returns_error() {
        assert!(OandaAdapter::from_oanda_units("abc").is_err());
    }

    #[test]
    fn order_type_roundtrip() {
        assert_eq!(
            OandaAdapter::to_oanda_order_type(tektii_gateway_core::models::OrderType::Market)
                .unwrap(),
            "MARKET"
        );
        assert_eq!(
            OandaAdapter::to_oanda_order_type(tektii_gateway_core::models::OrderType::Limit)
                .unwrap(),
            "LIMIT"
        );
        assert_eq!(
            OandaAdapter::to_oanda_order_type(tektii_gateway_core::models::OrderType::Stop)
                .unwrap(),
            "STOP"
        );
        assert_eq!(
            OandaAdapter::to_oanda_order_type(tektii_gateway_core::models::OrderType::StopLimit)
                .unwrap(),
            "MARKET_IF_TOUCHED"
        );
    }

    #[test]
    fn trailing_stop_order_type_unsupported() {
        assert!(
            OandaAdapter::to_oanda_order_type(tektii_gateway_core::models::OrderType::TrailingStop)
                .is_err()
        );
    }

    #[test]
    fn from_oanda_order_type_mapping() {
        assert_eq!(
            OandaAdapter::from_oanda_order_type("MARKET").unwrap(),
            tektii_gateway_core::models::OrderType::Market
        );
        assert_eq!(
            OandaAdapter::from_oanda_order_type("LIMIT").unwrap(),
            tektii_gateway_core::models::OrderType::Limit
        );
        assert_eq!(
            OandaAdapter::from_oanda_order_type("STOP").unwrap(),
            tektii_gateway_core::models::OrderType::Stop
        );
        assert_eq!(
            OandaAdapter::from_oanda_order_type("MARKET_IF_TOUCHED").unwrap(),
            tektii_gateway_core::models::OrderType::StopLimit
        );
        assert_eq!(
            OandaAdapter::from_oanda_order_type("STOP_LOSS").unwrap(),
            tektii_gateway_core::models::OrderType::Stop
        );
        assert_eq!(
            OandaAdapter::from_oanda_order_type("TAKE_PROFIT").unwrap(),
            tektii_gateway_core::models::OrderType::Limit
        );
    }

    #[test]
    fn tif_mapping() {
        assert_eq!(
            OandaAdapter::to_oanda_tif(tektii_gateway_core::models::TimeInForce::Gtc),
            "GTC"
        );
        assert_eq!(
            OandaAdapter::to_oanda_tif(tektii_gateway_core::models::TimeInForce::Day),
            "GFD"
        );
        assert_eq!(
            OandaAdapter::to_oanda_tif(tektii_gateway_core::models::TimeInForce::Ioc),
            "IOC"
        );
        assert_eq!(
            OandaAdapter::to_oanda_tif(tektii_gateway_core::models::TimeInForce::Fok),
            "FOK"
        );
    }

    #[test]
    fn from_oanda_tif_mapping() {
        assert_eq!(
            OandaAdapter::from_oanda_tif("GTC").unwrap(),
            tektii_gateway_core::models::TimeInForce::Gtc
        );
        assert_eq!(
            OandaAdapter::from_oanda_tif("GFD").unwrap(),
            tektii_gateway_core::models::TimeInForce::Day
        );
        assert_eq!(
            OandaAdapter::from_oanda_tif("IOC").unwrap(),
            tektii_gateway_core::models::TimeInForce::Ioc
        );
        assert_eq!(
            OandaAdapter::from_oanda_tif("FOK").unwrap(),
            tektii_gateway_core::models::TimeInForce::Fok
        );
        assert_eq!(
            OandaAdapter::from_oanda_tif("GTD").unwrap(),
            tektii_gateway_core::models::TimeInForce::Gtc
        );
    }

    #[test]
    fn parse_position_id_with_long_suffix() {
        let (inst, side) = OandaAdapter::parse_position_id("EUR_USD_LONG");
        assert_eq!(inst, "EUR_USD");
        assert_eq!(side, Some(PositionSide::Long));
    }

    #[test]
    fn parse_position_id_with_short_suffix() {
        let (inst, side) = OandaAdapter::parse_position_id("EUR_USD_SHORT");
        assert_eq!(inst, "EUR_USD");
        assert_eq!(side, Some(PositionSide::Short));
    }

    #[test]
    fn parse_position_id_raw_oanda() {
        let (inst, side) = OandaAdapter::parse_position_id("EUR_USD");
        assert_eq!(inst, "EUR_USD");
        assert_eq!(side, None);
    }

    #[test]
    fn translate_oanda_position_long_only() {
        let oanda_pos = OandaPosition {
            instrument: "EUR_USD".to_string(),
            long: OandaPositionSide {
                units: "10000".to_string(),
                average_price: Some("1.08525".to_string()),
                unrealized_pl: Some("125.50".to_string()),
            },
            short: OandaPositionSide {
                units: "0".to_string(),
                average_price: None,
                unrealized_pl: None,
            },
            unrealized_pl: Some("125.50".to_string()),
        };

        let positions = OandaAdapter::translate_oanda_position(&oanda_pos);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].symbol, "EUR_USD");
        assert_eq!(positions[0].side, PositionSide::Long);
        assert_eq!(positions[0].quantity, Decimal::from(10000));
        assert_eq!(positions[0].id, "EUR_USD_LONG");
    }

    #[test]
    fn translate_oanda_position_both_sides() {
        let oanda_pos = OandaPosition {
            instrument: "GBP_USD".to_string(),
            long: OandaPositionSide {
                units: "5000".to_string(),
                average_price: Some("1.27000".to_string()),
                unrealized_pl: Some("50.00".to_string()),
            },
            short: OandaPositionSide {
                units: "3000".to_string(),
                average_price: Some("1.27500".to_string()),
                unrealized_pl: Some("-20.00".to_string()),
            },
            unrealized_pl: Some("30.00".to_string()),
        };

        let positions = OandaAdapter::translate_oanda_position(&oanda_pos);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].side, PositionSide::Long);
        assert_eq!(positions[0].quantity, Decimal::from(5000));
        assert_eq!(positions[1].side, PositionSide::Short);
        assert_eq!(positions[1].quantity, Decimal::from(3000));
    }

    #[test]
    fn translate_oanda_position_empty() {
        let oanda_pos = OandaPosition {
            instrument: "USD_JPY".to_string(),
            long: OandaPositionSide {
                units: "0".to_string(),
                average_price: None,
                unrealized_pl: None,
            },
            short: OandaPositionSide {
                units: "0".to_string(),
                average_price: None,
                unrealized_pl: None,
            },
            unrealized_pl: None,
        };

        let positions = OandaAdapter::translate_oanda_position(&oanda_pos);
        assert!(positions.is_empty());
    }

    #[test]
    fn maps_oanda_insufficient_margin() {
        assert_eq!(
            map_oanda_reject_code("INSUFFICIENT_MARGIN"),
            reject_codes::INSUFFICIENT_FUNDS
        );
    }

    #[test]
    fn maps_oanda_insufficient_funds() {
        assert_eq!(
            map_oanda_reject_code("INSUFFICIENT_FUNDS"),
            reject_codes::INSUFFICIENT_FUNDS
        );
    }

    #[test]
    fn maps_oanda_insufficient_liquidity() {
        assert_eq!(
            map_oanda_reject_code("INSUFFICIENT_LIQUIDITY"),
            reject_codes::INSUFFICIENT_FUNDS
        );
    }

    #[test]
    fn maps_oanda_market_halted() {
        assert_eq!(
            map_oanda_reject_code("MARKET_HALTED"),
            reject_codes::MARKET_CLOSED
        );
    }

    #[test]
    fn maps_oanda_instrument_not_tradeable() {
        assert_eq!(
            map_oanda_reject_code("INSTRUMENT_NOT_TRADEABLE"),
            reject_codes::MARKET_CLOSED
        );
    }

    #[test]
    fn passes_through_unknown_oanda_reason() {
        assert_eq!(
            map_oanda_reject_code("SOME_OTHER_REASON"),
            "SOME_OTHER_REASON"
        );
    }

    #[test]
    fn empty_oanda_reason_returns_order_rejected() {
        assert_eq!(map_oanda_reject_code(""), "ORDER_REJECTED");
    }

    // =====================================================================
    // map_http_error
    // =====================================================================

    #[test]
    fn http_error_429_rate_limited() {
        let body = serde_json::json!({"errorMessage": "Rate limit exceeded"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::TOO_MANY_REQUESTS, &body);
        match err {
            GatewayError::RateLimited {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, None),
            other => panic!("Expected RateLimited, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_503_provider_unavailable() {
        let body = serde_json::json!({"errorMessage": "Maintenance"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, &body);
        match err {
            GatewayError::ProviderUnavailable { message } => {
                assert_eq!(message, "Maintenance");
            }
            other => panic!("Expected ProviderUnavailable, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_500_provider_error_with_oanda() {
        let body = serde_json::json!({"errorMessage": "Crash"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &body);
        match err {
            GatewayError::ProviderError { provider, .. } => {
                assert_eq!(provider.as_deref(), Some("oanda"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_400_invalid_request() {
        let body = serde_json::json!({"errorMessage": "Bad params"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::BAD_REQUEST, &body);
        match err {
            GatewayError::InvalidRequest { message, .. } => {
                assert_eq!(message, "Bad params");
            }
            other => panic!("Expected InvalidRequest, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_401_unauthorized() {
        let body = serde_json::json!({"errorMessage": "Invalid token"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::UNAUTHORIZED, &body);
        match err {
            GatewayError::Unauthorized { code, .. } => {
                assert_eq!(code, "AUTH_FAILED");
            }
            other => panic!("Expected Unauthorized, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_404_provider_error() {
        let body = serde_json::json!({"errorMessage": "Not found"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::NOT_FOUND, &body);
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("404"), "message should mention 404");
                assert_eq!(provider.as_deref(), Some("oanda"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_422_order_rejected_with_mapped_code() {
        let body = serde_json::json!({
            "errorMessage": "Order rejected",
            "rejectReason": "INSUFFICIENT_LIQUIDITY"
        });
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::UNPROCESSABLE_ENTITY, &body);
        match err {
            GatewayError::OrderRejected { reject_code, .. } => {
                assert_eq!(reject_code.as_deref(), Some("INSUFFICIENT_FUNDS"));
            }
            other => panic!("Expected OrderRejected, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_unknown_status() {
        let body = serde_json::json!({"errorMessage": "Teapot"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::IM_A_TEAPOT, &body);
        match err {
            GatewayError::ProviderError {
                message, provider, ..
            } => {
                assert!(message.contains("HTTP 418"), "got: {message}");
                assert_eq!(provider.as_deref(), Some("oanda"));
            }
            other => panic!("Expected ProviderError, got: {other:?}"),
        }
    }

    #[test]
    fn http_error_uses_error_message_field() {
        // Oanda uses errorMessage, not message — verify extraction priority
        let body = serde_json::json!({"errorMessage": "Oanda error", "message": "Generic"});
        let err = OandaAdapter::map_http_error(reqwest::StatusCode::BAD_REQUEST, &body);
        match err {
            GatewayError::InvalidRequest { message, .. } => {
                assert_eq!(message, "Oanda error");
            }
            other => panic!("Expected InvalidRequest, got: {other:?}"),
        }
    }
}
