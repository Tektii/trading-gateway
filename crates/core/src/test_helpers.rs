//! Shared test helpers for core's own unit tests.
//!
//! These exist because core cannot use `tektii-gateway-test-support` as a
//! dev-dependency (circular dependency: test-support depends on core).
//!
//! External tests should use `tektii_gateway_test_support::mock_state::test_gateway_state()`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::adapter::{AdapterRegistry, BracketStrategy, ProviderCapabilities, TradingAdapter};
use crate::api::state::GatewayState;
use crate::config::ReconnectionConfig;
use crate::correlation::CorrelationStore;
use crate::error::{GatewayError, GatewayResult};
use crate::exit_management::ExitHandlerRegistry;
use crate::models::*;
use crate::subscription::filter::SubscriptionFilter;
use crate::websocket::connection::WsConnectionManager;
use crate::websocket::registry::ProviderRegistry;

/// Create a minimal `GatewayState` for testing with a no-op adapter.
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

/// Minimal no-op adapter for tests that don't invoke adapter methods.
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
