//! Moving a resting exit leg to a new trigger price.
//!
//! Prefers the provider's native modify, which is atomic at the broker. Where
//! that is unsupported the leg is cancelled and re-placed, and a rejected
//! replacement is followed by restoring the original exit -- so a failed move
//! leaves the position exactly as protected as it was before the call.

use rust_decimal::Decimal;
use smallvec::SmallVec;
use tracing::{error, info, warn};

use super::handler::ExitHandler;
use super::types::{ActualOrder, ActualOrderStatus, ExitEntry, ExitEntryStatus, ExitLegType};
use crate::adapter::TradingAdapter;
use crate::error::GatewayError;
use crate::models::{ExitLegPlacement, ModifyOrderRequest};

/// Why a leg could not be moved, and whether the position still has cover.
#[derive(Debug)]
pub enum ReplaceExitLegError {
    /// The move failed with the original exit still live at the provider.
    StillProtected(GatewayError),
    /// The original was cancelled and neither the replacement nor the restore
    /// was accepted -- the position has no exit order covering this leg.
    Unprotected {
        /// The exit order that was cancelled and could not be re-established.
        cancelled_order_id: String,
        /// Why the replacement was rejected.
        source: GatewayError,
        /// Why restoring the original failed.
        restore_error: GatewayError,
    },
}

impl ExitHandler {
    /// The live exit leg of `leg_type` protecting `position_id`, if tracked.
    ///
    /// A leg only becomes resolvable by position once its entry order has
    /// filled and the provider reported a position id.
    #[must_use]
    pub fn live_exit_leg(&self, position_id: &str, leg_type: ExitLegType) -> Option<ExitEntry> {
        self.pending_by_placeholder.iter().find_map(|entry| {
            let entry = entry.value();
            let matches = entry.position_id.as_deref() == Some(position_id)
                && entry.order_type == leg_type
                && matches!(
                    entry.status,
                    ExitEntryStatus::Placed { .. } | ExitEntryStatus::PartiallyTriggered { .. }
                )
                && entry
                    .status
                    .actual_orders()
                    .iter()
                    .any(ActualOrder::is_active);
            matches.then(|| entry.clone())
        })
    }

    /// Holds the fill-processing lock for the whole read-modify-write span:
    /// fill routing cancels and removes these same orders on an OCO fill, and
    /// interleaving the two would leave a replacement live but untracked.
    pub async fn replace_exit_leg(
        &self,
        placeholder_id: &str,
        new_trigger_price: Decimal,
        adapter: &dyn TradingAdapter,
    ) -> Result<ExitLegPlacement, ReplaceExitLegError> {
        let _fill_guard = self.fill_processing_lock.lock().await;

        let entry = self.get_entry(placeholder_id).ok_or_else(|| {
            ReplaceExitLegError::StillProtected(GatewayError::OrderNotFound {
                id: placeholder_id.to_string(),
            })
        })?;

        // A leg spans one order per partial fill; leaving any of them at the old
        // trigger would report a move that only partly happened.
        let resting: Vec<ActualOrder> = entry
            .status
            .actual_orders()
            .iter()
            .filter(|order| order.is_active())
            .cloned()
            .collect();

        if resting.is_empty() {
            return Err(ReplaceExitLegError::StillProtected(
                GatewayError::OrderNotModifiable {
                    order_id: placeholder_id.to_string(),
                    reason: format!("{} leg has no resting order to move", entry.order_type),
                },
            ));
        }

        let mut order_ids = Vec::with_capacity(resting.len());
        for order in &resting {
            order_ids.push(
                self.move_resting_order(placeholder_id, &entry, order, new_trigger_price, adapter)
                    .await?,
            );
        }

        Ok(ExitLegPlacement {
            order_ids,
            trigger_price: new_trigger_price,
        })
    }

    /// Move one resting order, returning the order id now holding it.
    async fn move_resting_order(
        &self,
        placeholder_id: &str,
        entry: &ExitEntry,
        resting: &ActualOrder,
        new_trigger_price: Decimal,
        adapter: &dyn TradingAdapter,
    ) -> Result<String, ReplaceExitLegError> {
        match adapter
            .modify_order(
                &resting.order_id,
                &modify_request(entry.order_type, new_trigger_price),
            )
            .await
        {
            Ok(result) => {
                let order_id = result.order.id;
                info!(
                    placeholder_id,
                    order_id = %order_id,
                    new_trigger_price = %new_trigger_price,
                    "Moved exit leg via provider modify"
                );
                self.record_moved_leg(placeholder_id, resting, order_id.clone(), new_trigger_price);
                Ok(order_id)
            }
            Err(GatewayError::UnsupportedOperation { .. }) => {
                self.cancel_and_replace(placeholder_id, entry, resting, new_trigger_price, adapter)
                    .await
            }
            Err(e) => Err(ReplaceExitLegError::StillProtected(e)),
        }
    }

    /// Move a resting order on a provider that cannot modify in place.
    ///
    /// The exit is off the book between the cancel and the replacement being
    /// accepted; a rejected replacement is followed by re-placing the original,
    /// and only a failed restore leaves the position uncovered.
    async fn cancel_and_replace(
        &self,
        placeholder_id: &str,
        entry: &ExitEntry,
        resting: &ActualOrder,
        new_trigger_price: Decimal,
        adapter: &dyn TradingAdapter,
    ) -> Result<String, ReplaceExitLegError> {
        adapter
            .cancel_order(&resting.order_id)
            .await
            .map_err(ReplaceExitLegError::StillProtected)?;

        // Only the unfilled remainder still needs covering -- re-placing the
        // order's original size would over-hedge a partially filled exit.
        let quantity = resting.remaining_qty();

        let mut moved = entry.clone();
        moved.trigger_price = new_trigger_price;
        let replacement = Self::build_exit_order_request(
            &moved,
            quantity,
            self.placed_count(placeholder_id),
            entry.position_id.as_deref(),
        );

        let replace_error = match adapter.submit_order(&replacement).await {
            Ok(handle) => {
                info!(
                    placeholder_id,
                    order_id = %handle.id,
                    previous_order_id = %resting.order_id,
                    new_trigger_price = %new_trigger_price,
                    "Moved exit leg via cancel-and-replace"
                );
                self.record_moved_leg(
                    placeholder_id,
                    resting,
                    handle.id.clone(),
                    new_trigger_price,
                );
                return Ok(handle.id);
            }
            Err(e) => e,
        };

        warn!(
            placeholder_id,
            error = %replace_error,
            "Exit leg replacement rejected — restoring the original exit"
        );

        let restore = Self::build_exit_order_request(
            entry,
            quantity,
            self.placed_count(placeholder_id),
            entry.position_id.as_deref(),
        );

        match adapter.submit_order(&restore).await {
            Ok(handle) => {
                info!(
                    placeholder_id,
                    order_id = %handle.id,
                    "Original exit restored after a rejected replacement"
                );
                self.record_moved_leg(placeholder_id, resting, handle.id, entry.trigger_price);
                Err(ReplaceExitLegError::StillProtected(
                    GatewayError::OrderNotModifiable {
                        order_id: resting.order_id.clone(),
                        reason: format!(
                            "replacement rejected ({replace_error}); the original exit was restored"
                        ),
                    },
                ))
            }
            Err(restore_error) => {
                error!(
                    placeholder_id,
                    order_id = %resting.order_id,
                    error = %replace_error,
                    restore_error = %restore_error,
                    "Position unprotected — exit leg cancelled and could not be re-established"
                );
                self.update_entry_to_failed(placeholder_id, &restore_error.to_string(), 0);
                Err(ReplaceExitLegError::Unprotected {
                    cancelled_order_id: resting.order_id.clone(),
                    source: replace_error,
                    restore_error,
                })
            }
        }
    }

    /// Orders placed for this entry so far, which seeds the replacement's
    /// `client_order_id`. Read live so successive moves never reuse an id.
    fn placed_count(&self, placeholder_id: &str) -> usize {
        self.pending_by_placeholder
            .get(placeholder_id)
            .map_or(0, |entry| entry.value().status.actual_orders().len())
    }

    /// Point the entry at the order now holding the leg, at its new trigger.
    ///
    /// Without this a later cancel-on-close would chase the replaced order id.
    fn record_moved_leg(
        &self,
        placeholder_id: &str,
        previous: &ActualOrder,
        new_order_id: String,
        trigger_price: Decimal,
    ) {
        let Some(mut entry) = self.pending_by_placeholder.get_mut(placeholder_id) else {
            warn!(
                placeholder_id,
                order_id = %new_order_id,
                "Exit entry vanished mid-move — the replacement order is live but untracked"
            );
            return;
        };
        let entry = entry.value_mut();

        let mut orders: SmallVec<[ActualOrder; 4]> =
            entry.status.actual_orders().iter().cloned().collect();
        let replacement = ActualOrder {
            order_id: new_order_id,
            quantity: previous.remaining_qty(),
            placed_at: chrono::Utc::now(),
            status: ActualOrderStatus::Open,
            leg_type: previous.leg_type,
        };
        if let Some(slot) = orders
            .iter_mut()
            .find(|order| order.order_id == previous.order_id)
        {
            *slot = replacement;
        } else {
            warn!(
                placeholder_id,
                previous_order_id = %previous.order_id,
                "Replaced order was no longer tracked — recording the replacement anyway"
            );
            orders.push(replacement);
        }

        entry.trigger_price = trigger_price;
        entry.status = match &entry.status {
            ExitEntryStatus::PartiallyTriggered {
                filled_qty,
                total_qty,
                ..
            } => ExitEntryStatus::PartiallyTriggered {
                filled_qty: *filled_qty,
                total_qty: *total_qty,
                actual_orders: orders,
            },
            ExitEntryStatus::Failed {
                error,
                retry_count,
                failed_at,
                ..
            } => ExitEntryStatus::Failed {
                error: error.clone(),
                retry_count: *retry_count,
                failed_at: *failed_at,
                actual_orders: orders,
            },
            _ => ExitEntryStatus::Placed {
                actual_orders: orders,
            },
        };
        entry.increment_version();
    }
}

/// A stop-loss leg rests as a stop order and a take-profit as a limit order, so
/// each carries its trigger in a different field.
fn modify_request(leg_type: ExitLegType, trigger_price: Decimal) -> ModifyOrderRequest {
    match leg_type {
        ExitLegType::StopLoss => ModifyOrderRequest {
            stop_price: Some(trigger_price),
            ..Default::default()
        },
        ExitLegType::TakeProfit => ModifyOrderRequest {
            limit_price: Some(trigger_price),
            ..Default::default()
        },
    }
}
