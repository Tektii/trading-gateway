//! Shared test infrastructure for Tektii Gateway.
//!
//! This is the **single source of truth** for all test mocks, fixtures, and harnesses.
//! Add as a `[dev-dependency]` in any workspace crate that needs test infrastructure.
//!
//! # Available modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`mock_adapter`] | `MockTradingAdapter` — fake broker adapter for integration/e2e tests |
//! | [`mock_executor`] | `MockStopOrderExecutor` — records stop order lifecycle calls |
//! | [`mock_exit_handler`] | `MockExitHandler` — records exit handling calls for assertions |
//! | [`mock_price_source`] | `MockPriceSource` — fake price feed for trailing stop tests |
//! | [`mock_provider`] | `MockWebSocketProvider` — fake broker WS stream with event injection |
//! | [`mock_state`] | `test_gateway_state()` — minimal `GatewayState` with no-op adapter |
//! | [`models`] | Test fixture factories (`test_order()`, `test_position()`, etc.) |
//! | [`harness`] | `TestGateway` — spins up a full gateway (REST + WS) for e2e tests |
//! | [`websocket`] | `MockWsServer` — single-client mock WebSocket server |
//! | [`wiremock_helpers`] | Convenience functions for adapter wiremock tests |
//!
//! # What lives in core behind `test-utils` feature
//!
//! Only `ExitHandler::insert_entry()` remains in core behind the `test-utils` feature
//! gate, because it requires private field access. Everything else lives here.

pub mod harness;
pub mod mock_adapter;
pub mod mock_executor;
pub mod mock_exit_handler;
pub mod mock_price_source;
pub mod mock_provider;
pub mod mock_state;
pub mod models;
pub mod websocket;
pub mod wiremock_helpers;

// Re-export so adapter tests don't need a separate wiremock dependency.
pub use wiremock;
