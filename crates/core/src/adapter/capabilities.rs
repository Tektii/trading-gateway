//! Provider capability types.

use crate::models::{Capabilities, OrderRequest};

/// Strategy for handling bracket orders (SL/TP).
///
/// Different providers have varying levels of native bracket order support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BracketStrategy {
    /// No bracket order support
    None,
    /// Provider supports native bracket orders (e.g., Alpaca stocks)
    NativeBracket,
    /// Provider supports native OCO (e.g., Binance)
    NativeOco,
    /// Use pending SL/TP system (universal fallback)
    PendingSlTp,
}

impl BracketStrategy {
    /// Check if this strategy uses native provider support.
    #[must_use]
    pub const fn is_native(&self) -> bool {
        matches!(self, Self::NativeBracket | Self::NativeOco)
    }

    /// Check if this strategy requires client-side management.
    #[must_use]
    pub const fn requires_client_management(&self) -> bool {
        matches!(self, Self::None | Self::PendingSlTp)
    }
}

/// Provider capability queries.
///
/// Synchronous queries about a provider's static capabilities.
pub trait ProviderCapabilities: Send + Sync {
    /// Check if provider supports native bracket orders for symbol.
    fn supports_bracket_orders(&self, symbol: &str) -> bool;

    /// Check if provider supports native OCO for symbol.
    fn supports_oco_orders(&self, symbol: &str) -> bool;

    /// Check if provider supports OTO (one-triggers-other).
    fn supports_oto_orders(&self, symbol: &str) -> bool;

    /// Determine bracket strategy for an order.
    fn bracket_strategy(&self, order: &OrderRequest) -> BracketStrategy;

    /// Get full capabilities object.
    fn capabilities(&self) -> Capabilities;
}
