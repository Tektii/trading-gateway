//! Binance authentication utilities.
//!
//! Provides HMAC-SHA256 signing for authenticated Binance API requests.
//! Used by all Binance product adapters (Spot, Futures, Margin, etc.).

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Sign a query string using HMAC-SHA256.
///
/// Binance requires all authenticated endpoints to include a `signature` parameter
/// computed as HMAC-SHA256 of the query string using the API secret.
pub fn sign_query(api_secret: &str, query: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(api_secret.as_bytes()).expect("HMAC can take any size");
    mac.update(query.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Get current timestamp in milliseconds for Binance API requests.
///
/// Binance requires a `timestamp` parameter on all authenticated requests,
/// representing milliseconds since Unix epoch.
///
/// # Panics
///
/// Panics if system time is before Unix epoch (should never happen).
pub fn current_timestamp_ms() -> i64 {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// Default recvWindow parameter value (5000ms).
///
/// `recvWindow` specifies the number of milliseconds after `timestamp` that
/// a request remains valid. Binance recommends 5000ms (5 seconds).
pub const DEFAULT_RECV_WINDOW: i64 = 5000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_query_produces_valid_hex() {
        let signature = sign_query("test_secret", "symbol=BTCUSDT&timestamp=1234567890000");

        // Should be 64-char hex string (256 bits = 32 bytes = 64 hex chars)
        assert_eq!(signature.len(), 64);
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sign_query_is_deterministic() {
        let sig1 = sign_query("secret", "query=test");
        let sig2 = sign_query("secret", "query=test");
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn sign_query_differs_with_different_secrets() {
        let sig1 = sign_query("secret1", "query=test");
        let sig2 = sign_query("secret2", "query=test");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn sign_query_differs_with_different_queries() {
        let sig1 = sign_query("secret", "query=test1");
        let sig2 = sign_query("secret", "query=test2");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn current_timestamp_ms_returns_reasonable_value() {
        let ts = current_timestamp_ms();
        assert!(ts > 1_577_836_800_000);
        assert!(ts < 4_102_444_800_000);
    }
}
