//! Market data models for bars and timeframes.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Bar timeframe/interval.
///
/// Supports 12 timeframes from 1 minute to 1 week.
/// Serializes to compact string format: `"1m"`, `"1h"`, `"1d"`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema, Default)]
pub enum Timeframe {
    /// 1 minute
    #[serde(rename = "1m")]
    OneMinute,
    /// 2 minutes
    #[serde(rename = "2m")]
    TwoMinutes,
    /// 5 minutes
    #[serde(rename = "5m")]
    FiveMinutes,
    /// 10 minutes
    #[serde(rename = "10m")]
    TenMinutes,
    /// 15 minutes
    #[serde(rename = "15m")]
    FifteenMinutes,
    /// 30 minutes
    #[serde(rename = "30m")]
    ThirtyMinutes,
    /// 1 hour
    #[serde(rename = "1h")]
    #[default]
    OneHour,
    /// 2 hours
    #[serde(rename = "2h")]
    TwoHours,
    /// 4 hours
    #[serde(rename = "4h")]
    FourHours,
    /// 12 hours
    #[serde(rename = "12h")]
    TwelveHours,
    /// 1 day
    #[serde(rename = "1d")]
    OneDay,
    /// 1 week
    #[serde(rename = "1w")]
    OneWeek,
}

impl Timeframe {
    /// Get duration in seconds.
    #[must_use]
    pub const fn duration_secs(&self) -> u64 {
        match self {
            Self::OneMinute => 60,
            Self::TwoMinutes => 120,
            Self::FiveMinutes => 300,
            Self::TenMinutes => 600,
            Self::FifteenMinutes => 900,
            Self::ThirtyMinutes => 1_800,
            Self::OneHour => 3_600,
            Self::TwoHours => 7_200,
            Self::FourHours => 14_400,
            Self::TwelveHours => 43_200,
            Self::OneDay => 86_400,
            Self::OneWeek => 604_800,
        }
    }

    /// Get duration as `chrono::TimeDelta`.
    #[must_use]
    pub const fn duration(&self) -> chrono::TimeDelta {
        #[allow(clippy::cast_possible_wrap)]
        chrono::TimeDelta::seconds(self.duration_secs() as i64)
    }

    /// Get string representation (e.g., "1m", "1h").
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OneMinute => "1m",
            Self::TwoMinutes => "2m",
            Self::FiveMinutes => "5m",
            Self::TenMinutes => "10m",
            Self::FifteenMinutes => "15m",
            Self::ThirtyMinutes => "30m",
            Self::OneHour => "1h",
            Self::TwoHours => "2h",
            Self::FourHours => "4h",
            Self::TwelveHours => "12h",
            Self::OneDay => "1d",
            Self::OneWeek => "1w",
        }
    }
}

impl std::str::FromStr for Timeframe {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1m" => Ok(Self::OneMinute),
            "2m" => Ok(Self::TwoMinutes),
            "5m" => Ok(Self::FiveMinutes),
            "10m" => Ok(Self::TenMinutes),
            "15m" => Ok(Self::FifteenMinutes),
            "30m" => Ok(Self::ThirtyMinutes),
            "1h" => Ok(Self::OneHour),
            "2h" => Ok(Self::TwoHours),
            "4h" => Ok(Self::FourHours),
            "12h" => Ok(Self::TwelveHours),
            "1d" => Ok(Self::OneDay),
            "1w" => Ok(Self::OneWeek),
            _ => Err(format!(
                "Invalid timeframe: '{s}'. Valid: 1m, 2m, 5m, 10m, 15m, 30m, 1h, 2h, 4h, 12h, 1d, 1w"
            )),
        }
    }
}

impl std::fmt::Display for Timeframe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// OHLCV bar/candlestick.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Bar {
    /// Symbol in canonical format
    pub symbol: String,

    /// Data source provider
    pub provider: String,

    /// Bar timeframe
    pub timeframe: Timeframe,

    /// Bar start timestamp
    pub timestamp: DateTime<Utc>,

    /// Opening price
    pub open: Decimal,

    /// Highest price during the period
    pub high: Decimal,

    /// Lowest price during the period
    pub low: Decimal,

    /// Closing price
    pub close: Decimal,

    /// Trading volume during the period
    pub volume: Decimal,
}

/// Request parameters for historical bars.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, utoipa::IntoParams)]
#[serde(rename_all = "snake_case")]
pub struct BarParams {
    /// Bar timeframe/interval
    pub timeframe: Timeframe,

    /// Start of date range (inclusive)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,

    /// End of date range (inclusive)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,

    /// Maximum number of bars to return
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}
