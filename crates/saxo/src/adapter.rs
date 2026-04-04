//! Saxo Bank adapter for forex and CFD trading via the OpenAPI.
//!
//! This module implements the `TradingAdapter` trait for Saxo Bank's OpenAPI,
//! providing access to forex, indices, stocks, and commodity CFD markets.

use super::error::SaxoError;
use super::http::SaxoHttpClient;
use super::instruments::SaxoInstrumentMap;
use super::types::{
    SaxoBalanceResponse, SaxoChartDataPoint, SaxoChartResponse, SaxoClientResponse,
    SaxoClosedPosition, SaxoClosedPositionsResponse, SaxoDuration, SaxoInfoPriceResponse,
    SaxoModifyOrderRequest, SaxoNetPosition, SaxoNetPositionsResponse, SaxoOrder,
    SaxoOrderDuration, SaxoOrderRequest, SaxoOrderResponse, SaxoOrdersResponse,
    SaxoPrecheckResponse, SaxoRelatedOrderRequest,
};

use async_trait::async_trait;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, instrument, warn};

use tektii_gateway_core::circuit_breaker::{
    AdapterCircuitBreaker, CircuitBreakerSnapshot, is_outage_error,
};
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::{ExitHandler, ExitHandlerConfig};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::WsMessage;

use crate::capabilities::SaxoCapabilities;
use crate::credentials::SaxoCredentials;

use tektii_gateway_core::adapter::{ProviderCapabilities, TradingAdapter};
use tektii_gateway_core::models::{
    Account, Bar, BarParams, CancelOrderResult, Capabilities, ClosePositionRequest,
    ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams,
    Position, PositionSide, Quote, Side, Timeframe, Trade, TradeQueryParams, TradingPlatform,
};

// ============================================================================
// Constants
// ============================================================================

/// Maximum length for Saxo `ExternalReference` field.
const MAX_EXTERNAL_REFERENCE_LEN: usize = 50;

/// Default asset types to load instruments for.
const DEFAULT_ASSET_TYPES: &[&str] = &["FxSpot", "CfdOnIndex", "CfdOnStock", "CfdOnFutures"];

// ============================================================================
// Adapter
// ============================================================================

/// Saxo Bank REST API adapter implementing `TradingAdapter`.
pub struct SaxoAdapter {
    /// Authenticated HTTP client with auto-refresh.
    http_client: SaxoHttpClient,
    /// Bidirectional symbol <-> UIC map.
    instrument_map: SaxoInstrumentMap,
    /// Saxo account key (from credentials).
    account_key: String,
    /// Saxo client key (loaded at startup).
    client_key: String,
    /// Provider capabilities.
    capabilities: SaxoCapabilities,

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
    /// Whether to run pre-trade margin precheck before placing orders.
    precheck_enabled: bool,
}

impl SaxoAdapter {
    /// Create a new Saxo adapter with full event infrastructure.
    ///
    /// This is async because it loads the instrument map and client key from
    /// the Saxo API at startup.
    ///
    /// # Errors
    ///
    /// Returns `SaxoError` if the HTTP client, instrument map, or client key
    /// cannot be initialized.
    pub async fn new(
        credentials: &SaxoCredentials,
        broadcaster: broadcast::Sender<WsMessage>,
        platform: TradingPlatform,
    ) -> Result<Self, SaxoError> {
        let http_client = SaxoHttpClient::new(credentials, platform)?;

        // Load instrument map
        let instrument_map = SaxoInstrumentMap::load(&http_client, DEFAULT_ASSET_TYPES).await?;
        if instrument_map.is_empty() {
            return Err(SaxoError::Config(
                "No instruments loaded from Saxo API".to_string(),
            ));
        }
        debug!(
            instruments = instrument_map.len(),
            "Saxo instrument map loaded"
        );

        // Load client key
        let client_response: SaxoClientResponse = http_client.get("/port/v1/clients/me").await?;
        let client_key = client_response.client_key;
        if client_key.is_empty() {
            return Err(SaxoError::Config(
                "Saxo API returned empty client key".to_string(),
            ));
        }
        debug!(client_key = %client_key, "Saxo client key loaded");

        let state_manager = Arc::new(StateManager::new());

        let exit_handler = Arc::new(ExitHandler::with_defaults(
            Arc::clone(&state_manager),
            platform,
        ));

        let event_router = Arc::new(EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler)
                as Arc<dyn tektii_gateway_core::exit_management::ExitHandling>,
            broadcaster,
            platform,
        ));

        let circuit_breaker = Arc::new(RwLock::new(AdapterCircuitBreaker::new(
            3,
            Duration::from_secs(300),
            "saxo",
        )));

        let precheck_enabled = credentials.precheck_enabled();
        if precheck_enabled {
            debug!("Saxo pre-trade margin precheck enabled");
        }

        Ok(Self {
            http_client,
            instrument_map,
            account_key: credentials.account_key.clone(),
            client_key,
            capabilities: SaxoCapabilities::new(),
            state_manager,
            exit_handler,
            event_router,
            platform,
            circuit_breaker,
            precheck_enabled,
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
    // Symbol Resolution
    // =========================================================================

    fn resolve_symbol(
        &self,
        symbol: &str,
    ) -> GatewayResult<&super::instruments::SaxoInstrumentInfo> {
        self.instrument_map
            .resolve_symbol(symbol)
            .map_err(GatewayError::from)
    }

    fn resolve_uic(&self, uic: u32, asset_type: &str) -> GatewayResult<String> {
        self.instrument_map
            .resolve_uic(uic, asset_type)
            .map(ToString::to_string)
            .map_err(GatewayError::from)
    }

    // =========================================================================
    // Order Type Mapping
    // =========================================================================

    fn to_saxo_order_type(order_type: tektii_gateway_core::models::OrderType) -> &'static str {
        match order_type {
            tektii_gateway_core::models::OrderType::Market => "Market",
            tektii_gateway_core::models::OrderType::Limit => "Limit",
            tektii_gateway_core::models::OrderType::Stop => "StopIfTraded",
            tektii_gateway_core::models::OrderType::StopLimit => "StopLimit",
            tektii_gateway_core::models::OrderType::TrailingStop => "TrailingStopIfTraded",
        }
    }

    pub(crate) fn from_saxo_order_type(
        saxo_type: &str,
    ) -> GatewayResult<tektii_gateway_core::models::OrderType> {
        match saxo_type {
            "Market" => Ok(tektii_gateway_core::models::OrderType::Market),
            "Limit" => Ok(tektii_gateway_core::models::OrderType::Limit),
            "StopIfTraded" | "Stop" => Ok(tektii_gateway_core::models::OrderType::Stop),
            "StopLimit" => Ok(tektii_gateway_core::models::OrderType::StopLimit),
            "TrailingStopIfTraded" | "TrailingStop" => {
                Ok(tektii_gateway_core::models::OrderType::TrailingStop)
            }
            other => Err(GatewayError::InvalidValue {
                field: "order_type".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown Saxo order type: {other}"),
            }),
        }
    }

    // =========================================================================
    // Side Mapping
    // =========================================================================

    fn to_saxo_buy_sell(side: Side) -> &'static str {
        match side {
            Side::Buy => "Buy",
            Side::Sell => "Sell",
        }
    }

    pub(crate) fn from_saxo_buy_sell(saxo_side: &str) -> GatewayResult<Side> {
        match saxo_side {
            "Buy" => Ok(Side::Buy),
            "Sell" => Ok(Side::Sell),
            other => Err(GatewayError::InvalidValue {
                field: "buy_sell".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown Saxo buy/sell: {other}"),
            }),
        }
    }

    // =========================================================================
    // Duration (Time-in-Force) Mapping
    // =========================================================================

    fn to_saxo_duration(tif: tektii_gateway_core::models::TimeInForce) -> SaxoOrderDuration {
        let duration_type = match tif {
            tektii_gateway_core::models::TimeInForce::Gtc => "GoodTillCancel",
            tektii_gateway_core::models::TimeInForce::Day => "DayOrder",
            tektii_gateway_core::models::TimeInForce::Ioc => "ImmediateOrCancel",
            tektii_gateway_core::models::TimeInForce::Fok => "FillOrKill",
        };
        SaxoOrderDuration {
            duration_type: duration_type.to_string(),
        }
    }

    pub(crate) fn from_saxo_duration(
        duration: &SaxoDuration,
    ) -> GatewayResult<tektii_gateway_core::models::TimeInForce> {
        match duration.duration_type.as_str() {
            "GoodTillCancel" | "GoodTillDate" => Ok(tektii_gateway_core::models::TimeInForce::Gtc),
            "DayOrder" => Ok(tektii_gateway_core::models::TimeInForce::Day),
            "ImmediateOrCancel" => Ok(tektii_gateway_core::models::TimeInForce::Ioc),
            "FillOrKill" => Ok(tektii_gateway_core::models::TimeInForce::Fok),
            other => Err(GatewayError::InvalidValue {
                field: "duration_type".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown Saxo duration type: {other}"),
            }),
        }
    }

    // =========================================================================
    // Order Status Mapping
    // =========================================================================

    pub(crate) fn from_saxo_order_status(
        status: &str,
    ) -> GatewayResult<tektii_gateway_core::models::OrderStatus> {
        match status {
            "Working" => Ok(tektii_gateway_core::models::OrderStatus::Open),
            "Filled" | "FinalFill" => Ok(tektii_gateway_core::models::OrderStatus::Filled),
            "Cancelled" | "Canceled" => Ok(tektii_gateway_core::models::OrderStatus::Cancelled),
            "Rejected" => Ok(tektii_gateway_core::models::OrderStatus::Rejected),
            "Expired" => Ok(tektii_gateway_core::models::OrderStatus::Expired),
            "LockedPlacementPending" | "Placed" | "NotWorking" => {
                Ok(tektii_gateway_core::models::OrderStatus::Pending)
            }
            "PartiallyFilled" => Ok(tektii_gateway_core::models::OrderStatus::PartiallyFilled),
            other => Err(GatewayError::InvalidValue {
                field: "status".to_string(),
                provided: Some(other.to_string()),
                message: format!("Unknown Saxo order status: {other}"),
            }),
        }
    }

    // =========================================================================
    // Translation Helpers
    // =========================================================================

    fn translate_order(&self, saxo_order: &SaxoOrder) -> GatewayResult<Order> {
        let symbol = self.resolve_uic(saxo_order.uic, &saxo_order.asset_type)?;
        let side = Self::from_saxo_buy_sell(&saxo_order.buy_sell)?;
        let order_type = Self::from_saxo_order_type(&saxo_order.order_type)?;
        let status = Self::from_saxo_order_status(&saxo_order.status)?;

        let tif = saxo_order
            .duration
            .as_ref()
            .map(Self::from_saxo_duration)
            .transpose()?
            .unwrap_or(tektii_gateway_core::models::TimeInForce::Gtc);

        let quantity =
            rust_decimal::prelude::FromPrimitive::from_f64(saxo_order.amount).unwrap_or_default();
        let filled_quantity =
            rust_decimal::prelude::FromPrimitive::from_f64(saxo_order.filled_amount)
                .unwrap_or_default();
        let remaining = quantity - filled_quantity;

        let (limit_price, stop_price) = match order_type {
            tektii_gateway_core::models::OrderType::Limit => (
                saxo_order
                    .price
                    .and_then(rust_decimal::prelude::FromPrimitive::from_f64),
                None,
            ),
            tektii_gateway_core::models::OrderType::Stop => (
                None,
                saxo_order
                    .price
                    .and_then(rust_decimal::prelude::FromPrimitive::from_f64),
            ),
            tektii_gateway_core::models::OrderType::StopLimit => (
                saxo_order
                    .stop_price
                    .and_then(rust_decimal::prelude::FromPrimitive::from_f64),
                saxo_order
                    .price
                    .and_then(rust_decimal::prelude::FromPrimitive::from_f64),
            ),
            _ => (None, None),
        };

        let (stop_loss, take_profit) = Self::extract_sl_tp_from_related(saxo_order);

        let trailing_distance = saxo_order
            .trailing_stop_distance_to_market
            .and_then(rust_decimal::prelude::FromPrimitive::from_f64);

        let now = chrono::Utc::now();

        Ok(Order {
            id: saxo_order.order_id.clone(),
            client_order_id: saxo_order.external_reference.clone(),
            symbol,
            side,
            order_type,
            time_in_force: tif,
            quantity,
            filled_quantity,
            remaining_quantity: remaining,
            limit_price,
            stop_price,
            average_fill_price: saxo_order
                .average_fill_price
                .and_then(rust_decimal::prelude::FromPrimitive::from_f64),
            status,
            stop_loss,
            take_profit,
            trailing_distance,
            trailing_type: None,
            reject_reason: None,
            position_id: None,
            reduce_only: None,
            post_only: None,
            hidden: None,
            display_quantity: None,
            oco_group_id: None,
            correlation_id: None,
            created_at: now,
            updated_at: now,
        })
    }

    fn extract_sl_tp_from_related(order: &SaxoOrder) -> (Option<Decimal>, Option<Decimal>) {
        let mut stop_loss = None;
        let mut take_profit = None;

        if let Some(ref related) = order.related_orders {
            for rel in related {
                match rel.order_type.as_str() {
                    "StopIfTraded" | "Stop" => {
                        stop_loss = rel
                            .price
                            .and_then(rust_decimal::prelude::FromPrimitive::from_f64);
                    }
                    "Limit" => {
                        take_profit = rel
                            .price
                            .and_then(rust_decimal::prelude::FromPrimitive::from_f64);
                    }
                    _ => {}
                }
            }
        }

        if let Some(ref related_open) = order.related_open_orders {
            for rel in related_open {
                match rel.order_type.as_str() {
                    "StopIfTraded" | "Stop" => {
                        stop_loss = rel
                            .price
                            .and_then(rust_decimal::prelude::FromPrimitive::from_f64);
                    }
                    "Limit" => {
                        take_profit = rel
                            .price
                            .and_then(rust_decimal::prelude::FromPrimitive::from_f64);
                    }
                    _ => {}
                }
            }
        }

        (stop_loss, take_profit)
    }

    fn translate_position(&self, pos: &SaxoNetPosition) -> GatewayResult<Position> {
        let base = &pos.net_position_base;
        let symbol = self.resolve_uic(base.uic, &base.asset_type)?;

        let (side, quantity) = if base.amount >= 0.0 {
            (
                PositionSide::Long,
                rust_decimal::prelude::FromPrimitive::from_f64(base.amount).unwrap_or_default(),
            )
        } else {
            (
                PositionSide::Short,
                rust_decimal::prelude::FromPrimitive::from_f64(base.amount.abs())
                    .unwrap_or_default(),
            )
        };

        let now = chrono::Utc::now();

        Ok(Position {
            id: pos.net_position_id.clone(),
            symbol,
            side,
            quantity,
            average_entry_price: rust_decimal::prelude::FromPrimitive::from_f64(
                base.average_open_price,
            )
            .unwrap_or_default(),
            current_price: rust_decimal::prelude::FromPrimitive::from_f64(base.current_price)
                .unwrap_or_default(),
            unrealized_pnl: rust_decimal::prelude::FromPrimitive::from_f64(
                base.profit_loss_on_trade,
            )
            .unwrap_or_default(),
            realized_pnl: Decimal::ZERO,
            margin_mode: None,
            leverage: None,
            liquidation_price: None,
            opened_at: now,
            updated_at: now,
        })
    }

    fn translate_quote(&self, resp: &SaxoInfoPriceResponse) -> GatewayResult<Quote> {
        let symbol = self.resolve_uic(resp.uic, &resp.asset_type)?;
        let bid =
            rust_decimal::prelude::FromPrimitive::from_f64(resp.quote.bid).unwrap_or_default();
        let ask =
            rust_decimal::prelude::FromPrimitive::from_f64(resp.quote.ask).unwrap_or_default();
        let mid = rust_decimal::prelude::FromPrimitive::from_f64(resp.quote.mid)
            .unwrap_or_else(|| (bid + ask) / Decimal::from(2));

        Ok(Quote {
            symbol,
            provider: "saxo".to_string(),
            bid,
            bid_size: None,
            ask,
            ask_size: None,
            last: mid,
            volume: None,
            timestamp: chrono::Utc::now(),
        })
    }

    fn translate_bar(data_point: &SaxoChartDataPoint, symbol: &str, timeframe: Timeframe) -> Bar {
        let timestamp = chrono::DateTime::parse_from_rfc3339(&data_point.time)
            .or_else(|_| {
                chrono::DateTime::parse_from_rfc3339(&format!(
                    "{}Z",
                    data_point.time.trim_end_matches('Z')
                ))
            })
            .map_or_else(|_| chrono::Utc::now(), |dt| dt.with_timezone(&chrono::Utc));

        Bar {
            symbol: symbol.to_string(),
            provider: "saxo".to_string(),
            timeframe,
            open: rust_decimal::prelude::FromPrimitive::from_f64(data_point.mid_open())
                .unwrap_or_default(),
            high: rust_decimal::prelude::FromPrimitive::from_f64(data_point.mid_high())
                .unwrap_or_default(),
            low: rust_decimal::prelude::FromPrimitive::from_f64(data_point.mid_low())
                .unwrap_or_default(),
            close: rust_decimal::prelude::FromPrimitive::from_f64(data_point.mid_close())
                .unwrap_or_default(),
            volume: rust_decimal::prelude::FromPrimitive::from_f64(data_point.volume)
                .unwrap_or_default(),
            timestamp,
        }
    }

    fn translate_trade(&self, closed: &SaxoClosedPosition) -> GatewayResult<Trade> {
        let detail = &closed.closed_position;
        let symbol = self.resolve_uic(detail.uic, &detail.asset_type)?;
        let side = Self::from_saxo_buy_sell(&detail.buy_sell)?;

        let timestamp = detail
            .execution_time_close
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map_or_else(chrono::Utc::now, |dt| dt.with_timezone(&chrono::Utc));

        Ok(Trade {
            id: closed.closed_position_id.clone(),
            order_id: String::new(),
            symbol,
            side,
            quantity: rust_decimal::prelude::FromPrimitive::from_f64(detail.amount)
                .unwrap_or_default(),
            price: rust_decimal::prelude::FromPrimitive::from_f64(detail.close_price)
                .unwrap_or_default(),
            commission: rust_decimal::prelude::FromPrimitive::from_f64(detail.costs_total)
                .unwrap_or_default(),
            commission_currency: String::new(),
            is_maker: None,
            timestamp,
        })
    }

    // =========================================================================
    // Pre-Trade Margin Precheck
    // =========================================================================

    async fn precheck_order(&self, request: &SaxoOrderRequest) -> GatewayResult<()> {
        let result: Result<SaxoPrecheckResponse, SaxoError> = self
            .http_client
            .post("/trade/v2/orders/precheck", request)
            .await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        if resp.pre_check_result == "Ok" {
            debug!(
                margin_utilization_pct = resp.margin_utilization_pct,
                estimated_cash = resp.estimated_cash_required,
                "Saxo precheck passed"
            );
            return Ok(());
        }

        let (error_code, message) = match &resp.error_info {
            Some(info) => (
                info.error_code.clone(),
                info.message
                    .clone()
                    .unwrap_or_else(|| resp.pre_check_result.clone()),
            ),
            None => (None, resp.pre_check_result.clone()),
        };

        warn!(
            error_code = ?error_code,
            message = %message,
            "Saxo precheck rejected order"
        );

        Err(GatewayError::OrderRejected {
            reason: message,
            reject_code: Some(
                crate::error::map_saxo_reject_code(error_code.as_deref()).to_string(),
            ),
            details: resp
                .margin_utilization_pct
                .map(|pct| serde_json::json!({ "margin_utilization_pct": pct })),
        })
    }

    // =========================================================================
    // Order Request Building
    // =========================================================================

    fn build_order_request(
        &self,
        request: &tektii_gateway_core::models::OrderRequest,
    ) -> GatewayResult<SaxoOrderRequest> {
        let info = self.resolve_symbol(&request.symbol)?;
        let saxo_type = Self::to_saxo_order_type(request.order_type);

        let duration = if request.order_type == tektii_gateway_core::models::OrderType::Market {
            SaxoOrderDuration {
                duration_type: "FillOrKill".to_string(),
            }
        } else {
            Self::to_saxo_duration(request.time_in_force)
        };

        let order_price = request
            .limit_price
            .or(request.stop_price)
            .and_then(|d| d.to_f64());

        let stop_limit_price =
            if request.order_type == tektii_gateway_core::models::OrderType::StopLimit {
                request.limit_price.and_then(|d| d.to_f64())
            } else {
                None
            };

        let trailing_stop_distance = request.trailing_distance.and_then(|d| d.to_f64());

        let external_reference = request.client_order_id.as_ref().map(|id| {
            if id.len() > MAX_EXTERNAL_REFERENCE_LEN {
                id[..MAX_EXTERNAL_REFERENCE_LEN].to_string()
            } else {
                id.clone()
            }
        });

        let related_orders = Self::build_related_orders(request, info);

        Ok(SaxoOrderRequest {
            uic: info.uic,
            asset_type: info.asset_type.clone(),
            buy_sell: Self::to_saxo_buy_sell(request.side).to_string(),
            amount: request.quantity.to_f64().unwrap_or(0.0),
            order_type: saxo_type.to_string(),
            manual_order: false,
            account_key: self.account_key.clone(),
            order_duration: duration,
            order_price,
            stop_limit_price,
            trailing_stop_distance_to_market: trailing_stop_distance,
            trailing_stop_step: None,
            external_reference,
            orders: related_orders,
        })
    }

    fn build_related_orders(
        request: &tektii_gateway_core::models::OrderRequest,
        info: &super::instruments::SaxoInstrumentInfo,
    ) -> Option<Vec<SaxoRelatedOrderRequest>> {
        let has_sl = request.stop_loss.is_some();
        let has_tp = request.take_profit.is_some();

        if !has_sl && !has_tp {
            return None;
        }

        let opposite_side = match request.side {
            Side::Buy => "Sell",
            Side::Sell => "Buy",
        };
        let amount = request.quantity.to_f64().unwrap_or(0.0);

        let mut related = Vec::new();

        if let Some(sl) = request.stop_loss {
            related.push(SaxoRelatedOrderRequest {
                order_type: "StopIfTraded".to_string(),
                order_price: sl.to_f64(),
                order_duration: SaxoOrderDuration {
                    duration_type: "GoodTillCancel".to_string(),
                },
                buy_sell: opposite_side.to_string(),
                amount,
            });
        }

        if let Some(tp) = request.take_profit {
            related.push(SaxoRelatedOrderRequest {
                order_type: "Limit".to_string(),
                order_price: tp.to_f64(),
                order_duration: SaxoOrderDuration {
                    duration_type: "GoodTillCancel".to_string(),
                },
                buy_sell: opposite_side.to_string(),
                amount,
            });
        }

        let _ = info;

        Some(related)
    }

    // =========================================================================
    // Timeframe Mapping
    // =========================================================================

    fn to_saxo_horizon(params: &BarParams) -> u32 {
        match params.timeframe {
            Timeframe::OneMinute => 1,
            Timeframe::TwoMinutes => 2,
            Timeframe::FiveMinutes => 5,
            Timeframe::TenMinutes => 10,
            Timeframe::FifteenMinutes => 15,
            Timeframe::ThirtyMinutes => 30,
            Timeframe::OneHour => 60,
            Timeframe::TwoHours => 120,
            Timeframe::FourHours => 240,
            Timeframe::TwelveHours => 720,
            Timeframe::OneDay => 1440,
            Timeframe::OneWeek => 10080,
        }
    }
}

// ============================================================================
// Decimal helper
// ============================================================================

trait DecimalToF64 {
    fn to_f64(&self) -> Option<f64>;
}

impl DecimalToF64 for Decimal {
    fn to_f64(&self) -> Option<f64> {
        use std::str::FromStr;
        f64::from_str(&self.to_string()).ok()
    }
}

// ============================================================================
// TradingAdapter Implementation
// ============================================================================

#[async_trait]
impl TradingAdapter for SaxoAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &self.capabilities
    }

    fn platform(&self) -> TradingPlatform {
        self.platform
    }

    fn provider_name(&self) -> &'static str {
        "saxo"
    }

    // =========================================================================
    // Account
    // =========================================================================

    #[instrument(skip(self), name = "saxo_get_account")]
    async fn get_account(&self) -> GatewayResult<Account> {
        self.check_circuit_breaker().await?;

        let result: Result<SaxoBalanceResponse, SaxoError> =
            self.http_client.get("/port/v1/balances/me").await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        let equity =
            rust_decimal::prelude::FromPrimitive::from_f64(resp.total_value).unwrap_or_default();
        let balance =
            rust_decimal::prelude::FromPrimitive::from_f64(resp.cash_balance).unwrap_or_default();

        Ok(Account {
            balance,
            equity,
            margin_used: rust_decimal::prelude::FromPrimitive::from_f64(
                resp.margin_used_by_current_positions,
            )
            .unwrap_or_default(),
            margin_available: rust_decimal::prelude::FromPrimitive::from_f64(resp.margin_available)
                .unwrap_or_default(),
            unrealized_pnl: rust_decimal::prelude::FromPrimitive::from_f64(
                resp.unrealized_positions_value,
            )
            .unwrap_or_default(),
            currency: resp.currency,
        })
    }

    // =========================================================================
    // Orders
    // =========================================================================

    #[instrument(skip(self, request), name = "saxo_submit_order")]
    async fn submit_order(
        &self,
        request: &tektii_gateway_core::models::OrderRequest,
    ) -> GatewayResult<OrderHandle> {
        self.check_circuit_breaker().await?;

        let saxo_request = self.build_order_request(request)?;

        if self.precheck_enabled {
            self.precheck_order(&saxo_request).await?;
        }

        let result: Result<SaxoOrderResponse, SaxoError> = self
            .http_client
            .post("/trade/v2/orders", &saxo_request)
            .await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        let status = if request.order_type == tektii_gateway_core::models::OrderType::Market {
            tektii_gateway_core::models::OrderStatus::Filled
        } else {
            tektii_gateway_core::models::OrderStatus::Open
        };

        Ok(OrderHandle {
            id: resp.order_id,
            client_order_id: request.client_order_id.clone(),
            correlation_id: None,
            status,
        })
    }

    #[instrument(skip(self), name = "saxo_get_order")]
    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        self.check_circuit_breaker().await?;

        let result: Result<SaxoOrdersResponse, SaxoError> =
            self.http_client.get("/port/v1/orders/me").await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        resp.data
            .iter()
            .find(|o| o.order_id == order_id)
            .map(|o| self.translate_order(o))
            .transpose()?
            .ok_or_else(|| GatewayError::OrderNotFound {
                id: order_id.to_string(),
            })
    }

    #[instrument(skip(self, params), name = "saxo_get_orders")]
    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.check_circuit_breaker().await?;

        let result: Result<SaxoOrdersResponse, SaxoError> =
            self.http_client.get("/port/v1/orders/me").await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        let mut orders: Vec<Order> = resp
            .data
            .iter()
            .filter_map(|o| self.translate_order(o).ok())
            .collect();

        if let Some(ref symbol) = params.symbol {
            orders.retain(|o| &o.symbol == symbol);
        }
        if let Some(ref statuses) = params.status {
            orders.retain(|o| statuses.contains(&o.status));
        }
        if let Some(ref side) = params.side {
            orders.retain(|o| &o.side == side);
        }
        if let Some(ref client_id) = params.client_order_id {
            orders.retain(|o| o.client_order_id.as_deref() == Some(client_id));
        }

        Ok(orders)
    }

    #[instrument(skip(self, params), name = "saxo_get_order_history")]
    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.get_orders(params).await
    }

    #[instrument(skip(self, request), name = "saxo_modify_order")]
    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        self.check_circuit_breaker().await?;

        let modify_request = SaxoModifyOrderRequest {
            account_key: self.account_key.clone(),
            amount: request.quantity.and_then(|d| d.to_f64()),
            order_price: request
                .limit_price
                .or(request.stop_price)
                .and_then(|d| d.to_f64()),
            order_duration: None,
            trailing_stop_distance_to_market: request.trailing_distance.and_then(|d| d.to_f64()),
            trailing_stop_step: None,
        };

        let path = format!("/trade/v2/orders/{order_id}");
        let result: Result<serde_json::Value, SaxoError> =
            self.http_client.patch(&path, &modify_request).await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        result.map_err(GatewayError::from)?;

        let order = self.get_order(order_id).await?;
        Ok(ModifyOrderResult {
            order,
            previous_order_id: None,
        })
    }

    #[instrument(skip(self), name = "saxo_cancel_order")]
    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        self.check_circuit_breaker().await?;

        let path = format!(
            "/trade/v2/orders/{order_id}?AccountKey={}",
            self.account_key
        );
        let result = self.http_client.delete(&path).await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        result.map_err(GatewayError::from)?;

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

    #[instrument(skip(self, params), name = "saxo_get_trades")]
    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        self.check_circuit_breaker().await?;

        let path = format!("/port/v1/closedpositions?ClientKey={}", self.client_key);

        let result: Result<SaxoClosedPositionsResponse, SaxoError> =
            self.http_client.get(&path).await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        let mut trades: Vec<Trade> = resp
            .data
            .iter()
            .filter_map(|t| self.translate_trade(t).ok())
            .collect();

        if let Some(ref symbol) = params.symbol {
            trades.retain(|t| &t.symbol == symbol);
        }

        Ok(trades)
    }

    // =========================================================================
    // Positions
    // =========================================================================

    #[instrument(skip(self), name = "saxo_get_positions")]
    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
        self.check_circuit_breaker().await?;

        let result: Result<SaxoNetPositionsResponse, SaxoError> =
            self.http_client.get("/port/v1/netpositions/me").await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        let mut positions: Vec<Position> = resp
            .data
            .iter()
            .filter_map(|p| self.translate_position(p).ok())
            .collect();

        if let Some(sym) = symbol {
            positions.retain(|p| p.symbol == sym);
        }

        Ok(positions)
    }

    #[instrument(skip(self), name = "saxo_get_position")]
    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        self.check_circuit_breaker().await?;

        let result: Result<SaxoNetPositionsResponse, SaxoError> =
            self.http_client.get("/port/v1/netpositions/me").await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        resp.data
            .iter()
            .find(|p| p.net_position_id == position_id)
            .map(|p| self.translate_position(p))
            .transpose()?
            .ok_or_else(|| GatewayError::PositionNotFound {
                id: position_id.to_string(),
            })
    }

    #[instrument(skip(self, request), name = "saxo_close_position")]
    async fn close_position(
        &self,
        position_id: &str,
        request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        self.check_circuit_breaker().await?;

        let position = self.get_position(position_id).await?;
        let info = self.resolve_symbol(&position.symbol)?;

        let close_side = match position.side {
            PositionSide::Long => "Sell",
            PositionSide::Short => "Buy",
        };

        let close_amount = request
            .quantity
            .and_then(|d| d.to_f64())
            .unwrap_or_else(|| position.quantity.to_f64().unwrap_or(0.0));

        let close_request = SaxoOrderRequest {
            uic: info.uic,
            asset_type: info.asset_type.clone(),
            buy_sell: close_side.to_string(),
            amount: close_amount,
            order_type: "Market".to_string(),
            manual_order: false,
            account_key: self.account_key.clone(),
            order_duration: SaxoOrderDuration {
                duration_type: "FillOrKill".to_string(),
            },
            order_price: None,
            stop_limit_price: None,
            trailing_stop_distance_to_market: None,
            trailing_stop_step: None,
            external_reference: None,
            orders: None,
        };

        let result: Result<SaxoOrderResponse, SaxoError> = self
            .http_client
            .post("/trade/v2/orders", &close_request)
            .await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        Ok(OrderHandle {
            id: resp.order_id,
            client_order_id: None,
            correlation_id: None,
            status: tektii_gateway_core::models::OrderStatus::Filled,
        })
    }

    // =========================================================================
    // Market Data
    // =========================================================================

    #[instrument(skip(self), name = "saxo_get_quote")]
    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        self.check_circuit_breaker().await?;

        let info = self.resolve_symbol(symbol)?;
        let path = format!(
            "/trade/v1/infoprices?Uic={}&AssetType={}&FieldGroups=Quote",
            info.uic, info.asset_type
        );

        let result: Result<SaxoInfoPriceResponse, SaxoError> = self.http_client.get(&path).await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;
        self.translate_quote(&resp)
    }

    #[instrument(skip(self, params), name = "saxo_get_bars")]
    async fn get_bars(&self, symbol: &str, params: &BarParams) -> GatewayResult<Vec<Bar>> {
        self.check_circuit_breaker().await?;

        let info = self.resolve_symbol(symbol)?;
        let horizon = Self::to_saxo_horizon(params);

        let mut path = format!(
            "/chart/v3/charts?Uic={}&AssetType={}&Horizon={}",
            info.uic, info.asset_type, horizon
        );

        if let Some(limit) = params.limit {
            path = format!("{path}&Count={limit}");
        }

        let result: Result<SaxoChartResponse, SaxoError> = self.http_client.get(&path).await;

        if let Err(ref err) = result {
            let api_err = SaxoError::to_gateway_error_ref(err);
            self.record_if_outage(&api_err).await;
        }

        let resp = result.map_err(GatewayError::from)?;

        let normalized_symbol = self.resolve_uic(info.uic, &info.asset_type)?;

        Ok(resp
            .data
            .iter()
            .map(|dp| Self::translate_bar(dp, &normalized_symbol, params.timeframe))
            .collect())
    }

    // =========================================================================
    // Capabilities & Status
    // =========================================================================

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Ok(self.capabilities.capabilities())
    }

    #[instrument(skip(self), name = "saxo_get_connection_status")]
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
                warn!(error = %e, "Saxo connection check failed");
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn to_saxo_order_type_market() {
        assert_eq!(
            SaxoAdapter::to_saxo_order_type(tektii_gateway_core::models::OrderType::Market),
            "Market"
        );
    }

    #[test]
    fn to_saxo_order_type_limit() {
        assert_eq!(
            SaxoAdapter::to_saxo_order_type(tektii_gateway_core::models::OrderType::Limit),
            "Limit"
        );
    }

    #[test]
    fn to_saxo_order_type_stop() {
        assert_eq!(
            SaxoAdapter::to_saxo_order_type(tektii_gateway_core::models::OrderType::Stop),
            "StopIfTraded"
        );
    }

    #[test]
    fn to_saxo_order_type_trailing_stop() {
        assert_eq!(
            SaxoAdapter::to_saxo_order_type(tektii_gateway_core::models::OrderType::TrailingStop),
            "TrailingStopIfTraded"
        );
    }

    #[test]
    fn from_saxo_order_type_roundtrip() {
        assert_eq!(
            SaxoAdapter::from_saxo_order_type("Market").unwrap(),
            tektii_gateway_core::models::OrderType::Market
        );
        assert_eq!(
            SaxoAdapter::from_saxo_order_type("Limit").unwrap(),
            tektii_gateway_core::models::OrderType::Limit
        );
        assert_eq!(
            SaxoAdapter::from_saxo_order_type("StopIfTraded").unwrap(),
            tektii_gateway_core::models::OrderType::Stop
        );
    }

    #[test]
    fn from_saxo_order_type_unknown_returns_error() {
        assert!(SaxoAdapter::from_saxo_order_type("UnknownType").is_err());
    }

    #[test]
    fn to_saxo_buy_sell_mapping() {
        assert_eq!(SaxoAdapter::to_saxo_buy_sell(Side::Buy), "Buy");
        assert_eq!(SaxoAdapter::to_saxo_buy_sell(Side::Sell), "Sell");
    }

    #[test]
    fn from_saxo_buy_sell_mapping() {
        assert_eq!(SaxoAdapter::from_saxo_buy_sell("Buy").unwrap(), Side::Buy);
        assert_eq!(SaxoAdapter::from_saxo_buy_sell("Sell").unwrap(), Side::Sell);
    }

    #[test]
    fn from_saxo_buy_sell_unknown_returns_error() {
        assert!(SaxoAdapter::from_saxo_buy_sell("Hold").is_err());
    }

    #[test]
    fn decimal_to_f64_conversion() {
        assert!((dec!(1.23456).to_f64().unwrap() - 1.234_56).abs() < f64::EPSILON);
    }
}
