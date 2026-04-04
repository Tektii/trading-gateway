//! Trading platform type definitions.
//!
//! Identifies trading platforms (Alpaca, Binance, etc.) and their environments.

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use utoipa::ToSchema;

/// Trading platform identifier.
///
/// Each variant represents a specific trading platform and environment combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TradingPlatform {
    /// Alpaca live trading (real money)
    AlpacaLive,
    /// Alpaca paper trading (simulated)
    AlpacaPaper,

    /// Binance spot live trading (real money)
    BinanceSpotLive,
    /// Binance spot testnet (simulated)
    BinanceSpotTestnet,

    /// Binance futures live trading (real money)
    BinanceFuturesLive,
    /// Binance futures testnet (simulated)
    BinanceFuturesTestnet,

    /// Binance margin live trading (real money)
    BinanceMarginLive,
    /// Binance margin testnet (simulated)
    BinanceMarginTestnet,

    /// Binance COIN-M futures live trading (real money)
    BinanceCoinFuturesLive,
    /// Binance COIN-M futures testnet (simulated)
    BinanceCoinFuturesTestnet,

    /// Oanda v20 practice account (simulated)
    OandaPractice,
    /// Oanda v20 live account (real money)
    OandaLive,

    /// Saxo Bank SIM environment (simulated)
    SaxoSim,
    /// Saxo Bank LIVE environment (real money)
    SaxoLive,

    /// Tektii engine (historical simulation)
    Tektii,

    /// Mock provider for local development (no broker credentials required)
    Mock,
}

impl TradingPlatform {
    /// Get the platform kind (groups related platforms).
    #[must_use]
    pub const fn kind(&self) -> TradingPlatformKind {
        match self {
            Self::AlpacaLive | Self::AlpacaPaper => TradingPlatformKind::Alpaca,
            Self::BinanceSpotLive | Self::BinanceSpotTestnet => TradingPlatformKind::BinanceSpot,
            Self::BinanceFuturesLive | Self::BinanceFuturesTestnet => {
                TradingPlatformKind::BinanceFutures
            }
            Self::BinanceMarginLive | Self::BinanceMarginTestnet => {
                TradingPlatformKind::BinanceMargin
            }
            Self::BinanceCoinFuturesLive | Self::BinanceCoinFuturesTestnet => {
                TradingPlatformKind::BinanceCoinFutures
            }
            Self::OandaPractice | Self::OandaLive => TradingPlatformKind::Oanda,
            Self::SaxoSim | Self::SaxoLive => TradingPlatformKind::Saxo,
            Self::Tektii => TradingPlatformKind::Tektii,
            Self::Mock => TradingPlatformKind::Mock,
        }
    }

    /// Check if this is a paper/testnet/simulated platform (not real money).
    #[must_use]
    pub const fn is_paper(&self) -> bool {
        matches!(
            self,
            Self::AlpacaPaper
                | Self::BinanceSpotTestnet
                | Self::BinanceFuturesTestnet
                | Self::BinanceMarginTestnet
                | Self::BinanceCoinFuturesTestnet
                | Self::OandaPractice
                | Self::SaxoSim
                | Self::Tektii
                | Self::Mock
        )
    }

    /// Check if this is a live platform (real money).
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(
            self,
            Self::AlpacaLive
                | Self::BinanceSpotLive
                | Self::BinanceFuturesLive
                | Self::BinanceMarginLive
                | Self::BinanceCoinFuturesLive
                | Self::OandaLive
                | Self::SaxoLive
        )
    }

    /// Get kebab-case string identifier for this platform (used in metrics labels).
    #[must_use]
    pub const fn header_value(&self) -> &'static str {
        match self {
            Self::AlpacaLive => "alpaca-live",
            Self::AlpacaPaper => "alpaca-paper",
            Self::BinanceSpotLive => "binance-spot-live",
            Self::BinanceSpotTestnet => "binance-spot-testnet",
            Self::BinanceFuturesLive => "binance-futures-live",
            Self::BinanceFuturesTestnet => "binance-futures-testnet",
            Self::BinanceMarginLive => "binance-margin-live",
            Self::BinanceMarginTestnet => "binance-margin-testnet",
            Self::BinanceCoinFuturesLive => "binance-coin-futures-live",
            Self::BinanceCoinFuturesTestnet => "binance-coin-futures-testnet",
            Self::OandaPractice => "oanda-practice",
            Self::OandaLive => "oanda-live",
            Self::SaxoSim => "saxo-sim",
            Self::SaxoLive => "saxo-live",
            Self::Tektii => "tektii",
            Self::Mock => "mock",
        }
    }

    /// Get a human-readable display name.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::AlpacaLive => "Alpaca (Live)",
            Self::AlpacaPaper => "Alpaca (Paper)",
            Self::BinanceSpotLive => "Binance Spot (Live)",
            Self::BinanceSpotTestnet => "Binance Spot (Testnet)",
            Self::BinanceFuturesLive => "Binance Futures (Live)",
            Self::BinanceFuturesTestnet => "Binance Futures (Testnet)",
            Self::BinanceMarginLive => "Binance Margin (Live)",
            Self::BinanceMarginTestnet => "Binance Margin (Testnet)",
            Self::BinanceCoinFuturesLive => "Binance COIN-M Futures (Live)",
            Self::BinanceCoinFuturesTestnet => "Binance COIN-M Futures (Testnet)",
            Self::OandaPractice => "Oanda (Practice)",
            Self::OandaLive => "Oanda (Live)",
            Self::SaxoSim => "Saxo Bank (SIM)",
            Self::SaxoLive => "Saxo Bank (Live)",
            Self::Tektii => "Tektii",
            Self::Mock => "Mock (Local Development)",
        }
    }
}

impl FromStr for TradingPlatform {
    type Err = TradingPlatformParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "alpaca-live" | "alpaca_live" => Ok(Self::AlpacaLive),
            "alpaca-paper" | "alpaca_paper" | "alpaca" => Ok(Self::AlpacaPaper),
            "binance-spot-live" | "binance_spot_live" => Ok(Self::BinanceSpotLive),
            "binance-spot-testnet" | "binance_spot_testnet" | "binance-spot" => {
                Ok(Self::BinanceSpotTestnet)
            }
            "binance-futures-live" | "binance_futures_live" => Ok(Self::BinanceFuturesLive),
            "binance-futures-testnet" | "binance_futures_testnet" | "binance-futures" => {
                Ok(Self::BinanceFuturesTestnet)
            }
            "binance-margin-live" | "binance_margin_live" => Ok(Self::BinanceMarginLive),
            "binance-margin-testnet" | "binance_margin_testnet" | "binance-margin" => {
                Ok(Self::BinanceMarginTestnet)
            }
            "binance-coin-futures-live" | "binance_coin_futures_live" => {
                Ok(Self::BinanceCoinFuturesLive)
            }
            "binance-coin-futures-testnet"
            | "binance_coin_futures_testnet"
            | "binance-coin-futures" => Ok(Self::BinanceCoinFuturesTestnet),
            "oanda-live" | "oanda_live" => Ok(Self::OandaLive),
            "oanda-practice" | "oanda_practice" | "oanda" => Ok(Self::OandaPractice),
            "saxo-sim" | "saxo_sim" | "saxo" => Ok(Self::SaxoSim),
            "saxo-live" | "saxo_live" => Ok(Self::SaxoLive),
            "tektii" => Ok(Self::Tektii),
            "mock" => Ok(Self::Mock),
            _ => Err(TradingPlatformParseError(s.to_string())),
        }
    }
}

impl std::fmt::Display for TradingPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.header_value())
    }
}

/// Error returned when parsing a [`TradingPlatform`] from a string fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradingPlatformParseError(pub String);

impl std::fmt::Display for TradingPlatformParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown trading platform '{}'. Valid platforms: alpaca-live, alpaca-paper, \
             binance-spot-live, binance-spot-testnet, binance-futures-live, \
             binance-futures-testnet, binance-margin-live, binance-margin-testnet, \
             binance-coin-futures-live, binance-coin-futures-testnet, mock, oanda-practice, \
             oanda-live, saxo-sim, saxo-live, tektii",
            self.0
        )
    }
}

impl std::error::Error for TradingPlatformParseError {}

/// Trading platform kind (groups platforms by implementation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TradingPlatformKind {
    /// Alpaca Markets
    Alpaca,
    /// Binance Spot
    BinanceSpot,
    /// Binance Futures (USD-M)
    BinanceFutures,
    /// Binance Margin (Cross/Isolated)
    BinanceMargin,
    /// Binance COIN-M Futures
    BinanceCoinFutures,
    /// Oanda v20 forex broker
    Oanda,
    /// Saxo Bank forex/CFD broker
    Saxo,
    /// Tektii simulation engine
    Tektii,
    /// Mock provider for local development
    Mock,
}

/// Valid values for `GATEWAY_PROVIDER`.
pub const VALID_PROVIDERS: &str = "alpaca, binance_spot, mock, oanda, saxo, tektii";

impl TradingPlatformKind {
    /// Get all platforms for this kind.
    #[must_use]
    pub const fn platforms(&self) -> &'static [TradingPlatform] {
        match self {
            Self::Alpaca => &[TradingPlatform::AlpacaLive, TradingPlatform::AlpacaPaper],
            Self::BinanceSpot => &[
                TradingPlatform::BinanceSpotLive,
                TradingPlatform::BinanceSpotTestnet,
            ],
            Self::BinanceFutures => &[
                TradingPlatform::BinanceFuturesLive,
                TradingPlatform::BinanceFuturesTestnet,
            ],
            Self::BinanceMargin => &[
                TradingPlatform::BinanceMarginLive,
                TradingPlatform::BinanceMarginTestnet,
            ],
            Self::BinanceCoinFutures => &[
                TradingPlatform::BinanceCoinFuturesLive,
                TradingPlatform::BinanceCoinFuturesTestnet,
            ],
            Self::Oanda => &[TradingPlatform::OandaPractice, TradingPlatform::OandaLive],
            Self::Saxo => &[TradingPlatform::SaxoSim, TradingPlatform::SaxoLive],
            Self::Tektii => &[TradingPlatform::Tektii],
            Self::Mock => &[TradingPlatform::Mock],
        }
    }

    /// Map `(provider, mode)` to a concrete [`TradingPlatform`].
    #[must_use]
    pub const fn to_platform(self, mode: GatewayMode) -> TradingPlatform {
        match (self, mode) {
            (Self::Alpaca, GatewayMode::Paper) => TradingPlatform::AlpacaPaper,
            (Self::Alpaca, GatewayMode::Live) => TradingPlatform::AlpacaLive,
            (Self::BinanceSpot, GatewayMode::Paper) => TradingPlatform::BinanceSpotTestnet,
            (Self::BinanceSpot, GatewayMode::Live) => TradingPlatform::BinanceSpotLive,
            (Self::BinanceFutures, GatewayMode::Paper) => TradingPlatform::BinanceFuturesTestnet,
            (Self::BinanceFutures, GatewayMode::Live) => TradingPlatform::BinanceFuturesLive,
            (Self::BinanceMargin, GatewayMode::Paper) => TradingPlatform::BinanceMarginTestnet,
            (Self::BinanceMargin, GatewayMode::Live) => TradingPlatform::BinanceMarginLive,
            (Self::BinanceCoinFutures, GatewayMode::Paper) => {
                TradingPlatform::BinanceCoinFuturesTestnet
            }
            (Self::BinanceCoinFutures, GatewayMode::Live) => {
                TradingPlatform::BinanceCoinFuturesLive
            }
            (Self::Oanda, GatewayMode::Paper) => TradingPlatform::OandaPractice,
            (Self::Oanda, GatewayMode::Live) => TradingPlatform::OandaLive,
            (Self::Saxo, GatewayMode::Paper) => TradingPlatform::SaxoSim,
            (Self::Saxo, GatewayMode::Live) => TradingPlatform::SaxoLive,
            (Self::Tektii, _) => TradingPlatform::Tektii,
            (Self::Mock, _) => TradingPlatform::Mock,
        }
    }

    /// Required environment variables for this provider.
    #[must_use]
    pub const fn required_env_vars(&self) -> &'static [&'static str] {
        match self {
            Self::Alpaca => &["ALPACA_API_KEY", "ALPACA_API_SECRET"],
            Self::BinanceSpot
            | Self::BinanceFutures
            | Self::BinanceMargin
            | Self::BinanceCoinFutures => &["BINANCE_API_KEY", "BINANCE_API_SECRET"],
            Self::Oanda => &["OANDA_API_KEY", "OANDA_ACCOUNT_ID"],
            Self::Saxo => &["SAXO_APP_KEY", "SAXO_APP_SECRET", "SAXO_ACCOUNT_KEY"],
            Self::Tektii => &["TEKTII_ENGINE_URL", "TEKTII_ENGINE_WS_URL"],
            Self::Mock => &[],
        }
    }
}

impl FromStr for TradingPlatformKind {
    type Err = TradingPlatformParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "alpaca" => Ok(Self::Alpaca),
            "binance_spot" => Ok(Self::BinanceSpot),
            // UK-restricted: Binance derivatives/margin unavailable to UK retail (FCA)
            // "binance_futures" => Ok(Self::BinanceFutures),
            // "binance_margin" => Ok(Self::BinanceMargin),
            // "binance_coin_futures" => Ok(Self::BinanceCoinFutures),
            "oanda" => Ok(Self::Oanda),
            "saxo" => Ok(Self::Saxo),
            "tektii" => Ok(Self::Tektii),
            "mock" => Ok(Self::Mock),
            _ => Err(TradingPlatformParseError(s.to_string())),
        }
    }
}

impl std::fmt::Display for TradingPlatformKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alpaca => write!(f, "alpaca"),
            Self::BinanceSpot => write!(f, "binance_spot"),
            Self::BinanceFutures => write!(f, "binance_futures"),
            Self::BinanceMargin => write!(f, "binance_margin"),
            Self::BinanceCoinFutures => write!(f, "binance_coin_futures"),
            Self::Oanda => write!(f, "oanda"),
            Self::Saxo => write!(f, "saxo"),
            Self::Tektii => write!(f, "tektii"),
            Self::Mock => write!(f, "mock"),
        }
    }
}

/// Gateway trading mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GatewayMode {
    /// Paper/testnet/simulated trading (default).
    #[default]
    Paper,
    /// Live trading with real funds.
    Live,
}

impl FromStr for GatewayMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "paper" => Ok(Self::Paper),
            "live" => Ok(Self::Live),
            _ => Err(format!(
                "invalid GATEWAY_MODE='{s}': valid values are 'paper', 'live'"
            )),
        }
    }
}

impl std::fmt::Display for GatewayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paper => write!(f, "paper"),
            Self::Live => write!(f, "live"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_kind_from_str_all_valid() {
        let cases = [
            ("alpaca", TradingPlatformKind::Alpaca),
            ("binance_spot", TradingPlatformKind::BinanceSpot),
            // UK-restricted: futures/margin/coin_futures disabled
            ("oanda", TradingPlatformKind::Oanda),
            ("saxo", TradingPlatformKind::Saxo),
            ("tektii", TradingPlatformKind::Tektii),
            ("mock", TradingPlatformKind::Mock),
        ];
        for (s, expected) in cases {
            assert_eq!(s.parse::<TradingPlatformKind>().unwrap(), expected);
        }
    }

    #[test]
    fn platform_kind_from_str_uk_restricted_rejected() {
        for s in ["binance_futures", "binance_margin", "binance_coin_futures"] {
            assert!(
                s.parse::<TradingPlatformKind>().is_err(),
                "{s} should be rejected (UK-restricted)"
            );
        }
    }

    #[test]
    fn platform_kind_from_str_invalid() {
        assert!("alpaka".parse::<TradingPlatformKind>().is_err());
        assert!("ALPACA".parse::<TradingPlatformKind>().is_err());
        assert!("".parse::<TradingPlatformKind>().is_err());
    }

    #[test]
    fn platform_kind_display_roundtrip() {
        let kinds = [
            TradingPlatformKind::Alpaca,
            TradingPlatformKind::BinanceSpot,
            TradingPlatformKind::Oanda,
            TradingPlatformKind::Saxo,
            TradingPlatformKind::Tektii,
            TradingPlatformKind::Mock,
        ];
        for kind in kinds {
            assert_eq!(
                kind.to_string().parse::<TradingPlatformKind>().unwrap(),
                kind
            );
        }
    }

    #[test]
    fn to_platform_exhaustive() {
        use GatewayMode::{Live, Paper};
        use TradingPlatformKind as K;

        assert_eq!(K::Alpaca.to_platform(Paper), TradingPlatform::AlpacaPaper);
        assert_eq!(K::Alpaca.to_platform(Live), TradingPlatform::AlpacaLive);
        assert_eq!(
            K::BinanceSpot.to_platform(Paper),
            TradingPlatform::BinanceSpotTestnet
        );
        assert_eq!(
            K::BinanceSpot.to_platform(Live),
            TradingPlatform::BinanceSpotLive
        );
        assert_eq!(
            K::BinanceFutures.to_platform(Paper),
            TradingPlatform::BinanceFuturesTestnet
        );
        assert_eq!(
            K::BinanceFutures.to_platform(Live),
            TradingPlatform::BinanceFuturesLive
        );
        assert_eq!(
            K::BinanceMargin.to_platform(Paper),
            TradingPlatform::BinanceMarginTestnet
        );
        assert_eq!(
            K::BinanceMargin.to_platform(Live),
            TradingPlatform::BinanceMarginLive
        );
        assert_eq!(
            K::BinanceCoinFutures.to_platform(Paper),
            TradingPlatform::BinanceCoinFuturesTestnet
        );
        assert_eq!(
            K::BinanceCoinFutures.to_platform(Live),
            TradingPlatform::BinanceCoinFuturesLive
        );
        assert_eq!(K::Oanda.to_platform(Paper), TradingPlatform::OandaPractice);
        assert_eq!(K::Oanda.to_platform(Live), TradingPlatform::OandaLive);
        assert_eq!(K::Saxo.to_platform(Paper), TradingPlatform::SaxoSim);
        assert_eq!(K::Saxo.to_platform(Live), TradingPlatform::SaxoLive);
        assert_eq!(K::Tektii.to_platform(Paper), TradingPlatform::Tektii);
        assert_eq!(K::Tektii.to_platform(Live), TradingPlatform::Tektii);
        assert_eq!(K::Mock.to_platform(Paper), TradingPlatform::Mock);
        assert_eq!(K::Mock.to_platform(Live), TradingPlatform::Mock);
    }

    #[test]
    fn gateway_mode_from_str() {
        assert_eq!("paper".parse::<GatewayMode>().unwrap(), GatewayMode::Paper);
        assert_eq!("live".parse::<GatewayMode>().unwrap(), GatewayMode::Live);
        assert!("LIVE".parse::<GatewayMode>().is_err());
        assert!("test".parse::<GatewayMode>().is_err());
    }

    #[test]
    fn gateway_mode_default_is_paper() {
        assert_eq!(GatewayMode::default(), GatewayMode::Paper);
    }

    #[test]
    fn required_env_vars_non_empty() {
        let kinds = [
            TradingPlatformKind::Alpaca,
            TradingPlatformKind::BinanceSpot,
            TradingPlatformKind::BinanceFutures,
            TradingPlatformKind::Oanda,
            TradingPlatformKind::Saxo,
            TradingPlatformKind::Tektii,
        ];
        for kind in kinds {
            assert!(
                !kind.required_env_vars().is_empty(),
                "{kind} should have required env vars"
            );
        }
    }

    #[test]
    fn mock_requires_no_env_vars() {
        assert!(
            TradingPlatformKind::Mock.required_env_vars().is_empty(),
            "Mock should require no env vars"
        );
    }
}
