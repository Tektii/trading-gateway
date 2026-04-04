//! Mock `TradingAdapter` for integration tests.
//!
//! Pre-populate with orders, positions, quotes, bars, and trades.
//! Configure responses for submit/cancel/close via builder methods.
//! Thread-safe via internal `Mutex`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::adapter::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::models::{
    Account, Bar, BarParams, CancelOrderResult, Capabilities, ClosePositionRequest,
    ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams,
    OrderRequest, OrderStatus, OrderType, Position, PositionMode, Quote, Trade, TradeQueryParams,
    TradingPlatform,
};

use crate::models::{test_account, test_order_handle};

/// A mock trading adapter for integration tests.
///
/// Pre-populate with orders and positions; queries return matching entries.
/// Thread-safe via internal `Mutex`.
pub struct MockTradingAdapter {
    platform: TradingPlatform,
    orders: Mutex<HashMap<String, Order>>,
    positions: Mutex<Vec<Position>>,
    get_orders_error: Mutex<Option<GatewayError>>,
    get_positions_error: Mutex<Option<GatewayError>>,
    // E2E harness fields
    account: Mutex<Account>,
    quotes: Mutex<HashMap<String, Quote>>,
    bars: Mutex<HashMap<String, Vec<Bar>>>,
    trades: Mutex<Vec<Trade>>,
    submitted_orders: Mutex<Vec<OrderRequest>>,
    submit_order_responses: Mutex<VecDeque<GatewayResult<OrderHandle>>>,
    cancelled_orders: Mutex<Vec<String>>,
    cancel_order_responses: Mutex<HashMap<String, GatewayResult<CancelOrderResult>>>,
    close_position_responses: Mutex<HashMap<String, GatewayResult<OrderHandle>>>,
    modified_orders: Mutex<Vec<(String, ModifyOrderRequest)>>,
    modify_order_responses: Mutex<HashMap<String, GatewayResult<ModifyOrderResult>>>,
}

impl MockTradingAdapter {
    /// Create a new mock adapter for the given platform.
    #[must_use]
    pub fn new(platform: TradingPlatform) -> Self {
        Self {
            platform,
            orders: Mutex::new(HashMap::new()),
            positions: Mutex::new(Vec::new()),
            get_orders_error: Mutex::new(None),
            get_positions_error: Mutex::new(None),
            account: Mutex::new(test_account()),
            quotes: Mutex::new(HashMap::new()),
            bars: Mutex::new(HashMap::new()),
            trades: Mutex::new(Vec::new()),
            submitted_orders: Mutex::new(Vec::new()),
            submit_order_responses: Mutex::new(VecDeque::new()),
            cancelled_orders: Mutex::new(Vec::new()),
            cancel_order_responses: Mutex::new(HashMap::new()),
            close_position_responses: Mutex::new(HashMap::new()),
            modified_orders: Mutex::new(Vec::new()),
            modify_order_responses: Mutex::new(HashMap::new()),
        }
    }

    // =========================================================================
    // Builder methods — existing
    // =========================================================================

    /// Add an order that `get_order()` and `get_orders()` will return.
    #[must_use]
    pub fn with_order(self, order: Order) -> Self {
        self.orders
            .lock()
            .expect("lock")
            .insert(order.id.clone(), order);
        self
    }

    /// Add a position that `get_positions()` will return.
    #[must_use]
    pub fn with_position(self, position: Position) -> Self {
        self.positions.lock().expect("lock").push(position);
        self
    }

    /// Make `get_orders()` return an error instead of querying stored orders.
    #[must_use]
    pub fn with_get_orders_error(self, error: GatewayError) -> Self {
        *self.get_orders_error.lock().expect("lock") = Some(error);
        self
    }

    /// Make `get_positions()` return an error instead of querying stored positions.
    #[must_use]
    pub fn with_get_positions_error(self, error: GatewayError) -> Self {
        *self.get_positions_error.lock().expect("lock") = Some(error);
        self
    }

    // =========================================================================
    // Builder methods — E2E harness
    // =========================================================================

    /// Set the account returned by `get_account()`.
    #[must_use]
    pub fn with_account(self, account: Account) -> Self {
        *self.account.lock().expect("lock") = account;
        self
    }

    /// Add a quote returned by `get_quote()` (keyed by symbol).
    #[must_use]
    pub fn with_quote(self, quote: Quote) -> Self {
        self.quotes
            .lock()
            .expect("lock")
            .insert(quote.symbol.clone(), quote);
        self
    }

    /// Set bars returned by `get_bars()` for a symbol.
    #[must_use]
    pub fn with_bars(self, symbol: &str, bars: Vec<Bar>) -> Self {
        self.bars
            .lock()
            .expect("lock")
            .insert(symbol.to_string(), bars);
        self
    }

    /// Add a trade returned by `get_trades()`.
    #[must_use]
    pub fn with_trade(self, trade: Trade) -> Self {
        self.trades.lock().expect("lock").push(trade);
        self
    }

    /// Enqueue a response for the next `submit_order()` call (FIFO).
    #[must_use]
    pub fn with_submit_order_response(self, response: GatewayResult<OrderHandle>) -> Self {
        self.submit_order_responses
            .lock()
            .expect("lock")
            .push_back(response);
        self
    }

    /// Set the response for `cancel_order()` on a specific order ID.
    #[must_use]
    pub fn with_cancel_order_response(
        self,
        order_id: &str,
        response: GatewayResult<CancelOrderResult>,
    ) -> Self {
        self.cancel_order_responses
            .lock()
            .expect("lock")
            .insert(order_id.to_string(), response);
        self
    }

    /// Set the response for `close_position()` on a specific position ID.
    #[must_use]
    pub fn with_close_position_response(
        self,
        position_id: &str,
        response: GatewayResult<OrderHandle>,
    ) -> Self {
        self.close_position_responses
            .lock()
            .expect("lock")
            .insert(position_id.to_string(), response);
        self
    }

    /// Set the response for `modify_order()` on a specific order ID.
    #[must_use]
    pub fn with_modify_order_response(
        self,
        order_id: &str,
        response: GatewayResult<ModifyOrderResult>,
    ) -> Self {
        self.modify_order_responses
            .lock()
            .expect("lock")
            .insert(order_id.to_string(), response);
        self
    }

    // =========================================================================
    // Assertion helpers
    // =========================================================================

    /// Get all order requests that were passed to `submit_order()`.
    pub fn submitted_orders(&self) -> Vec<OrderRequest> {
        self.submitted_orders.lock().expect("lock").clone()
    }

    /// Get all order IDs that were passed to `cancel_order()`.
    pub fn cancelled_orders(&self) -> Vec<String> {
        self.cancelled_orders.lock().expect("lock").clone()
    }

    /// Get all (order_id, request) pairs that were passed to `modify_order()`.
    pub fn modified_orders(&self) -> Vec<(String, ModifyOrderRequest)> {
        self.modified_orders.lock().expect("lock").clone()
    }
}

impl ProviderCapabilities for MockTradingAdapter {
    fn supports_bracket_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn supports_oco_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn supports_oto_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn bracket_strategy(&self, _order: &OrderRequest) -> BracketStrategy {
        BracketStrategy::None
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![],
            supported_order_types: vec![OrderType::Market, OrderType::Limit],
            position_mode: PositionMode::Netting,
            features: vec![],
            max_leverage: None,
            rate_limits: None,
        }
    }
}

#[async_trait]
impl TradingAdapter for MockTradingAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        self
    }

    fn platform(&self) -> TradingPlatform {
        self.platform
    }

    fn provider_name(&self) -> &'static str {
        "mock"
    }

    async fn get_account(&self) -> GatewayResult<Account> {
        Ok(self.account.lock().expect("lock").clone())
    }

    async fn submit_order(&self, request: &OrderRequest) -> GatewayResult<OrderHandle> {
        self.submitted_orders
            .lock()
            .expect("lock")
            .push(request.clone());

        if let Some(response) = self
            .submit_order_responses
            .lock()
            .expect("lock")
            .pop_front()
        {
            return response;
        }

        Ok(test_order_handle())
    }

    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        self.orders
            .lock()
            .expect("lock")
            .get(order_id)
            .cloned()
            .ok_or_else(|| GatewayError::OrderNotFound {
                id: order_id.to_string(),
            })
    }

    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        if let Some(err) = self.get_orders_error.lock().expect("lock").take() {
            return Err(err);
        }

        let orders = self.orders.lock().expect("lock");
        let mut result: Vec<Order> = orders.values().cloned().collect();

        if let Some(ref statuses) = params.status {
            result.retain(|o| statuses.contains(&o.status));
        }
        if let Some(ref symbol) = params.symbol {
            result.retain(|o| &o.symbol == symbol);
        }

        Ok(result)
    }

    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        self.get_orders(params).await
    }

    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        self.modified_orders
            .lock()
            .expect("lock")
            .push((order_id.to_string(), request.clone()));

        if let Some(response) = self
            .modify_order_responses
            .lock()
            .expect("lock")
            .remove(order_id)
        {
            return response;
        }

        // Fallback: apply modifications to the stored order.
        let mut order = self
            .orders
            .lock()
            .expect("lock")
            .get(order_id)
            .cloned()
            .ok_or_else(|| GatewayError::OrderNotFound {
                id: order_id.to_string(),
            })?;

        if let Some(limit_price) = request.limit_price {
            order.limit_price = Some(limit_price);
        }
        if let Some(stop_price) = request.stop_price {
            order.stop_price = Some(stop_price);
        }
        if let Some(quantity) = request.quantity {
            order.quantity = quantity;
        }
        if let Some(stop_loss) = request.stop_loss {
            order.stop_loss = Some(stop_loss);
        }
        if let Some(take_profit) = request.take_profit {
            order.take_profit = Some(take_profit);
        }
        if let Some(trailing_distance) = request.trailing_distance {
            order.trailing_distance = Some(trailing_distance);
        }

        // Update stored order so subsequent queries reflect the change.
        self.orders
            .lock()
            .expect("lock")
            .insert(order_id.to_string(), order.clone());

        Ok(ModifyOrderResult {
            order,
            previous_order_id: None,
        })
    }

    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        self.cancelled_orders
            .lock()
            .expect("lock")
            .push(order_id.to_string());

        if let Some(response) = self
            .cancel_order_responses
            .lock()
            .expect("lock")
            .remove(order_id)
        {
            return response;
        }

        // Default: return a cancelled version of the stored order, or a synthetic one.
        let order = self
            .orders
            .lock()
            .expect("lock")
            .get(order_id)
            .cloned()
            .map_or_else(
                || {
                    let mut o = crate::models::test_order();
                    o.id = order_id.to_string();
                    o.status = OrderStatus::Cancelled;
                    o
                },
                |mut o| {
                    o.status = OrderStatus::Cancelled;
                    o
                },
            );

        Ok(CancelOrderResult {
            success: true,
            order,
        })
    }

    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        let trades = self.trades.lock().expect("lock");
        let mut result = trades.clone();

        if let Some(ref symbol) = params.symbol {
            result.retain(|t| &t.symbol == symbol);
        }
        if let Some(ref order_id) = params.order_id {
            result.retain(|t| &t.order_id == order_id);
        }

        Ok(result)
    }

    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
        if let Some(err) = self.get_positions_error.lock().expect("lock").take() {
            return Err(err);
        }

        let positions = self.positions.lock().expect("lock");
        let mut result = positions.clone();

        if let Some(sym) = symbol {
            result.retain(|p| p.symbol == sym);
        }

        Ok(result)
    }

    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        self.positions
            .lock()
            .expect("lock")
            .iter()
            .find(|p| p.id == position_id)
            .cloned()
            .ok_or_else(|| GatewayError::PositionNotFound {
                id: position_id.to_string(),
            })
    }

    async fn close_position(
        &self,
        position_id: &str,
        _request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        if let Some(response) = self
            .close_position_responses
            .lock()
            .expect("lock")
            .remove(position_id)
        {
            return response;
        }

        Ok(test_order_handle())
    }

    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        self.quotes
            .lock()
            .expect("lock")
            .get(symbol)
            .cloned()
            .ok_or_else(|| GatewayError::SymbolNotFound {
                symbol: symbol.to_string(),
            })
    }

    async fn get_bars(&self, symbol: &str, _params: &BarParams) -> GatewayResult<Vec<Bar>> {
        Ok(self
            .bars
            .lock()
            .expect("lock")
            .get(symbol)
            .cloned()
            .unwrap_or_default())
    }

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Ok(ProviderCapabilities::capabilities(self))
    }

    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        Ok(ConnectionStatus {
            connected: true,
            latency_ms: 0,
            last_heartbeat: chrono::Utc::now(),
        })
    }
}
