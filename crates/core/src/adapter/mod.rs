//! Core trading adapter traits and registry.

pub mod bracket_links;
pub mod capabilities;
pub mod registry;

pub use bracket_links::BracketLinks;
pub use capabilities::{BracketStrategy, ProviderCapabilities};
pub use registry::AdapterRegistry;

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{RwLock, mpsc};

use crate::circuit_breaker::CircuitBreakerSnapshot;
use crate::error::{GatewayError, GatewayResult};
use crate::models::{
    Account, Bar, BarParams, CancelAllResult, CancelOrderResult, Capabilities,
    ClosePositionRequest, ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order,
    OrderHandle, OrderQueryParams, OrderRequest, OrderStatus, PlaceOcoOrderRequest,
    PlaceOcoOrderResponse, Position, Quote, Trade, TradeQueryParams, TradingPlatform,
};
use crate::websocket::provider::ProviderEvent;

/// Core trading adapter trait.
///
/// Unified interface for all trading operations across different providers.
#[async_trait]
pub trait TradingAdapter: Send + Sync {
    /// Get provider capabilities.
    fn capabilities(&self) -> &dyn ProviderCapabilities;

    /// Trading platform identifier.
    fn platform(&self) -> TradingPlatform;

    /// Human-readable provider name.
    fn provider_name(&self) -> &'static str;

    /// Shared handle to the provider's outbound `ProviderEvent` sender, used to
    /// inject events into the same stream the [`WebSocketProvider`] feeds to
    /// connected strategies.
    ///
    /// Most adapters return `None` — their events flow exclusively from the
    /// provider's streaming side. The Oanda adapter returns `Some`: its
    /// synchronous REST order response carries the fill, which the adapter
    /// publishes onto this shared sender (populated by the provider's
    /// `connect`) so strategies observe it without waiting on the transaction
    /// stream; the stream's copy of the same transaction is deduplicated
    /// inside the Oanda crate.
    ///
    /// [`WebSocketProvider`]: crate::websocket::provider::WebSocketProvider
    fn provider_event_sender(
        &self,
    ) -> Option<Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>> {
        None
    }

    /// Entry order that `order_id` exits, for brokers that manage brackets
    /// themselves and report the link in their own payloads.
    ///
    /// Neither Alpaca nor Saxo puts a parent reference on the leg — the link
    /// only ever appears on the entry, pointing down at its legs. An adapter
    /// that supports native brackets therefore records the link when it sees an
    /// entry and answers from that record here; one that does not returns
    /// `None` and the gateway falls back to the exits it synthesized itself.
    ///
    /// `status` is the leg's state in the event being reported. An adapter
    /// releases the link once that state is terminal — after returning it, so
    /// the event reporting the resolution still carries it. Events are the
    /// gateway's primary channel, so this is where a resolved leg's link is
    /// actually reclaimed; REST reads release on the same rule when they happen
    /// to be the observer.
    fn parent_order_id_for(&self, _order_id: &str, _status: OrderStatus) -> Option<String> {
        None
    }

    /// Get account information.
    async fn get_account(&self) -> GatewayResult<Account>;

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

    /// Get trade history.
    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>>;

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

    /// Get provider capabilities at runtime.
    async fn get_capabilities(&self) -> GatewayResult<Capabilities>;

    /// Get current connection status.
    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus>;

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
