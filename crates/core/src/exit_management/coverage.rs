//! Exit order coverage tracking.
//!
//! Provides types and functions for calculating position coverage based on
//! **active** exit orders only (Open, `PartiallyFilled`), not terminal orders
//! (Filled, Cancelled, Rejected, Expired).

use rust_decimal::Decimal;

use super::types::{ActualOrder, ExitLegType};

/// Tracks coverage for a single entry order's exit legs.
///
/// Both stop-loss and take-profit cover the **same** position (not additive).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitCoverage {
    /// Total filled quantity on the primary/entry order.
    pub filled_qty: Decimal,
    /// Quantity covered by ACTIVE stop-loss orders only.
    pub sl_covered: Decimal,
    /// Quantity covered by ACTIVE take-profit orders only.
    pub tp_covered: Decimal,
}

#[allow(dead_code)]
impl ExitCoverage {
    /// Creates a new coverage tracker with zero coverage.
    #[must_use]
    pub const fn new(filled_qty: Decimal) -> Self {
        Self {
            filled_qty,
            sl_covered: Decimal::ZERO,
            tp_covered: Decimal::ZERO,
        }
    }

    /// Returns quantity that still needs stop-loss protection.
    #[must_use]
    pub fn qty_needing_sl(&self) -> Decimal {
        (self.filled_qty - self.sl_covered).max(Decimal::ZERO)
    }

    /// Returns quantity that still needs take-profit protection.
    #[must_use]
    pub fn qty_needing_tp(&self) -> Decimal {
        (self.filled_qty - self.tp_covered).max(Decimal::ZERO)
    }

    /// Returns quantity needing coverage for a specific leg type.
    #[must_use]
    pub fn qty_needing(&self, leg_type: ExitLegType) -> Decimal {
        match leg_type {
            ExitLegType::StopLoss => self.qty_needing_sl(),
            ExitLegType::TakeProfit => self.qty_needing_tp(),
        }
    }

    /// Returns true if position is fully covered by both SL and TP.
    #[must_use]
    pub fn is_fully_covered(&self) -> bool {
        self.qty_needing_sl().is_zero() && self.qty_needing_tp().is_zero()
    }

    /// Returns true if position has any coverage gap.
    #[must_use]
    pub fn has_gap(&self) -> bool {
        !self.is_fully_covered()
    }
}

impl Default for ExitCoverage {
    fn default() -> Self {
        Self::new(Decimal::ZERO)
    }
}

/// Calculates active coverage from a list of actual orders.
///
/// Only counts ACTIVE orders (Open, `PartiallyFilled`) using their remaining quantity.
pub fn calculate_active_coverage(
    existing_orders: &[ActualOrder],
    leg_type: ExitLegType,
) -> Decimal {
    existing_orders
        .iter()
        .filter(|o| o.is_active() && o.leg_type == leg_type)
        .map(super::types::ActualOrder::remaining_qty)
        .sum()
}

/// Builds a complete coverage snapshot from existing orders.
#[allow(dead_code)]
pub fn build_coverage(filled_qty: Decimal, existing_orders: &[ActualOrder]) -> ExitCoverage {
    ExitCoverage {
        filled_qty,
        sl_covered: calculate_active_coverage(existing_orders, ExitLegType::StopLoss),
        tp_covered: calculate_active_coverage(existing_orders, ExitLegType::TakeProfit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit_management::types::ActualOrderStatus;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn make_order(qty: Decimal, status: ActualOrderStatus, leg_type: ExitLegType) -> ActualOrder {
        ActualOrder {
            order_id: "test-order".to_string(),
            quantity: qty,
            placed_at: Utc::now(),
            status,
            leg_type,
        }
    }

    #[test]
    fn test_coverage_qty_needing() {
        let coverage = ExitCoverage {
            filled_qty: dec!(1.0),
            sl_covered: dec!(0.5),
            tp_covered: dec!(0.8),
        };

        assert_eq!(coverage.qty_needing_sl(), dec!(0.5));
        assert_eq!(coverage.qty_needing_tp(), dec!(0.2));
    }

    #[test]
    fn test_coverage_fully_covered() {
        let coverage = ExitCoverage {
            filled_qty: dec!(1.0),
            sl_covered: dec!(1.0),
            tp_covered: dec!(1.0),
        };

        assert!(coverage.is_fully_covered());
        assert!(!coverage.has_gap());
    }

    #[test]
    fn test_coverage_not_fully_covered_when_sl_missing() {
        let coverage = ExitCoverage {
            filled_qty: dec!(1.0),
            sl_covered: dec!(0.5),
            tp_covered: dec!(1.0),
        };

        assert!(!coverage.is_fully_covered());
        assert!(coverage.has_gap());
    }

    #[test]
    fn test_coverage_over_covered_returns_zero_needed() {
        let coverage = ExitCoverage {
            filled_qty: dec!(0.5),
            sl_covered: dec!(1.0),
            tp_covered: dec!(1.0),
        };

        assert!(coverage.qty_needing_sl().is_zero());
        assert!(coverage.qty_needing_tp().is_zero());
        assert!(coverage.is_fully_covered());
    }

    #[test]
    fn test_calculate_active_coverage_filters_terminal_orders() {
        let orders = vec![
            make_order(dec!(0.5), ActualOrderStatus::Open, ExitLegType::StopLoss),
            make_order(
                dec!(0.3),
                ActualOrderStatus::Filled {
                    filled_at: Utc::now(),
                },
                ExitLegType::StopLoss,
            ),
            make_order(
                dec!(0.2),
                ActualOrderStatus::Cancelled {
                    cancelled_at: Utc::now(),
                },
                ExitLegType::StopLoss,
            ),
        ];

        let coverage = calculate_active_coverage(&orders, ExitLegType::StopLoss);
        assert_eq!(coverage, dec!(0.5));
    }

    #[test]
    fn test_calculate_active_coverage_uses_remaining_qty() {
        let orders = vec![make_order(
            dec!(1.0),
            ActualOrderStatus::PartiallyFilled {
                filled_qty: dec!(0.6),
                updated_at: Utc::now(),
            },
            ExitLegType::StopLoss,
        )];

        let coverage = calculate_active_coverage(&orders, ExitLegType::StopLoss);
        assert_eq!(coverage, dec!(0.4));
    }

    #[test]
    fn test_calculate_active_coverage_filters_by_leg_type() {
        let orders = vec![
            make_order(dec!(0.5), ActualOrderStatus::Open, ExitLegType::StopLoss),
            make_order(dec!(0.7), ActualOrderStatus::Open, ExitLegType::TakeProfit),
        ];

        let sl_coverage = calculate_active_coverage(&orders, ExitLegType::StopLoss);
        let tp_coverage = calculate_active_coverage(&orders, ExitLegType::TakeProfit);

        assert_eq!(sl_coverage, dec!(0.5));
        assert_eq!(tp_coverage, dec!(0.7));
    }

    #[test]
    fn test_build_coverage_complete() {
        let orders = vec![
            make_order(dec!(0.5), ActualOrderStatus::Open, ExitLegType::StopLoss),
            make_order(dec!(0.3), ActualOrderStatus::Open, ExitLegType::TakeProfit),
        ];

        let coverage = build_coverage(dec!(1.0), &orders);

        assert_eq!(coverage.filled_qty, dec!(1.0));
        assert_eq!(coverage.sl_covered, dec!(0.5));
        assert_eq!(coverage.tp_covered, dec!(0.3));
        assert_eq!(coverage.qty_needing_sl(), dec!(0.5));
        assert_eq!(coverage.qty_needing_tp(), dec!(0.7));
    }
}
