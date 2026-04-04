pub mod adapter_executor;
pub mod executor;
pub mod handler;
pub mod price_source;
pub mod types;
pub mod ws_price_source;

pub use adapter_executor::AdapterStopOrderExecutor;
pub use executor::{
    ModifyStopRequest, PlacedStopOrder, StopOrderExecutor, StopOrderRequest, StopOrderStatus,
};
pub use handler::{RecoveryStats, TrailingStopHandler};
pub use price_source::{
    NoOpQuoteSubscriber, PriceSource, PriceUpdate, QuoteSubscriber, UnimplementedPriceSource,
};
pub use types::{TrailingEntry, TrailingEntryStatus, TrailingStopConfig};
pub use ws_price_source::WebSocketPriceSource;

#[cfg(test)]
pub use executor::mock::MockStopOrderExecutor;
#[cfg(test)]
pub use price_source::MockPriceSource;
