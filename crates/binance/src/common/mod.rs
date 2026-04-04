//! Common utilities shared across all Binance product adapters.
//!
//! This module contains shared functionality for:
//! - Binance Spot
//! - Binance Futures USDS-M
//! - Binance COIN-M Futures
//! - Binance Margin (Cross/Isolated)
//!
//! Each Binance product uses different API endpoints but shares common patterns:
//! - HMAC-SHA256 request signing
//! - Similar error codes and responses
//! - Timestamp and recvWindow handling

pub mod auth;
pub mod error;
pub mod http;
pub mod types;

#[allow(unused_imports)]
pub use auth::{DEFAULT_RECV_WINDOW, current_timestamp_ms, sign_query};
pub use error::binance_error_mapper;
pub use http::BinanceHttpClient;
#[allow(unused_imports)]
pub use types::*;
