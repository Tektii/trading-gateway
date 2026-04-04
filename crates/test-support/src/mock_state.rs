//! Test helpers for constructing `GatewayState` instances.
//!
//! Provides a minimal gateway state backed by a no-op adapter,
//! suitable for tests that need a `GatewayState` but don't invoke
//! adapter methods.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use tektii_gateway_core::adapter::{
    AdapterRegistry, BracketStrategy, ProviderCapabilities, TradingAdapter,
};
use tektii_gateway_core::api::state::GatewayState;
use tektii_gateway_core::config::ReconnectionConfig;
use tektii_gateway_core::correlation::CorrelationStore;
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::exit_management::ExitHandlerRegistry;
use tektii_gateway_core::models::{
    Account, Bar, BarParams, CancelOrderResult, Capabilities, ClosePositionRequest,
    ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams,
    OrderRequest, Position, PositionMode, Quote, Trade, TradeQueryParams, TradingPlatform,
};
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::websocket::connection::WsConnectionManager;
use tektii_gateway_core::websocket::registry::ProviderRegistry;

/// Create a minimal `GatewayState` for testing with a no-op adapter.
///
/// All adapter methods return `GatewayError::internal("noop")`.
/// Use this when your test needs a `GatewayState` but doesn't exercise
/// the trading adapter.
#[must_use]
pub fn test_gateway_state() -> GatewayState {
    let ws_manager = Arc::new(WsConnectionManager::new());
    let correlation_store = Arc::new(CorrelationStore::new());
    let provider_registry = Arc::new(ProviderRegistry::new(
        ws_manager.clone(),
        SubscriptionFilter::new(&[]),
        CancellationToken::new(),
        ReconnectionConfig::default(),
        correlation_store.clone(),
    ));

    GatewayState::new(
        AdapterRegistry::new(Arc::new(NoopAdapter), TradingPlatform::AlpacaPaper),
        ws_manager,
        provider_registry,
        Arc::new(ExitHandlerRegistry::new()),
        correlation_store,
        None,
    )
}

/// Minimal no-op adapter for tests that need a `GatewayState` but don't call adapter methods.
struct NoopAdapter;

struct NoopCapabilities;

impl ProviderCapabilities for NoopCapabilities {
    fn supports_bracket_orders(&self, _: &str) -> bool {
        false
    }
    fn supports_oco_orders(&self, _: &str) -> bool {
        false
    }
    fn supports_oto_orders(&self, _: &str) -> bool {
        false
    }
    fn bracket_strategy(&self, _: &OrderRequest) -> BracketStrategy {
        BracketStrategy::None
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![],
            supported_order_types: vec![],
            position_mode: PositionMode::Netting,
            features: vec![],
            max_leverage: None,
            rate_limits: None,
        }
    }
}

static NOOP_CAPS: NoopCapabilities = NoopCapabilities;

#[async_trait]
impl TradingAdapter for NoopAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &NOOP_CAPS
    }
    fn platform(&self) -> TradingPlatform {
        TradingPlatform::AlpacaPaper
    }
    fn provider_name(&self) -> &'static str {
        "noop"
    }
    async fn get_account(&self) -> GatewayResult<Account> {
        Err(GatewayError::internal("noop"))
    }
    async fn submit_order(&self, _: &OrderRequest) -> GatewayResult<OrderHandle> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_order(&self, _: &str) -> GatewayResult<Order> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_orders(&self, _: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_order_history(&self, _: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        Err(GatewayError::internal("noop"))
    }
    async fn modify_order(
        &self,
        _: &str,
        _: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        Err(GatewayError::internal("noop"))
    }
    async fn cancel_order(&self, _: &str) -> GatewayResult<CancelOrderResult> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_trades(&self, _: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_positions(&self, _: Option<&str>) -> GatewayResult<Vec<Position>> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_position(&self, _: &str) -> GatewayResult<Position> {
        Err(GatewayError::internal("noop"))
    }
    async fn close_position(
        &self,
        _: &str,
        _: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_quote(&self, _: &str) -> GatewayResult<Quote> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_bars(&self, _: &str, _: &BarParams) -> GatewayResult<Vec<Bar>> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Err(GatewayError::internal("noop"))
    }
    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        Err(GatewayError::internal("noop"))
    }
}
