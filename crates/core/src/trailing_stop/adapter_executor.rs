//! Generic stop order executor backed by [`TradingAdapter`].
//!
//! Delegates stop order operations to the provider's existing order API,
//! allowing a single implementation to work across all providers.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use tracing::{debug, warn};

use crate::adapter::TradingAdapter;
use crate::error::GatewayError;
use crate::models::{OrderRequest, OrderStatus, OrderType, TimeInForce, TradingPlatform};

use super::executor::{
    ModifyStopRequest, PlacedStopOrder, StopOrderExecutor, StopOrderRequest, StopOrderStatus,
};

/// Stop order executor that delegates to a [`TradingAdapter`].
///
/// Places STOP (market) orders through the adapter's `submit_order` method.
/// Uses cancel+place for modifications (not atomic — handler has reconciliation
/// logic for the brief unprotected window).
pub struct AdapterStopOrderExecutor {
    adapter: Arc<dyn TradingAdapter>,
    platform: TradingPlatform,
}

impl AdapterStopOrderExecutor {
    /// Creates a new executor backed by the given trading adapter.
    #[must_use]
    pub fn new(adapter: Arc<dyn TradingAdapter>, platform: TradingPlatform) -> Self {
        Self { adapter, platform }
    }

    /// Build an `OrderRequest` for a stop market order.
    fn build_stop_order(request: &StopOrderRequest) -> OrderRequest {
        OrderRequest {
            symbol: request.symbol.clone(),
            side: request.side,
            quantity: request.quantity,
            order_type: OrderType::Stop,
            stop_price: Some(request.stop_price),
            limit_price: None,
            trailing_distance: None,
            trailing_type: None,
            stop_loss: None,
            take_profit: None,
            position_id: None,
            reduce_only: true, // Trailing stops are protective — always reduce-only
            post_only: false,
            hidden: false,
            display_quantity: None,
            margin_mode: None,
            leverage: None,
            client_order_id: None,
            oco_group_id: None,
            time_in_force: TimeInForce::default(),
        }
    }
}

#[async_trait]
impl StopOrderExecutor for AdapterStopOrderExecutor {
    async fn place_stop_order(
        &self,
        request: &StopOrderRequest,
    ) -> Result<PlacedStopOrder, GatewayError> {
        let order_request = Self::build_stop_order(request);

        debug!(
            symbol = %request.symbol,
            side = ?request.side,
            stop_price = %request.stop_price,
            quantity = %request.quantity,
            platform = ?self.platform,
            "Placing trailing stop order"
        );

        let handle = self.adapter.submit_order(&order_request).await?;

        Ok(PlacedStopOrder {
            order_id: handle.id,
            stop_price: request.stop_price,
            placed_at: Utc::now(),
        })
    }

    async fn modify_stop_order(
        &self,
        request: &ModifyStopRequest,
    ) -> Result<PlacedStopOrder, GatewayError> {
        debug!(
            order_id = %request.order_id,
            symbol = %request.symbol,
            new_stop_price = %request.new_stop_price,
            platform = ?self.platform,
            "Modifying trailing stop order (cancel+place)"
        );

        // Step 1: Cancel the existing stop order
        let cancel_result = self.adapter.cancel_order(&request.order_id).await?;

        // Check if the stop was already filled (triggered) before we could modify it
        if cancel_result.order.status == OrderStatus::Filled {
            return Err(GatewayError::Internal {
                message: format!(
                    "Stop order {} already filled — trailing stop triggered before modification",
                    request.order_id
                ),
                source: None,
            });
        }

        // Step 2: Place the new stop order
        let new_request = StopOrderRequest {
            symbol: request.symbol.clone(),
            side: request.side,
            quantity: request.quantity,
            stop_price: request.new_stop_price,
            limit_price: request.new_limit_price,
        };
        let order_request = Self::build_stop_order(&new_request);

        match self.adapter.submit_order(&order_request).await {
            Ok(handle) => Ok(PlacedStopOrder {
                order_id: handle.id,
                stop_price: request.new_stop_price,
                placed_at: Utc::now(),
            }),
            Err(place_err) => {
                // Cancel succeeded but place failed — position is UNPROTECTED
                warn!(
                    cancelled_order_id = %request.order_id,
                    symbol = %request.symbol,
                    error = %place_err,
                    platform = ?self.platform,
                    "Stop replacement failed after cancel — position unprotected!"
                );
                Err(place_err)
            }
        }
    }

    async fn cancel_stop_order(&self, order_id: &str, _symbol: &str) -> Result<(), GatewayError> {
        debug!(
            order_id,
            platform = ?self.platform,
            "Cancelling trailing stop order"
        );

        let result = self.adapter.cancel_order(order_id).await?;

        if !result.success {
            warn!(
                order_id,
                status = ?result.order.status,
                "Stop order cancel returned success=false"
            );
        }

        Ok(())
    }

    async fn get_stop_order_status(
        &self,
        order_id: &str,
        _symbol: &str,
    ) -> Result<StopOrderStatus, GatewayError> {
        let order = self.adapter.get_order(order_id).await?;

        let is_active = matches!(
            order.status,
            OrderStatus::Pending | OrderStatus::Open | OrderStatus::PartiallyFilled
        );

        Ok(StopOrderStatus {
            order_id: order.id,
            stop_price: order.stop_price.unwrap_or(Decimal::ZERO),
            is_active,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Side;
    use rust_decimal_macros::dec;

    // Unit tests verify the OrderRequest building logic.
    // Integration tests with mock adapters are in the handler tests.

    #[test]
    fn build_stop_order_uses_stop_market_type() {
        let request = StopOrderRequest {
            symbol: "BTCUSDT".to_string(),
            side: Side::Sell,
            quantity: dec!(1.5),
            stop_price: dec!(47500),
            limit_price: dec!(47400), // Should be ignored for Stop market
        };

        let order = AdapterStopOrderExecutor::build_stop_order(&request);

        assert_eq!(order.order_type, OrderType::Stop);
        assert_eq!(order.stop_price, Some(dec!(47500)));
        assert!(
            order.limit_price.is_none(),
            "Stop market should not set limit_price"
        );
        assert!(order.reduce_only, "Trailing stops should be reduce-only");
        assert_eq!(order.symbol, "BTCUSDT");
        assert_eq!(order.side, Side::Sell);
        assert_eq!(order.quantity, dec!(1.5));
    }

    #[test]
    fn build_stop_order_buy_side() {
        let request = StopOrderRequest {
            symbol: "ETHUSDT".to_string(),
            side: Side::Buy,
            quantity: dec!(10),
            stop_price: dec!(3200),
            limit_price: dec!(3210),
        };

        let order = AdapterStopOrderExecutor::build_stop_order(&request);

        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.stop_price, Some(dec!(3200)));
    }
}
