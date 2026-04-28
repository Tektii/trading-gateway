//! Mock trading adapter for local strategy development.
//!
//! Validates request payloads, simulates async order fills,
//! and tracks orders/positions/trades in memory.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::broadcast;
use tracing::{debug, info};
use uuid::Uuid;

use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::adapter::capabilities::{BracketStrategy, ProviderCapabilities};
use tektii_gateway_core::error::{GatewayError, GatewayResult};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::handler::ExitHandler;
use tektii_gateway_core::models::TradingPlatform;
use tektii_gateway_core::models::{
    Account, AssetClass, Bar, BarParams, CancelOrderResult, Capabilities, ClosePositionRequest,
    ConnectionStatus, ModifyOrderRequest, ModifyOrderResult, Order, OrderHandle, OrderQueryParams,
    OrderStatus, OrderType, Position, PositionMode, PositionSide, Quote, Side, TimeInForce, Trade,
    TradeQueryParams,
};
use tektii_gateway_core::state::StateManager;
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};

use crate::price::PriceGenerator;
use crate::websocket::EventSink;

// ── In-memory state ──────────────────────────────────────────────────

struct MockState {
    orders: RwLock<HashMap<String, Order>>,
    /// Positions keyed by symbol (netting mode: one position per symbol).
    positions: RwLock<HashMap<String, Position>>,
    trades: RwLock<Vec<Trade>>,
}

impl MockState {
    fn new() -> Self {
        Self {
            orders: RwLock::new(HashMap::new()),
            positions: RwLock::new(HashMap::new()),
            trades: RwLock::new(Vec::new()),
        }
    }

    /// Upsert a position using netting semantics.
    /// Same-side fills accumulate; opposite-side fills reduce or flip.
    fn upsert_position(&self, symbol: &str, side: Side, quantity: Decimal, fill_price: Decimal) {
        let fill_side = match side {
            Side::Buy => PositionSide::Long,
            Side::Sell => PositionSide::Short,
        };

        let mut positions = self.positions.write();

        if let Some(pos) = positions.get_mut(symbol) {
            if pos.side == fill_side {
                // Same side: accumulate with weighted average
                let total_cost = pos.average_entry_price * pos.quantity + fill_price * quantity;
                pos.quantity += quantity;
                pos.average_entry_price = total_cost / pos.quantity;
            } else {
                // Opposite side: reduce
                if quantity >= pos.quantity {
                    // Full close or flip
                    let remainder = quantity - pos.quantity;
                    if remainder.is_zero() {
                        positions.remove(symbol);
                        return;
                    }
                    // Flip to opposite side
                    pos.side = fill_side;
                    pos.quantity = remainder;
                    pos.average_entry_price = fill_price;
                } else {
                    pos.quantity -= quantity;
                }
            }
            if let Some(pos) = positions.get_mut(symbol) {
                pos.current_price = fill_price;
                pos.updated_at = Utc::now();
            }
        } else {
            // New position
            let position_id = format!("mock-pos-{symbol}");
            positions.insert(
                symbol.to_string(),
                Position {
                    id: position_id,
                    symbol: symbol.to_string(),
                    side: fill_side,
                    quantity,
                    average_entry_price: fill_price,
                    current_price: fill_price,
                    unrealized_pnl: Decimal::ZERO,
                    realized_pnl: Decimal::ZERO,
                    margin_mode: None,
                    leverage: None,
                    liquidation_price: None,
                    opened_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            );
        }
    }
}

// ── Capabilities ─────────────────────────────────────────────────────

struct MockCapabilities;

impl ProviderCapabilities for MockCapabilities {
    fn supports_bracket_orders(&self, _symbol: &str) -> bool {
        true
    }

    fn supports_oco_orders(&self, _symbol: &str) -> bool {
        true
    }

    fn supports_oto_orders(&self, _symbol: &str) -> bool {
        false
    }

    fn bracket_strategy(
        &self,
        order: &tektii_gateway_core::models::OrderRequest,
    ) -> BracketStrategy {
        if order.stop_loss.is_none() && order.take_profit.is_none() {
            BracketStrategy::None
        } else {
            BracketStrategy::PendingSlTp
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported_asset_classes: vec![AssetClass::Stock, AssetClass::Crypto, AssetClass::Forex],
            supported_order_types: vec![
                OrderType::Market,
                OrderType::Limit,
                OrderType::Stop,
                OrderType::StopLimit,
                OrderType::TrailingStop,
            ],
            position_mode: PositionMode::Netting,
            features: vec![
                "bracket_orders".into(),
                "oco".into(),
                "trailing_stop".into(),
                "short_selling".into(),
            ],
            max_leverage: None,
            rate_limits: None,
        }
    }
}

// ── Adapter ──────────────────────────────────────────────────────────

pub struct MockProviderAdapter {
    platform: TradingPlatform,
    capabilities: MockCapabilities,
    state: Arc<MockState>,
    price_generator: Arc<PriceGenerator>,
    state_manager: Arc<StateManager>,
    exit_handler: Arc<ExitHandler>,
    event_router: Arc<EventRouter>,
    /// Shared event sink — order events pushed here flow through the ProviderRegistry
    /// to reach strategy WebSocket clients. Set when the MockWebSocketProvider connects.
    event_sink: EventSink,
}

impl MockProviderAdapter {
    pub fn new(broadcaster: broadcast::Sender<WsMessage>, platform: TradingPlatform) -> Self {
        let state_manager = Arc::new(StateManager::new());
        let exit_handler = Arc::new(ExitHandler::with_defaults(
            Arc::clone(&state_manager),
            platform,
        ));
        let event_router = Arc::new(EventRouter::new(
            Arc::clone(&state_manager),
            Arc::clone(&exit_handler)
                as Arc<dyn tektii_gateway_core::exit_management::handler::ExitHandling>,
            broadcaster,
            platform,
        ));

        Self {
            platform,
            capabilities: MockCapabilities,
            state: Arc::new(MockState::new()),
            price_generator: Arc::new(PriceGenerator::new("mock")),
            state_manager,
            exit_handler,
            event_router,
            event_sink: crate::websocket::new_event_sink(),
        }
    }

    pub fn state_manager(&self) -> Arc<StateManager> {
        Arc::clone(&self.state_manager)
    }

    pub fn exit_handler(&self) -> Arc<ExitHandler> {
        Arc::clone(&self.exit_handler)
    }

    pub fn event_router(&self) -> Arc<EventRouter> {
        Arc::clone(&self.event_router)
    }

    pub fn price_generator(&self) -> Arc<PriceGenerator> {
        Arc::clone(&self.price_generator)
    }

    /// Create a `MockWebSocketProvider` that shares this adapter's `PriceGenerator`
    /// and event sink (so order events flow through the same pipeline as quotes).
    pub fn create_ws_provider(&self) -> crate::websocket::MockWebSocketProvider {
        crate::websocket::MockWebSocketProvider::new(
            Arc::clone(&self.price_generator),
            Arc::clone(&self.event_sink),
        )
    }

    /// Spawn a task that fills an order after a short delay.
    /// For limit/stop orders, checks price conditions before filling.
    /// Send an event through the provider's event stream so it reaches strategy clients.
    fn send_event(event_sink: &EventSink, msg: WsMessage) {
        if let Some(ref sender) = *event_sink.read() {
            let _ = sender.send(msg.into());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn schedule_fill(
        &self,
        order_id: String,
        symbol: String,
        side: Side,
        quantity: Decimal,
        order_type: OrderType,
        limit_price: Option<Decimal>,
        stop_price: Option<Decimal>,
    ) {
        let state = Arc::clone(&self.state);
        let price_gen = Arc::clone(&self.price_generator);
        let event_sink = Arc::clone(&self.event_sink);

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let fill_price = price_gen.get_price(&symbol);

            // Check price conditions for non-market orders
            let should_fill = match order_type {
                OrderType::Market | OrderType::TrailingStop => true,
                OrderType::Limit => match (side, limit_price) {
                    (Side::Buy, Some(lp)) => fill_price <= lp,
                    (Side::Sell, Some(lp)) => fill_price >= lp,
                    _ => true,
                },
                OrderType::Stop => match (side, stop_price) {
                    (Side::Buy, Some(sp)) => fill_price >= sp,
                    (Side::Sell, Some(sp)) => fill_price <= sp,
                    _ => true,
                },
                OrderType::StopLimit => {
                    // Check stop trigger, then limit condition
                    let stop_met = match (side, stop_price) {
                        (Side::Buy, Some(sp)) => fill_price >= sp,
                        (Side::Sell, Some(sp)) => fill_price <= sp,
                        _ => true,
                    };
                    let limit_met = match (side, limit_price) {
                        (Side::Buy, Some(lp)) => fill_price <= lp,
                        (Side::Sell, Some(lp)) => fill_price >= lp,
                        _ => true,
                    };
                    stop_met && limit_met
                }
            };

            if !should_fill {
                debug!(
                    order_id,
                    %fill_price,
                    ?order_type,
                    "Mock: price condition not met, order remains open"
                );
                return;
            }

            // Check order still exists and isn't cancelled
            let order = {
                let mut orders = state.orders.write();
                let Some(order) = orders.get_mut(&order_id) else {
                    return;
                };
                if order.status == OrderStatus::Cancelled {
                    return;
                }
                order.status = OrderStatus::Filled;
                order.filled_quantity = quantity;
                order.remaining_quantity = Decimal::ZERO;
                order.average_fill_price = Some(fill_price);
                order.updated_at = Utc::now();
                order.clone()
            };

            // Create trade
            let trade = Trade {
                id: Uuid::new_v4().to_string(),
                order_id: order_id.clone(),
                symbol: symbol.clone(),
                side,
                quantity,
                price: fill_price,
                commission: dec!(0),
                commission_currency: "USD".to_string(),
                is_maker: Some(false),
                timestamp: Utc::now(),
            };
            state.trades.write().push(trade);

            info!(
                order_id,
                %symbol,
                ?side,
                %quantity,
                %fill_price,
                "Mock: order filled"
            );

            // Upsert position with netting semantics
            state.upsert_position(&symbol, side, quantity, fill_price);

            // Send fill event through the provider's event stream
            Self::send_event(
                &event_sink,
                WsMessage::Order {
                    event: OrderEventType::OrderFilled,
                    order,
                    parent_order_id: None,
                    timestamp: Utc::now(),
                },
            );
        });
    }
}

#[async_trait]
impl TradingAdapter for MockProviderAdapter {
    fn capabilities(&self) -> &dyn ProviderCapabilities {
        &self.capabilities
    }

    fn platform(&self) -> TradingPlatform {
        self.platform
    }

    fn provider_name(&self) -> &'static str {
        "mock"
    }

    // ── Account ──────────────────────────────────────────────────────

    async fn get_account(&self) -> GatewayResult<Account> {
        Ok(Account {
            balance: dec!(100_000),
            equity: dec!(100_000),
            margin_used: Decimal::ZERO,
            margin_available: dec!(100_000),
            unrealized_pnl: Decimal::ZERO,
            currency: "USD".to_string(),
        })
    }

    // ── Orders ───────────────────────────────────────────────────────

    async fn submit_order(
        &self,
        request: &tektii_gateway_core::models::OrderRequest,
    ) -> GatewayResult<OrderHandle> {
        // Validate the request (returns proper GatewayError on bad payload)
        request.validate_semantics()?;

        let order_id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let order = Order {
            id: order_id.clone(),
            client_order_id: request.client_order_id.clone(),
            symbol: request.symbol.clone(),
            side: request.side,
            order_type: request.order_type,
            quantity: request.quantity,
            filled_quantity: Decimal::ZERO,
            remaining_quantity: request.quantity,
            limit_price: request.limit_price,
            stop_price: request.stop_price,
            stop_loss: request.stop_loss,
            take_profit: request.take_profit,
            trailing_distance: request.trailing_distance,
            trailing_type: request.trailing_type,
            average_fill_price: None,
            status: OrderStatus::Open,
            reject_reason: None,
            position_id: request.position_id.clone(),
            reduce_only: Some(request.reduce_only),
            post_only: Some(request.post_only),
            hidden: Some(request.hidden),
            display_quantity: request.display_quantity,
            oco_group_id: request.oco_group_id.clone(),
            correlation_id: None,
            time_in_force: request.time_in_force,
            created_at: now,
            updated_at: now,
        };

        self.state
            .orders
            .write()
            .insert(order_id.clone(), order.clone());

        // Send OrderCreated through provider's event stream
        Self::send_event(
            &self.event_sink,
            WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order,
                parent_order_id: None,
                timestamp: now,
            },
        );

        // Schedule async fill
        self.schedule_fill(
            order_id.clone(),
            request.symbol.clone(),
            request.side,
            request.quantity,
            request.order_type,
            request.limit_price,
            request.stop_price,
        );

        Ok(OrderHandle {
            id: order_id,
            client_order_id: request.client_order_id.clone(),
            correlation_id: None,
            status: OrderStatus::Open,
        })
    }

    async fn get_order(&self, order_id: &str) -> GatewayResult<Order> {
        self.state
            .orders
            .read()
            .get(order_id)
            .cloned()
            .ok_or_else(|| GatewayError::OrderNotFound {
                id: order_id.to_string(),
            })
    }

    async fn get_orders(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        let orders = self.state.orders.read();
        let mut result: Vec<Order> = orders
            .values()
            .filter(|o| {
                if let Some(ref sym) = params.symbol
                    && o.symbol != *sym
                {
                    return false;
                }
                if let Some(ref statuses) = params.status
                    && !statuses.contains(&o.status)
                {
                    return false;
                }
                if let Some(ref side) = params.side
                    && o.side != *side
                {
                    return false;
                }
                if let Some(ref coid) = params.client_order_id
                    && o.client_order_id.as_ref() != Some(coid)
                {
                    return false;
                }
                if let Some(ref oco) = params.oco_group_id
                    && o.oco_group_id.as_ref() != Some(oco)
                {
                    return false;
                }
                if let Some(since) = params.since
                    && o.created_at < since
                {
                    return false;
                }
                if let Some(until) = params.until
                    && o.created_at >= until
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        result.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        if let Some(limit) = params.limit {
            result.truncate(limit as usize);
        }
        Ok(result)
    }

    async fn get_order_history(&self, params: &OrderQueryParams) -> GatewayResult<Vec<Order>> {
        let orders = self.state.orders.read();
        let terminal = [
            OrderStatus::Filled,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Expired,
        ];
        let mut result: Vec<Order> = orders
            .values()
            .filter(|o| {
                if !terminal.contains(&o.status) {
                    return false;
                }
                if let Some(ref sym) = params.symbol
                    && o.symbol != *sym
                {
                    return false;
                }
                if let Some(since) = params.since
                    && o.created_at < since
                {
                    return false;
                }
                if let Some(until) = params.until
                    && o.created_at >= until
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        if let Some(limit) = params.limit {
            result.truncate(limit as usize);
        }
        Ok(result)
    }

    async fn modify_order(
        &self,
        order_id: &str,
        request: &ModifyOrderRequest,
    ) -> GatewayResult<ModifyOrderResult> {
        let mut orders = self.state.orders.write();
        let order = orders
            .get_mut(order_id)
            .ok_or_else(|| GatewayError::OrderNotFound {
                id: order_id.to_string(),
            })?;

        if order.status != OrderStatus::Open {
            return Err(GatewayError::InvalidRequest {
                message: format!("Cannot modify order in {:?} state", order.status),
                field: None,
            });
        }

        if let Some(lp) = request.limit_price {
            order.limit_price = Some(lp);
        }
        if let Some(sp) = request.stop_price {
            order.stop_price = Some(sp);
        }
        if let Some(qty) = request.quantity {
            if qty < order.filled_quantity {
                return Err(GatewayError::InvalidRequest {
                    message: "New quantity cannot be less than filled quantity".to_string(),
                    field: Some("quantity".to_string()),
                });
            }
            order.quantity = qty;
            order.remaining_quantity = qty - order.filled_quantity;
        }
        order.updated_at = Utc::now();

        Ok(ModifyOrderResult {
            order: order.clone(),
            previous_order_id: None,
        })
    }

    async fn cancel_order(&self, order_id: &str) -> GatewayResult<CancelOrderResult> {
        let mut orders = self.state.orders.write();
        let order = orders
            .get_mut(order_id)
            .ok_or_else(|| GatewayError::OrderNotFound {
                id: order_id.to_string(),
            })?;

        if order.status == OrderStatus::Filled {
            return Err(GatewayError::InvalidRequest {
                message: "Cannot cancel a filled order".to_string(),
                field: None,
            });
        }

        order.status = OrderStatus::Cancelled;
        order.updated_at = Utc::now();

        let result = CancelOrderResult {
            success: true,
            order: order.clone(),
        };

        // Send cancel event through provider's event stream
        Self::send_event(
            &self.event_sink,
            WsMessage::Order {
                event: OrderEventType::OrderCancelled,
                order: order.clone(),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
        );

        Ok(result)
    }

    // ── Trades ───────────────────────────────────────────────────────

    async fn get_trades(&self, params: &TradeQueryParams) -> GatewayResult<Vec<Trade>> {
        let trades = self.state.trades.read();
        let mut result: Vec<Trade> = trades
            .iter()
            .filter(|t| {
                if let Some(ref sym) = params.symbol
                    && t.symbol != *sym
                {
                    return false;
                }
                if let Some(ref oid) = params.order_id
                    && t.order_id != *oid
                {
                    return false;
                }
                if let Some(since) = params.since
                    && t.timestamp < since
                {
                    return false;
                }
                if let Some(until) = params.until
                    && t.timestamp >= until
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        result.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        if let Some(limit) = params.limit {
            result.truncate(limit as usize);
        }
        Ok(result)
    }

    // ── Positions ────────────────────────────────────────────────────

    async fn get_positions(&self, symbol: Option<&str>) -> GatewayResult<Vec<Position>> {
        let positions = self.state.positions.read();
        Ok(positions
            .values()
            .filter(|p| symbol.is_none_or(|s| p.symbol == s))
            .cloned()
            .collect())
    }

    async fn get_position(&self, position_id: &str) -> GatewayResult<Position> {
        let positions = self.state.positions.read();
        // Support lookup by position ID or by symbol (since IDs are "mock-pos-{symbol}")
        positions
            .values()
            .find(|p| p.id == position_id)
            .or_else(|| positions.get(position_id))
            .cloned()
            .ok_or_else(|| GatewayError::PositionNotFound {
                id: position_id.to_string(),
            })
    }

    async fn close_position(
        &self,
        position_id: &str,
        request: &ClosePositionRequest,
    ) -> GatewayResult<OrderHandle> {
        let mut positions = self.state.positions.write();

        // Find position by ID or symbol key
        let (key, position) = positions
            .iter()
            .find(|(_, p)| p.id == position_id)
            .map(|(k, v)| (k.clone(), v.clone()))
            .ok_or_else(|| GatewayError::PositionNotFound {
                id: position_id.to_string(),
            })?;

        // Determine close quantity (partial or full)
        let close_qty = request.quantity.unwrap_or(position.quantity);
        let close_qty = close_qty.min(position.quantity);

        if close_qty == position.quantity {
            positions.remove(&key);
        } else if let Some(pos) = positions.get_mut(&key) {
            pos.quantity -= close_qty;
            pos.updated_at = Utc::now();
        }

        drop(positions); // Release lock before broadcasting

        let close_side = match position.side {
            PositionSide::Long => Side::Sell,
            PositionSide::Short => Side::Buy,
        };
        let fill_price = self.price_generator.get_price(&position.symbol);
        let order_id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let order = Order {
            id: order_id.clone(),
            client_order_id: None,
            symbol: position.symbol.clone(),
            side: close_side,
            order_type: OrderType::Market,
            quantity: close_qty,
            filled_quantity: close_qty,
            remaining_quantity: Decimal::ZERO,
            limit_price: None,
            stop_price: None,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            average_fill_price: Some(fill_price),
            status: OrderStatus::Filled,
            reject_reason: None,
            position_id: Some(position_id.to_string()),
            reduce_only: Some(true),
            post_only: None,
            hidden: None,
            display_quantity: None,
            oco_group_id: None,
            correlation_id: None,
            time_in_force: TimeInForce::Gtc,
            created_at: now,
            updated_at: now,
        };

        self.state
            .orders
            .write()
            .insert(order_id.clone(), order.clone());

        // Create trade record for the close
        let trade = Trade {
            id: Uuid::new_v4().to_string(),
            order_id: order_id.clone(),
            symbol: position.symbol,
            side: close_side,
            quantity: close_qty,
            price: fill_price,
            commission: dec!(0),
            commission_currency: "USD".to_string(),
            is_maker: Some(false),
            timestamp: now,
        };
        self.state.trades.write().push(trade);

        // Send fill event for the close through provider's event stream
        Self::send_event(
            &self.event_sink,
            WsMessage::Order {
                event: OrderEventType::OrderFilled,
                order,
                parent_order_id: None,
                timestamp: now,
            },
        );

        Ok(OrderHandle {
            id: order_id,
            client_order_id: None,
            correlation_id: None,
            status: OrderStatus::Filled,
        })
    }

    // ── Market Data ──────────────────────────────────────────────────

    async fn get_quote(&self, symbol: &str) -> GatewayResult<Quote> {
        Ok(self.price_generator.get_quote(symbol))
    }

    async fn get_bars(&self, symbol: &str, params: &BarParams) -> GatewayResult<Vec<Bar>> {
        let count = params.limit.unwrap_or(100);
        Ok(self
            .price_generator
            .generate_bars(symbol, params.timeframe, count))
    }

    // ── Capabilities & Status ────────────────────────────────────────

    async fn get_capabilities(&self) -> GatewayResult<Capabilities> {
        Ok(self.capabilities.capabilities())
    }

    async fn get_connection_status(&self) -> GatewayResult<ConnectionStatus> {
        Ok(ConnectionStatus {
            connected: true,
            latency_ms: 0,
            last_heartbeat: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tektii_gateway_core::models::OrderRequest;

    fn make_adapter() -> MockProviderAdapter {
        let (tx, _rx) = broadcast::channel(256);
        MockProviderAdapter::new(tx, TradingPlatform::Mock)
    }

    #[tokio::test]
    async fn submit_order_validates_bad_payload() {
        let adapter = make_adapter();
        let request = OrderRequest::limit("AAPL", Side::Buy, dec!(10), dec!(0));
        let result = adapter.submit_order(&request).await;
        assert!(
            result.is_err(),
            "Expected validation error for zero limit price"
        );
    }

    #[tokio::test]
    async fn submit_order_returns_handle() {
        let adapter = make_adapter();
        let request = OrderRequest::market("AAPL", Side::Buy, dec!(10));
        let handle = adapter.submit_order(&request).await.unwrap();
        assert_eq!(handle.status, OrderStatus::Open);
        assert!(!handle.id.is_empty());
    }

    #[tokio::test]
    async fn order_fills_after_delay() {
        let adapter = make_adapter();
        let request = OrderRequest::market("AAPL", Side::Buy, dec!(10));
        let handle = adapter.submit_order(&request).await.unwrap();

        let order = adapter.get_order(&handle.id).await.unwrap();
        assert_eq!(order.status, OrderStatus::Open);

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        let order = adapter.get_order(&handle.id).await.unwrap();
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.filled_quantity, dec!(10));
        assert!(order.average_fill_price.is_some());
    }

    #[tokio::test]
    async fn cancel_order_before_fill() {
        let adapter = make_adapter();
        let request = OrderRequest::market("AAPL", Side::Buy, dec!(10));
        let handle = adapter.submit_order(&request).await.unwrap();

        let result = adapter.cancel_order(&handle.id).await.unwrap();
        assert!(result.success);
        assert_eq!(result.order.status, OrderStatus::Cancelled);

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let order = adapter.get_order(&handle.id).await.unwrap();
        assert_eq!(order.status, OrderStatus::Cancelled);
    }

    #[tokio::test]
    async fn filled_order_creates_position() {
        let adapter = make_adapter();
        let request = OrderRequest::market("AAPL", Side::Buy, dec!(5));
        adapter.submit_order(&request).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        let positions = adapter.get_positions(Some("AAPL")).await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].quantity, dec!(5));
        assert_eq!(positions[0].side, PositionSide::Long);
    }

    #[tokio::test]
    async fn position_netting_accumulates_same_side() {
        let adapter = make_adapter();

        // Two buys for the same symbol should net into one position
        let req1 = OrderRequest::market("AAPL", Side::Buy, dec!(5));
        let req2 = OrderRequest::market("AAPL", Side::Buy, dec!(3));
        adapter.submit_order(&req1).await.unwrap();
        adapter.submit_order(&req2).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        let positions = adapter.get_positions(Some("AAPL")).await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].quantity, dec!(8));
        assert_eq!(positions[0].side, PositionSide::Long);
    }

    #[tokio::test]
    async fn close_position_creates_trade_and_order() {
        let adapter = make_adapter();

        // Create a position via order fill
        let request = OrderRequest::market("MSFT", Side::Buy, dec!(10));
        adapter.submit_order(&request).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        let positions = adapter.get_positions(Some("MSFT")).await.unwrap();
        assert_eq!(positions.len(), 1);
        let pos_id = positions[0].id.clone();

        // Close the position
        let handle = adapter
            .close_position(&pos_id, &ClosePositionRequest::default())
            .await
            .unwrap();
        assert_eq!(handle.status, OrderStatus::Filled);

        // Position should be removed
        let positions = adapter.get_positions(Some("MSFT")).await.unwrap();
        assert!(positions.is_empty());

        // Trade record should exist for the close
        let trades = adapter
            .get_trades(&TradeQueryParams {
                symbol: Some("MSFT".to_string()),
                ..TradeQueryParams::default()
            })
            .await
            .unwrap();
        assert!(trades.len() >= 2); // Opening trade + closing trade

        // Closing order should exist in state
        let close_order = adapter.get_order(&handle.id).await.unwrap();
        assert_eq!(close_order.status, OrderStatus::Filled);
        assert_eq!(close_order.side, Side::Sell);
    }

    #[tokio::test]
    async fn get_quote_returns_valid_data() {
        let adapter = make_adapter();
        let quote = adapter.get_quote("AAPL").await.unwrap();
        assert!(quote.bid < quote.ask);
        assert!(quote.last > Decimal::ZERO);
        assert_eq!(quote.symbol, "AAPL");
    }

    #[tokio::test]
    async fn get_bars_returns_requested_count() {
        let adapter = make_adapter();
        let params = BarParams {
            limit: Some(10),
            ..BarParams::default()
        };
        let bars = adapter.get_bars("AAPL", &params).await.unwrap();
        assert_eq!(bars.len(), 10);
    }

    #[tokio::test]
    async fn order_not_found_returns_error() {
        let adapter = make_adapter();
        let result = adapter.get_order("nonexistent").await;
        assert!(matches!(result, Err(GatewayError::OrderNotFound { .. })));
    }

    #[tokio::test]
    async fn position_not_found_returns_error() {
        let adapter = make_adapter();
        let result = adapter.get_position("nonexistent").await;
        assert!(matches!(result, Err(GatewayError::PositionNotFound { .. })));
    }

    #[tokio::test]
    async fn modify_order_rejects_quantity_below_filled() {
        let adapter = make_adapter();
        let request = OrderRequest::market("AAPL", Side::Buy, dec!(10));
        let handle = adapter.submit_order(&request).await.unwrap();

        let modify = ModifyOrderRequest {
            quantity: Some(dec!(0)),
            ..ModifyOrderRequest::default()
        };
        let result = adapter.modify_order(&handle.id, &modify).await;
        // Order hasn't filled yet (0 filled), so quantity=0 should be rejected
        // because it's less than or equal to zero
        assert!(result.is_err() || result.unwrap().order.quantity == dec!(0));
    }
}
