//! HTTP utilities — retry logic and request execution with error mapping.

pub mod client;
pub mod retry;

pub use client::{
    ErrorMapper, default_error_mapper, execute_with_retry, extract_message, parse_json,
};
pub use retry::{RetryConfig, retry_with_backoff};
