// False positive from utoipa OpenApi derive macro
#![allow(clippy::needless_for_each)]

//! REST API routes and handlers for the gateway.
//!
//! This module provides a router factory and all handler implementations
//! for the unified trading gateway API.

// Errors are documented in utoipa path macro responses, not Rust docs
#![allow(clippy::missing_errors_doc)]

use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use tracing::{info, instrument};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::circuit_breaker::CircuitBreakerSnapshot;
use crate::error::{ApiError, ErrorCode, GatewayError};
use crate::models::{
    Account, AssetClass, Bar, BarParams, CancelAllResult, CancelOrderResult, Capabilities,
    ClosePositionRequest, ConnectionStatus, DetailedHealthStatus, HealthStatus, MarginMode,
    ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams, OrderRequest,
    OrderStatus, OrderType, OverallStatus, Position, PositionMode, PositionSide, ProviderHealth,
    Quote, ReadyStatus, RejectReason, Side, Timeframe, Trade, TradeQueryParams, TradingPlatform,
    TrailingType,
};
use crate::websocket::messages::{
    AccountEventType, ConnectionEventType, OrderEventType, PingPayload, PositionEventType,
    RateLimitEventType, TradeEventType, WsErrorCode, WsMessage,
};

use super::extractors::{OptionalValidatedJson, ValidatedJson};
use super::state::GatewayState;

// ============================================================================
// Handler-defined types
// ============================================================================

/// Query parameters for cancelling all orders.
#[derive(Debug, Clone, Default, serde::Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
pub struct CancelAllOrdersParams {
    /// Filter by symbol (None = cancel all orders)
    pub symbol: Option<String>,
}

/// Request body for get positions with optional symbol filter.
#[derive(Debug, Clone, Default, serde::Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
pub struct GetPositionsParams {
    /// Optional symbol to filter positions.
    pub symbol: Option<String>,
}

/// Query parameters for closing all positions.
#[derive(Debug, Clone, Default, serde::Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
pub struct CloseAllPositionsParams {
    /// Filter by symbol (None = close all positions)
    pub symbol: Option<String>,
}

// ============================================================================
// Account
// ============================================================================

/// Get account information.
///
/// Returns current balances, margin, equity, and buying power.
#[utoipa::path(
    get,
    path = "/account",
    responses(
        (status = 200, description = "Account information", body = Account),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "account"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_account(
    State(state): State<GatewayState>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let account = adapter.get_account().await?;
    Ok(Json(account))
}

// ============================================================================
// Orders
// ============================================================================

/// Submit a new order.
///
/// Returns an order handle with the assigned order ID.
#[utoipa::path(
    post,
    path = "/orders",
    request_body = OrderRequest,
    responses(
        (status = 201, description = "Order submitted", body = OrderHandle),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 422, description = "Order rejected", body = ApiError),
        (status = 501, description = "Operation not supported by provider", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "orders"
)]
#[instrument(skip(state, request), fields(provider, symbol, correlation_id))]
pub async fn submit_order(
    State(state): State<GatewayState>,
    ValidatedJson(request): ValidatedJson<OrderRequest>,
) -> Result<impl IntoResponse, GatewayError> {
    request.validate_semantics()?;

    if state.is_shutting_down() {
        return Err(GatewayError::ShuttingDown);
    }

    let platform = state.platform();
    let correlation_id = uuid::Uuid::new_v4().to_string();
    tracing::Span::current().record("platform", platform.to_string());
    tracing::Span::current().record("symbol", &request.symbol);
    tracing::Span::current().record("correlation_id", &correlation_id);

    let adapter = state.adapter();

    let start = std::time::Instant::now();
    let result = adapter.submit_order(&request).await;
    let elapsed = start.elapsed().as_secs_f64();

    metrics::histogram!(
        "gateway_order_submit_duration_seconds",
        "platform" => platform.header_value(),
    )
    .record(elapsed);

    let mut handle = result?;

    metrics::counter!(
        "gateway_orders_submitted_total",
        "platform" => platform.header_value(),
    )
    .increment(1);

    state.correlation_store().store(&handle.id, &correlation_id);
    handle.correlation_id = Some(correlation_id);

    info!(
        correlation_id = handle.correlation_id.as_deref().unwrap_or(""),
        order_id = %handle.id,
        symbol = %request.symbol,
        side = ?request.side,
        order_type = ?request.order_type,
        quantity = %request.quantity,
        status = ?handle.status,
        "Order submitted"
    );

    Ok((axum::http::StatusCode::CREATED, Json(handle)))
}

/// Get order by ID.
#[utoipa::path(
    get,
    path = "/orders/{order_id}",
    params(
        ("order_id" = String, Path, description = "Order ID")
    ),
    responses(
        (status = 200, description = "Order details", body = Order),
        (status = 404, description = "Order not found", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "orders"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_order(
    State(state): State<GatewayState>,
    Path(order_id): Path<String>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let mut order = adapter.get_order(&order_id).await?;
    order.correlation_id = state.correlation_store().get(&order.id);
    Ok(Json(order))
}

/// List open orders.
#[utoipa::path(
    get,
    path = "/orders",
    params(OrderQueryParams),
    responses(
        (status = 200, description = "List of orders", body = Vec<Order>),
        (status = 400, description = "Invalid query parameters", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "orders"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_orders(
    State(state): State<GatewayState>,
    Query(params): Query<OrderQueryParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let mut orders = adapter.get_orders(&params).await?;
    let store = state.correlation_store();
    for order in &mut orders {
        order.correlation_id = store.get(&order.id);
    }
    Ok(Json(orders))
}

/// Get order history.
#[utoipa::path(
    get,
    path = "/orders/history",
    params(OrderQueryParams),
    responses(
        (status = 200, description = "Order history", body = Vec<Order>),
        (status = 400, description = "Invalid query parameters", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "orders"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_order_history(
    State(state): State<GatewayState>,
    Query(params): Query<OrderQueryParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let mut orders = adapter.get_order_history(&params).await?;
    let store = state.correlation_store();
    for order in &mut orders {
        order.correlation_id = store.get(&order.id);
    }
    Ok(Json(orders))
}

/// Modify an existing order.
#[utoipa::path(
    patch,
    path = "/orders/{order_id}",
    params(
        ("order_id" = String, Path, description = "Order ID")
    ),
    request_body = ModifyOrderRequest,
    responses(
        (status = 200, description = "Order modified", body = ModifyOrderResult),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 404, description = "Order not found", body = ApiError),
        (status = 409, description = "Order not modifiable", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "orders"
)]
#[instrument(skip(state, request), fields(provider))]
pub async fn modify_order(
    State(state): State<GatewayState>,
    Path(order_id): Path<String>,
    ValidatedJson(request): ValidatedJson<ModifyOrderRequest>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let result = adapter.modify_order(&order_id, &request).await?;
    Ok(Json(result))
}

/// Cancel an order.
#[utoipa::path(
    delete,
    path = "/orders/{order_id}",
    params(
        ("order_id" = String, Path, description = "Order ID")
    ),
    responses(
        (status = 200, description = "Order cancelled", body = CancelOrderResult),
        (status = 404, description = "Order not found", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "orders"
)]
#[instrument(skip(state), fields(provider))]
pub async fn cancel_order(
    State(state): State<GatewayState>,
    Path(order_id): Path<String>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let result = adapter.cancel_order(&order_id).await?;
    Ok(Json(result))
}

/// Cancel all orders.
///
/// Cancels all open orders, optionally filtered by symbol.
#[utoipa::path(
    delete,
    path = "/orders",
    params(CancelAllOrdersParams),
    responses(
        (status = 200, description = "Orders cancelled", body = CancelAllResult),
        (status = 400, description = "Invalid request", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "orders"
)]
#[instrument(skip(state), fields(provider, symbol))]
pub async fn cancel_all_orders(
    State(state): State<GatewayState>,
    Query(params): Query<CancelAllOrdersParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    if let Some(ref s) = params.symbol {
        tracing::Span::current().record("symbol", s.as_str());
    }

    let adapter = state.adapter();
    let result = adapter.cancel_all_orders(params.symbol.as_deref()).await?;
    Ok(Json(result))
}

// ============================================================================
// Trades
// ============================================================================

/// Get trade history.
#[utoipa::path(
    get,
    path = "/trades",
    params(TradeQueryParams),
    responses(
        (status = 200, description = "List of trades", body = Vec<Trade>),
        (status = 400, description = "Invalid query parameters", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "trades"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_trades(
    State(state): State<GatewayState>,
    Query(params): Query<TradeQueryParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let trades = adapter.get_trades(&params).await?;
    Ok(Json(trades))
}

// ============================================================================
// Positions
// ============================================================================

/// Get open positions.
#[utoipa::path(
    get,
    path = "/positions",
    params(GetPositionsParams),
    responses(
        (status = 200, description = "List of positions", body = Vec<Position>),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "positions"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_positions(
    State(state): State<GatewayState>,
    Query(params): Query<GetPositionsParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let positions = adapter.get_positions(params.symbol.as_deref()).await?;
    Ok(Json(positions))
}

/// Get position by ID.
#[utoipa::path(
    get,
    path = "/positions/{position_id}",
    params(
        ("position_id" = String, Path, description = "Position ID")
    ),
    responses(
        (status = 200, description = "Position details", body = Position),
        (status = 404, description = "Position not found", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "positions"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_position(
    State(state): State<GatewayState>,
    Path(position_id): Path<String>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let position = adapter.get_position(&position_id).await?;
    Ok(Json(position))
}

/// Close a position.
#[utoipa::path(
    delete,
    path = "/positions/{position_id}",
    params(
        ("position_id" = String, Path, description = "Position ID")
    ),
    request_body(content = Option<ClosePositionRequest>, description = "Optional close parameters"),
    responses(
        (status = 200, description = "Position close order created", body = OrderHandle),
        (status = 404, description = "Position not found", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "positions"
)]
#[instrument(skip(state, request), fields(provider))]
pub async fn close_position(
    State(state): State<GatewayState>,
    Path(position_id): Path<String>,
    OptionalValidatedJson(request): OptionalValidatedJson<ClosePositionRequest>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let close_request = request.unwrap_or_default();
    let handle = adapter.close_position(&position_id, &close_request).await?;
    Ok(Json(handle))
}

/// Close all positions.
///
/// Closes all open positions, optionally filtered by symbol.
#[utoipa::path(
    delete,
    path = "/positions",
    params(CloseAllPositionsParams),
    responses(
        (status = 200, description = "Close orders created", body = Vec<OrderHandle>),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "positions"
)]
#[instrument(skip(state), fields(provider, symbol))]
pub async fn close_all_positions(
    State(state): State<GatewayState>,
    Query(params): Query<CloseAllPositionsParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    if let Some(ref s) = params.symbol {
        tracing::Span::current().record("symbol", s.as_str());
    }

    let adapter = state.adapter();
    let handles = match adapter.close_all_positions(params.symbol.as_deref()).await {
        Ok(handles) => handles,
        // Normalize: if position not found, return empty array instead of 404
        Err(GatewayError::PositionNotFound { .. }) => vec![],
        Err(e) => return Err(e),
    };
    Ok(Json(handles))
}

// ============================================================================
// Market Data
// ============================================================================

/// Get quote for a symbol.
#[utoipa::path(
    get,
    path = "/quotes/{symbol}",
    params(
        ("symbol" = String, Path, description = "Symbol (e.g., AAPL, BTC/USD)")
    ),
    responses(
        (status = 200, description = "Quote data", body = Quote),
        (status = 404, description = "Symbol not found", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "market_data"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_quote(
    State(state): State<GatewayState>,
    Path(symbol): Path<String>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let quote = adapter.get_quote(&symbol).await?;
    Ok(Json(quote))
}

/// Get historical bars for a symbol.
///
/// Returns OHLCV candlestick data for the specified symbol and timeframe.
#[utoipa::path(
    get,
    path = "/bars/{symbol}",
    params(
        ("symbol" = String, Path, description = "Symbol (e.g., S:AAPL, F:EURUSD)"),
        BarParams
    ),
    responses(
        (status = 200, description = "Historical bar data", body = Vec<Bar>),
        (status = 400, description = "Invalid request (bad timeframe or symbol)", body = ApiError),
        (status = 404, description = "Symbol not found", body = ApiError),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "market_data"
)]
#[instrument(skip(state), fields(provider, symbol))]
pub async fn get_bars(
    State(state): State<GatewayState>,
    Path(symbol): Path<String>,
    Query(params): Query<BarParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());
    tracing::Span::current().record("symbol", &symbol);

    let adapter = state.adapter();
    let bars = adapter.get_bars(&symbol, &params).await?;
    Ok(Json(bars))
}

// ============================================================================
// System
// ============================================================================

/// Get provider capabilities.
#[utoipa::path(
    get,
    path = "/capabilities",
    responses(
        (status = 200, description = "Provider capabilities", body = Capabilities),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "system"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_capabilities(
    State(state): State<GatewayState>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let caps = adapter.get_capabilities().await?;
    Ok(Json(caps))
}

/// Get connection status.
#[utoipa::path(
    get,
    path = "/status",
    responses(
        (status = 200, description = "Connection status", body = ConnectionStatus),
        (status = 503, description = "Provider unavailable", body = ApiError)
    ),
    tag = "system"
)]
#[instrument(skip(state), fields(provider))]
pub async fn get_connection_status(
    State(state): State<GatewayState>,
) -> Result<impl IntoResponse, GatewayError> {
    let platform = state.platform();
    tracing::Span::current().record("platform", platform.to_string());

    let adapter = state.adapter();
    let status = adapter.get_connection_status().await?;
    Ok(Json(status))
}

// ============================================================================
// Circuit Breakers
// ============================================================================

/// Status of both circuit breakers.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct CircuitBreakerStatusResponse {
    /// Adapter circuit breaker (provider outage detection).
    pub adapter: CircuitBreakerSnapshot,
    /// Exit order circuit breaker (SL/TP placement failure detection).
    pub exit_order: CircuitBreakerSnapshot,
}

/// Get the status of both circuit breakers.
#[utoipa::path(
    get,
    path = "/circuit-breakers",
    responses(
        (status = 200, description = "Circuit breaker status", body = CircuitBreakerStatusResponse)
    ),
    tag = "operations"
)]
#[instrument(skip(state))]
pub async fn get_circuit_breaker_status(
    State(state): State<GatewayState>,
) -> Result<impl IntoResponse, GatewayError> {
    let adapter_snapshot = state
        .adapter()
        .circuit_breaker_status()
        .await
        .unwrap_or_else(CircuitBreakerSnapshot::closed);

    let exit_snapshot = exit_order_snapshot(&state).await;

    Ok(Json(CircuitBreakerStatusResponse {
        adapter: adapter_snapshot,
        exit_order: exit_snapshot,
    }))
}

/// Reset both circuit breakers to closed state.
///
/// Returns 409 Conflict if either breaker is in cooldown (re-tripped within 60s of last reset).
#[utoipa::path(
    post,
    path = "/circuit-breakers/reset",
    responses(
        (status = 200, description = "Circuit breakers reset", body = CircuitBreakerStatusResponse),
        (status = 409, description = "Reset rejected — cooldown active", body = ApiError)
    ),
    tag = "operations"
)]
#[instrument(skip(state))]
pub async fn reset_circuit_breakers(
    State(state): State<GatewayState>,
) -> Result<impl IntoResponse, GatewayError> {
    let adapter = state.adapter();
    let platform = state.platform();

    // Reset adapter circuit breaker
    let adapter_had_breaker = adapter.circuit_breaker_status().await.is_some();
    if adapter_had_breaker {
        let failure_count = adapter
            .circuit_breaker_status()
            .await
            .map_or(0, |s| s.failure_count);
        adapter.reset_adapter_circuit_breaker().await?;
        tracing::warn!(
            target = "adapter",
            failure_count_at_reset = failure_count,
            %platform,
            "Circuit breaker manually reset via API"
        );
    }

    // Reset exit order circuit breaker
    if let Some(exit_handler) = state.exit_handler_registry().get(&platform) {
        let failure_count = exit_handler.circuit_breaker_failure_count().await;
        exit_handler
            .reset_circuit_breaker()
            .await
            .map_err(|msg| GatewayError::ResetCooldown {
                message: msg.to_string(),
            })?;
        tracing::warn!(
            target = "exit_order",
            failure_count_at_reset = failure_count,
            %platform,
            "Circuit breaker manually reset via API"
        );
    }

    metrics::counter!("gateway_circuit_breaker_resets_total").increment(1);

    // Return current state after reset
    let adapter_snapshot = adapter
        .circuit_breaker_status()
        .await
        .unwrap_or_else(CircuitBreakerSnapshot::closed);
    let exit_snapshot = exit_order_snapshot(&state).await;

    Ok(Json(CircuitBreakerStatusResponse {
        adapter: adapter_snapshot,
        exit_order: exit_snapshot,
    }))
}

/// Helper to build a snapshot of the exit order circuit breaker.
async fn exit_order_snapshot(state: &GatewayState) -> CircuitBreakerSnapshot {
    let platform = state.platform();
    if let Some(exit_handler) = state.exit_handler_registry().get(&platform) {
        let cb_state = exit_handler.circuit_breaker_state().await;
        let failure_count = exit_handler.circuit_breaker_failure_count().await;
        CircuitBreakerSnapshot {
            state: match cb_state {
                crate::exit_management::circuit_breaker::CircuitState::Closed => {
                    "closed".to_string()
                }
                crate::exit_management::circuit_breaker::CircuitState::Open => "open".to_string(),
            },
            failure_count,
        }
    } else {
        CircuitBreakerSnapshot::closed()
    }
}

// ============================================================================
// Health Checks
// ============================================================================

/// Liveness probe endpoint.
///
/// Returns 200 OK if the process is up. Use for Kubernetes liveness probes.
/// This endpoint does not check provider connections or other dependencies.
#[utoipa::path(
    get,
    path = "/livez",
    responses(
        (status = 200, description = "Service is alive", body = HealthStatus)
    ),
    tag = "health"
)]
pub async fn get_livez() -> impl IntoResponse {
    Json(HealthStatus::ok())
}

/// Readiness probe endpoint.
///
/// Returns 200 OK if the adapter is configured and the gateway is not shutting down.
/// Returns 503 Service Unavailable otherwise. Use for Kubernetes readiness probes.
#[utoipa::path(
    get,
    path = "/readyz",
    responses(
        (status = 200, description = "Service is ready", body = ReadyStatus),
        (status = 503, description = "Service not ready", body = ReadyStatus)
    ),
    tag = "health"
)]
#[instrument(skip(state))]
pub async fn get_readyz(State(state): State<GatewayState>) -> impl IntoResponse {
    if state.is_shutting_down() {
        let status = ReadyStatus::not_ready("Gateway is shutting down");
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(status));
    }

    let platform = state.platform();
    let status = ReadyStatus::ready(vec![platform.to_string()]);
    (axum::http::StatusCode::OK, Json(status))
}

/// Detailed health endpoint.
///
/// Returns per-provider connection status. A single provider with stale
/// data is reported as "degraded", not unhealthy — this avoids k8s
/// restart loops from transient broker blips.
///
/// Always returns 200; degraded status is informational.
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Gateway health details", body = DetailedHealthStatus)
    ),
    tag = "health"
)]
pub async fn get_health(State(state): State<GatewayState>) -> impl IntoResponse {
    let platform = state.platform();
    let provider_registry = state.provider_registry();

    let stale = provider_registry
        .get_staleness(platform)
        .await
        .unwrap_or_default();
    let connected = stale.is_empty();

    let provider = ProviderHealth {
        platform: platform.to_string(),
        connected,
        stale_instruments: stale,
    };

    let status = if connected {
        OverallStatus::Connected
    } else {
        OverallStatus::Degraded
    };

    Json(DetailedHealthStatus {
        status,
        providers: vec![provider],
    })
}

// ============================================================================
// OpenAPI + Router Factory
// ============================================================================

/// `OpenAPI` documentation for the gateway API.
#[derive(OpenApi)]
#[openapi(
    components(schemas(
        // ================================================================
        // Core Enums
        // ================================================================
        Side,
        PositionSide,
        OrderType,
        OrderStatus,
        TrailingType,
        MarginMode,
        AssetClass,
        PositionMode,
        RejectReason,
        // ================================================================
        // Account
        // ================================================================
        Account,
        // ================================================================
        // Orders
        // ================================================================
        Order,
        OrderHandle,
        OrderRequest,
        OrderQueryParams,
        ModifyOrderRequest,
        ModifyOrderResult,
        CancelOrderResult,
        CancelAllResult,
        CancelAllOrdersParams,
        // ================================================================
        // Positions
        // ================================================================
        Position,
        ClosePositionRequest,
        CloseAllPositionsParams,
        GetPositionsParams,
        // ================================================================
        // Trades
        // ================================================================
        Trade,
        TradeQueryParams,
        // ================================================================
        // Market Data
        // ================================================================
        Quote,
        Bar,
        BarParams,
        Timeframe,
        // ================================================================
        // System
        // ================================================================
        Capabilities,
        ConnectionStatus,
        // ================================================================
        // Operations
        // ================================================================
        CircuitBreakerSnapshot,
        CircuitBreakerStatusResponse,
        // ================================================================
        // Health
        // ================================================================
        HealthStatus,
        ReadyStatus,
        DetailedHealthStatus,
        ProviderHealth,
        OverallStatus,
        // ================================================================
        // Trading Platform
        // ================================================================
        TradingPlatform,
        // ================================================================
        // Error Types
        // ================================================================
        ApiError, ErrorCode,
        // ================================================================
        // WebSocket Types
        // ================================================================
        WsMessage,
        PingPayload,
        OrderEventType,
        PositionEventType,
        AccountEventType,
        ConnectionEventType,
        RateLimitEventType,
        TradeEventType,
        WsErrorCode,
    )),
    tags(
        (name = "account", description = "Account information"),
        (name = "orders", description = "Order management"),
        (name = "positions", description = "Position management"),
        (name = "trades", description = "Trade history"),
        (name = "market_data", description = "Market data"),
        (name = "system", description = "System status and capabilities"),
        (name = "operations", description = "Operational controls (circuit breakers)"),
        (name = "health", description = "Health and readiness probes")
    ),
    info(
        title = "Tektii Gateway API",
        version = "1.0.0",
        description = "Unified trading gateway API for multiple providers",
        license(name = "Elastic License 2.0", url = "https://www.elastic.co/licensing/elastic-license")
    )
)]
pub struct GatewayApiDoc;

/// Create the gateway API router.
///
/// Returns an `OpenApiRouter` ready to have state attached.
/// Use `.split_for_parts()` to get both the router and the `OpenAPI` specification.
///
/// # Example
///
/// ```ignore
/// use tektii_gateway_core::api::routes::{create_gateway_router, GatewayApiDoc};
/// use tektii_gateway_core::api::state::GatewayState;
///
/// let state = GatewayState::new(registry);
/// let (router, openapi) = create_gateway_router().split_for_parts();
/// let app = router.with_state(state);
/// ```
pub fn create_gateway_router() -> OpenApiRouter<GatewayState> {
    // API routes — nested under /v1
    let api_routes = OpenApiRouter::new()
        // Account
        .routes(routes!(get_account))
        // Orders - note: specific routes must come before parameterized routes
        .routes(routes!(get_order_history))
        .routes(routes!(submit_order, get_orders, cancel_all_orders))
        .routes(routes!(get_order, modify_order, cancel_order))
        // Positions
        .routes(routes!(get_positions, close_all_positions))
        .routes(routes!(get_position, close_position))
        // Trades
        .routes(routes!(get_trades))
        // Market Data
        .routes(routes!(get_quote))
        .routes(routes!(get_bars))
        // System
        .routes(routes!(get_capabilities))
        .routes(routes!(get_connection_status))
        // Operations
        .routes(routes!(get_circuit_breaker_status))
        .routes(routes!(reset_circuit_breakers));

    // Operational routes stay at root
    OpenApiRouter::with_openapi(GatewayApiDoc::openapi())
        .nest("/v1", api_routes)
        .routes(routes!(get_livez))
        .routes(routes!(get_readyz))
        .routes(routes!(get_health))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::test_helpers::test_gateway_state;

    /// Build a minimal router with state for testing.
    fn test_app(state: GatewayState) -> axum::Router {
        let (router, _api) = create_gateway_router().split_for_parts();
        router.with_state(state)
    }

    // ========================================================================
    // 4.5 — Health endpoints
    // ========================================================================

    #[tokio::test]
    async fn livez_returns_200_ok() {
        let state = test_gateway_state();
        let app = test_app(state);

        let resp = app
            .oneshot(Request::get("/livez").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_returns_200_when_adapter_configured() {
        let state = test_gateway_state();
        let app = test_app(state);

        let resp = app
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_returns_503_when_shutting_down() {
        let state = test_gateway_state();
        state.set_shutting_down();
        let app = test_app(state);

        let resp = app
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ready"], false);
        assert_eq!(json["message"], "Gateway is shutting down");
    }

    // ========================================================================
    // 4.1.1 — submit_order shutdown guard
    // ========================================================================

    #[tokio::test]
    async fn submit_order_returns_503_when_shutting_down() {
        let state = test_gateway_state();
        state.set_shutting_down();
        let app = test_app(state);

        let resp = app
            .oneshot(
                Request::post("/v1/orders")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"symbol":"AAPL","side":"BUY","order_type":"MARKET","quantity":"1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "SHUTTING_DOWN");
    }

    // ========================================================================
    // 4.5 — /health detailed endpoint
    // ========================================================================

    #[tokio::test]
    async fn health_returns_connected_with_single_provider() {
        let state = test_gateway_state();
        let app = test_app(state);

        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "connected");
        assert_eq!(json["providers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn openapi_spec_has_v1_prefix_for_api_routes() {
        let (_router, api) = create_gateway_router().split_for_parts();
        let paths: Vec<&str> = api.paths.paths.keys().map(|s| s.as_str()).collect();

        // API routes should have /v1 prefix
        assert!(
            paths.contains(&"/v1/account"),
            "expected /v1/account in spec, got: {paths:?}"
        );
        assert!(
            paths.contains(&"/v1/orders"),
            "expected /v1/orders in spec, got: {paths:?}"
        );
        assert!(
            paths.contains(&"/v1/positions"),
            "expected /v1/positions in spec, got: {paths:?}"
        );

        // Health routes should NOT have /v1 prefix
        assert!(
            paths.contains(&"/livez"),
            "expected /livez at root, got: {paths:?}"
        );
        assert!(
            paths.contains(&"/readyz"),
            "expected /readyz at root, got: {paths:?}"
        );
        assert!(
            paths.contains(&"/health"),
            "expected /health at root, got: {paths:?}"
        );
    }

    #[tokio::test]
    async fn health_returns_200_always() {
        // Even when shutting down, /health returns 200 (degraded is informational)
        let state = test_gateway_state();
        state.set_shutting_down();
        let app = test_app(state);

        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ========================================================================
    // Circuit breaker endpoints
    // ========================================================================

    #[tokio::test]
    async fn get_circuit_breaker_status_returns_closed_by_default() {
        let state = test_gateway_state();
        let app = test_app(state);

        let resp = app
            .oneshot(
                Request::get("/v1/circuit-breakers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["adapter"]["state"], "closed");
        assert_eq!(json["adapter"]["failure_count"], 0);
        assert_eq!(json["exit_order"]["state"], "closed");
        assert_eq!(json["exit_order"]["failure_count"], 0);
    }

    #[tokio::test]
    async fn reset_circuit_breakers_returns_200() {
        let state = test_gateway_state();
        let app = test_app(state);

        let resp = app
            .oneshot(
                Request::post("/v1/circuit-breakers/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["adapter"]["state"], "closed");
        assert_eq!(json["exit_order"]["state"], "closed");
    }
}
