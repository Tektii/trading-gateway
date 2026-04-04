//! Binance COIN-M Futures-specific types.
//!
//! Response structures are identical to USDS-M Futures; re-export with aliases.

pub use super::super::futures::types::{
    BinanceFuturesAccount as BinanceCoinFuturesAccount,
    BinanceFuturesKline as BinanceCoinFuturesKline, BinanceFuturesOrder as BinanceCoinFuturesOrder,
    BinanceFuturesPosition as BinanceCoinFuturesPosition,
    BinanceFuturesQuote as BinanceCoinFuturesQuote, BinanceFuturesTrade as BinanceCoinFuturesTrade,
};

pub const BINANCE_COIN_FUTURES_BASE_URL: &str = "https://dapi.binance.com";
pub const BINANCE_COIN_FUTURES_TESTNET_URL: &str = "https://testnet.binancefuture.com";
