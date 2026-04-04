//! Mock provider for local strategy development.
//!
//! Provides a fully functional gateway adapter that requires no broker credentials.
//! Returns synthetic data, validates request payloads, simulates async order fills,
//! and streams price events over WebSocket — enough to verify strategy integration
//! before uploading for real backtesting or going live.

mod adapter;
mod price;
mod websocket;

pub use adapter::MockProviderAdapter;
pub use price::PriceGenerator;
pub use websocket::{MockWebSocketProvider, new_event_sink};
