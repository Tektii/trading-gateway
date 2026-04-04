//! Core trading adapter traits and registry.

pub mod capabilities;
pub mod registry;

pub use capabilities::{BracketStrategy, ProviderCapabilities};
pub use registry::AdapterRegistry;

use async_trait::async_trait;

use crate::circuit_breaker::CircuitBreakerSnapshot;
use crate::error::{GatewayError, GatewayResult};
use crate::models::{
    Account, Bar, BarParams, CancelAllResult, CancelOrderResult, Capabilities,
    ClosePositionRequest, ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order,
    OrderHandle, OrderQueryParams, OrderRequest, PlaceOcoOrderRequest, PlaceOcoOrderResponse,
    Position, Quote, Trade, TradeQueryParams, TradingPlatform,
};

/// Core trading adapter trait.
///
/// Unified interface for all trading operations across different providers.
#[async_trait]
pub trait TradingAdapter: Send + Sync {
    // === Identity ===

    /// Get provider capabilities.
    fn capabilities(&self) -> &dyn ProviderCapabilities;

    /// Trading platform identifier.
    fn platform(&self) -> TradingPlatform;

    /// Human-readable provider name.
    fn provider_name(&self) -> &'static str;

    // === Account ===

    /// Get account information.
    async fn get_account(&self) -> GatewayResult<Account>;

    // === Orders ===

    /// Submit a new order.
    async fn submit_order(&self, request: &OrderRequest) -> GatewayResult<OrderHandle>;

    /// Get order by ID.
    async fn get_order(&self, order_id: &str) -> GatewayResult<Order>;

    /// List orders with optional filters.
    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>>;

    /// Get order history (filled, cancelled, etc.).
    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>>;

    /// Modify an existing order.
    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult>;

    /// Cancel an order.
    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult>;

    /// Cancel all orders (optionally filtered by symbol).
    async fn cancel_all_orders(&self, symbol: Option<&str>) -> GatewayResult<CancelAllResult> {
        let params = OrderQueryParams {
            symbol: symbol.map(String::from),
            ..Default::default()
        };
        let orders = self.get_orders(&params).await?;

        let mut cancelled_count = 0u32;
        let mut failed_count = 0u32;
        let mut failed_order_ids = Vec::new();

        for order in orders {
            if self.cancel_order(&order.id).await.is_ok() {
                cancelled_count += 1;
            } else {
                failed_count += 1;
                failed_order_ids.push(order.id);
            }
        }

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

    // === Trades ===

    /// Get trade history.
    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>>;

    // === Positions ===

    /// Get open positions.
    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>>;

    /// Get position by ID.
    async fn get_position(&self, position_id: &str) -> GatewayResult<Position>;

    /// Close a position.
    async fn close_position(
        &self,
        position_id: &str,
        request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle>;

    /// Close all positions (optionally filtered by symbol).
    async fn close_all_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<OrderHandle>> {
        let positions = self.get_positions(symbol).await?;
        let mut handles = Vec::with_capacity(positions.len());

        for position in positions {
            let handle = self
                .close_position(&position.id, &ClosePositionRequest::default())
                .await?;
            handles.push(handle);
        }

        Ok(handles)
    }

    // === OCO Orders ===

    /// Place a new OCO order pair (stop-loss + take-profit).
    async fn place_oco_order(
        &self,
        _request: &PlaceOcoOrderRequest,
    ) -> GatewayResult<PlaceOcoOrderResponse> {
        Err(GatewayError::UnsupportedOperation {
            operation: "place_oco_order".to_string(),
            provider: self.provider_name().to_string(),
        })
    }

    // === Market Data ===

    /// Get current quote for symbol.
    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote>;

    /// Get quotes for multiple symbols.
    async fn get_quotes(&self, symbols: &[&str]) -> GatewayResult<Vec<Quote>> {
        let mut quotes = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            quotes.push(self.get_quote(symbol).await?);
        }
        Ok(quotes)
    }

    /// Get historical bars.
    async fn get_bars(&self, symbol: &str, params: &BarParams) -> GatewayResult<Vec<Bar>>;

    // === Capabilities ===

    /// Get provider capabilities at runtime.
    async fn get_capabilities(&self) -> GatewayResult<Capabilities>;

    // === Connection Status ===

    /// Get current connection status.
    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus>;

    // === Circuit Breaker ===

    /// Get the adapter circuit breaker status as a point-in-time snapshot.
    ///
    /// Returns `None` for adapters without a circuit breaker (e.g. mocks).
    async fn circuit_breaker_status(&self) -> Option<CircuitBreakerSnapshot> {
        None
    }

    /// Reset the adapter circuit breaker to closed state.
    ///
    /// Returns `Err` if the breaker is in cooldown (re-tripped too soon after last reset).
    async fn reset_adapter_circuit_breaker(&self) -> GatewayResult<()> {
        Err(GatewayError::UnsupportedOperation {
            operation: "reset_adapter_circuit_breaker".to_string(),
            provider: self.provider_name().to_string(),
        })
    }
}
