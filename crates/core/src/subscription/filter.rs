//! Subscription-based event filtering for WebSocket broadcasts.
//!
//! This module provides efficient O(1) lookup filtering for events before
//! broadcasting to connected strategies. Events must match the configured
//! subscription criteria (platform, instrument, event type) to be forwarded.

use std::collections::{HashMap, HashSet};

use crate::models::{Timeframe, TradingPlatform};
use crate::websocket::messages::{InstrumentSubscription, WsMessage};

/// Event kind for zero-allocation matching in hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EventKind {
    Quote,
    OrderUpdate,
    PositionUpdate,
    TradeUpdate,
    AccountUpdate,
}

/// Events subscribed for a specific instrument.
#[derive(Debug, Default)]
struct SubscribedEvents {
    /// Subscribe to all events
    all: bool,
    /// Subscribe to all candle timeframes
    all_candles: bool,
    /// Specific candle timeframes (stored as `candle_1m` format for matching)
    candle_timeframes: HashSet<String>,
    /// Non-candle event kinds
    events: HashSet<EventKind>,
}

impl SubscribedEvents {
    fn add_event(&mut self, event: &str) {
        match event {
            "*" => self.all = true,
            "candle_*" | "bars" | "bar" => self.all_candles = true,
            "quote" | "quotes" => {
                self.events.insert(EventKind::Quote);
            }
            "order_update" | "orders" => {
                self.events.insert(EventKind::OrderUpdate);
            }
            "position_update" | "positions" => {
                self.events.insert(EventKind::PositionUpdate);
            }
            "trade_update" | "trades" => {
                self.events.insert(EventKind::TradeUpdate);
            }
            "account_update" => {
                self.events.insert(EventKind::AccountUpdate);
            }
            e if e.starts_with("candle_") => {
                self.candle_timeframes.insert(e.to_string());
            }
            e if e.starts_with("candles.") => {
                // Normalize "candles.1m" -> "candle_1m"
                let interval = &e["candles.".len()..];
                self.candle_timeframes.insert(format!("candle_{interval}"));
            }
            _ => {} // Unknown event type - ignore
        }
    }

    #[inline]
    fn matches_candle(&self, timeframe: Timeframe) -> bool {
        if self.all || self.all_candles {
            return true;
        }
        let key = format!("candle_{timeframe}");
        self.candle_timeframes.contains(&key)
    }

    #[inline]
    fn matches_event(&self, kind: EventKind) -> bool {
        self.all || self.events.contains(&kind)
    }
}

/// Pre-computed subscription filter for efficient event matching.
///
/// Built from `Vec<InstrumentSubscription>` at startup, providing O(1) lookups.
/// System events (`Connection`, `RateLimit`, `Error`, `Ping`, `Pong`) always pass through.
#[derive(Debug)]
pub struct SubscriptionFilter {
    /// Map: platform -> (instrument -> `subscribed_events`)
    /// Using nested `HashMap` for better cache locality and no tuple key allocation.
    subscriptions: HashMap<TradingPlatform, HashMap<String, SubscribedEvents>>,

    /// Map: platform -> `subscribed_events` for wildcard instruments ("*")
    platform_wildcards: HashMap<TradingPlatform, SubscribedEvents>,

    /// Map: platform -> true if `account_update` is subscribed
    /// Account events have no symbol, so tracked separately.
    account_subscriptions: HashMap<TradingPlatform, bool>,
}

impl SubscriptionFilter {
    /// Create a new subscription filter from a list of subscriptions.
    #[must_use]
    pub fn new(subscriptions: &[InstrumentSubscription]) -> Self {
        let mut filter = Self {
            subscriptions: HashMap::with_capacity(subscriptions.len()),
            platform_wildcards: HashMap::new(),
            account_subscriptions: HashMap::new(),
        };

        for sub in subscriptions {
            // Track account subscriptions at platform level
            if sub.events.iter().any(|e| e == "account_update" || e == "*") {
                filter.account_subscriptions.insert(sub.platform, true);
            }

            if sub.instrument == "*" {
                // Wildcard instrument
                let events = filter.platform_wildcards.entry(sub.platform).or_default();
                for event in &sub.events {
                    events.add_event(event);
                }
            } else {
                // Specific instrument
                let platform_subs = filter.subscriptions.entry(sub.platform).or_default();
                let events = platform_subs.entry(sub.instrument.clone()).or_default();
                for event in &sub.events {
                    events.add_event(event);
                }
            }
        }

        filter
    }

    /// Check if an event matches the subscription filter.
    ///
    /// Returns `true` if the event should be forwarded to strategies.
    ///
    /// # Arguments
    ///
    /// * `event` - The WebSocket message to check
    /// * `platform` - The trading platform this event came from
    #[must_use]
    pub fn matches(&self, event: &WsMessage, platform: TradingPlatform) -> bool {
        // Empty filter = pass all events (no static subscriptions configured)
        if self.subscriptions.is_empty() && self.platform_wildcards.is_empty() {
            return true;
        }

        // System events always pass through
        if Self::is_system_event(event) {
            return true;
        }

        match event {
            WsMessage::Candle { bar, .. } => {
                self.matches_candle(platform, &bar.symbol, bar.timeframe)
            }
            WsMessage::QuoteData { quote, .. } => {
                self.matches_simple(platform, &quote.symbol, EventKind::Quote)
            }
            WsMessage::Order { order, .. } => {
                self.matches_simple(platform, &order.symbol, EventKind::OrderUpdate)
            }
            WsMessage::Position { position, .. } => {
                self.matches_simple(platform, &position.symbol, EventKind::PositionUpdate)
            }
            WsMessage::Trade { trade, .. } => {
                self.matches_simple(platform, &trade.symbol, EventKind::TradeUpdate)
            }
            WsMessage::Account { .. } => self
                .account_subscriptions
                .get(&platform)
                .copied()
                .unwrap_or(false),
            // Client messages and internal signals shouldn't reach filter
            WsMessage::Pong | WsMessage::EventAck { .. } | WsMessage::Close { .. } => false,
            // System events handled above, but match exhaustively
            WsMessage::Connection { .. }
            | WsMessage::DataStaleness { .. }
            | WsMessage::RateLimit { .. }
            | WsMessage::Error { .. }
            | WsMessage::Ping { .. } => true,
        }
    }

    #[inline]
    fn matches_candle(
        &self,
        platform: TradingPlatform,
        symbol: &str,
        timeframe: Timeframe,
    ) -> bool {
        // Check exact instrument match
        if let Some(platform_subs) = self.subscriptions.get(&platform)
            && let Some(events) = platform_subs.get(symbol)
            && events.matches_candle(timeframe)
        {
            return true;
        }

        // Check wildcard
        if let Some(events) = self.platform_wildcards.get(&platform)
            && events.matches_candle(timeframe)
        {
            return true;
        }

        false
    }

    #[inline]
    fn matches_simple(&self, platform: TradingPlatform, symbol: &str, kind: EventKind) -> bool {
        // Check exact instrument match
        if let Some(platform_subs) = self.subscriptions.get(&platform)
            && let Some(events) = platform_subs.get(symbol)
            && events.matches_event(kind)
        {
            return true;
        }

        // Check wildcard
        if let Some(events) = self.platform_wildcards.get(&platform)
            && events.matches_event(kind)
        {
            return true;
        }

        false
    }

    /// Check if this is a system event that should always pass through.
    #[inline]
    const fn is_system_event(event: &WsMessage) -> bool {
        matches!(
            event,
            WsMessage::Connection { .. }
                | WsMessage::DataStaleness { .. }
                | WsMessage::RateLimit { .. }
                | WsMessage::Error { .. }
                | WsMessage::Ping { .. }
                | WsMessage::Pong
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        Account, Bar, Order, OrderStatus, OrderType, Position, PositionSide, Quote, Side,
        TimeInForce, Timeframe, Trade,
    };
    use crate::websocket::messages::{
        AccountEventType, ConnectionEventType, OrderEventType, PositionEventType,
        RateLimitEventType, TradeEventType, WsErrorCode,
    };
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn test_subscription(
        platform: TradingPlatform,
        instrument: &str,
        events: Vec<&str>,
    ) -> InstrumentSubscription {
        InstrumentSubscription {
            platform,
            instrument: instrument.to_string(),
            events: events.into_iter().map(String::from).collect(),
        }
    }

    fn test_order(symbol: &str) -> Order {
        Order {
            id: "test-order".to_string(),
            client_order_id: None,
            symbol: symbol.to_string(),
            side: Side::Buy,
            order_type: OrderType::Market,
            quantity: dec!(1),
            filled_quantity: dec!(0),
            remaining_quantity: dec!(1),
            limit_price: None,
            stop_price: None,
            stop_loss: None,
            take_profit: None,
            trailing_distance: None,
            trailing_type: None,
            average_fill_price: None,
            status: OrderStatus::Pending,
            reject_reason: None,
            position_id: None,
            reduce_only: None,
            post_only: None,
            hidden: None,
            display_quantity: None,
            oco_group_id: None,
            correlation_id: None,
            time_in_force: TimeInForce::Gtc,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn test_position(symbol: &str) -> Position {
        Position {
            id: "test-position".to_string(),
            symbol: symbol.to_string(),
            side: PositionSide::Long,
            quantity: dec!(1),
            average_entry_price: dec!(100),
            current_price: dec!(105),
            unrealized_pnl: dec!(5),
            realized_pnl: dec!(0),
            margin_mode: None,
            leverage: None,
            liquidation_price: None,
            opened_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn test_bar(symbol: &str, timeframe: Timeframe) -> Bar {
        Bar {
            symbol: symbol.to_string(),
            provider: "test".to_string(),
            timeframe,
            open: dec!(100),
            high: dec!(105),
            low: dec!(99),
            close: dec!(102),
            volume: dec!(1000),
            timestamp: Utc::now(),
        }
    }

    fn test_quote(symbol: &str) -> Quote {
        Quote {
            symbol: symbol.to_string(),
            provider: "test".to_string(),
            bid: dec!(99),
            bid_size: None,
            ask: dec!(101),
            ask_size: None,
            last: dec!(100),
            volume: None,
            timestamp: Utc::now(),
        }
    }

    fn test_trade(symbol: &str) -> Trade {
        Trade {
            id: "test-trade".to_string(),
            order_id: "test-order".to_string(),
            symbol: symbol.to_string(),
            side: Side::Buy,
            quantity: dec!(1),
            price: dec!(100),
            commission: dec!(0.1),
            commission_currency: "USD".to_string(),
            is_maker: None,
            timestamp: Utc::now(),
        }
    }

    fn test_account() -> Account {
        Account {
            balance: dec!(10000),
            equity: dec!(10500),
            margin_used: dec!(1000),
            margin_available: dec!(9500),
            unrealized_pnl: dec!(500),
            currency: "USD".to_string(),
        }
    }

    // =========================================================================
    // System events always pass
    // =========================================================================

    #[test]
    fn test_system_events_always_pass() {
        let filter = SubscriptionFilter::new(&[]);
        let now = Utc::now();

        // Empty filter should still pass system events
        assert!(filter.matches(
            &WsMessage::Connection {
                event: ConnectionEventType::Connected,
                error: None,
                broker: None,
                gap_duration_ms: None,
                timestamp: now,
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::RateLimit {
                event: RateLimitEventType::RateLimitWarning,
                requests_remaining: 10,
                reset_at: now,
                timestamp: now,
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Error {
                code: WsErrorCode::InternalError,
                message: "test".to_string(),
                details: None,
                timestamp: now,
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Ping { timestamp: now },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(&WsMessage::Pong, TradingPlatform::AlpacaPaper));
    }

    // =========================================================================
    // Exact instrument match
    // =========================================================================

    #[test]
    fn test_exact_instrument_match() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["order_update"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Should match AAPL order_update
        let order_event = WsMessage::Order {
            event: OrderEventType::OrderCreated,
            order: test_order("AAPL"),
            parent_order_id: None,
            timestamp: Utc::now(),
        };
        assert!(filter.matches(&order_event, TradingPlatform::AlpacaPaper));

        // Should NOT match TSLA (different instrument)
        let tsla_event = WsMessage::Order {
            event: OrderEventType::OrderCreated,
            order: test_order("TSLA"),
            parent_order_id: None,
            timestamp: Utc::now(),
        };
        assert!(!filter.matches(&tsla_event, TradingPlatform::AlpacaPaper));

        // Should NOT match AAPL position_update (different event type)
        let position_event = WsMessage::Position {
            event: PositionEventType::PositionOpened,
            position: test_position("AAPL"),
            timestamp: Utc::now(),
        };
        assert!(!filter.matches(&position_event, TradingPlatform::AlpacaPaper));
    }

    // =========================================================================
    // Multiple event types
    // =========================================================================

    #[test]
    fn test_multiple_event_types() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["order_update", "position_update"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        assert!(filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderFilled,
                order: test_order("AAPL"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Position {
                event: PositionEventType::PositionOpened,
                position: test_position("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    // =========================================================================
    // Wildcard instrument
    // =========================================================================

    #[test]
    fn test_wildcard_instrument() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "*",
            vec!["order_update"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Should match any instrument
        assert!(filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order: test_order("AAPL"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order: test_order("TSLA"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        // But still respects event type filter
        assert!(!filter.matches(
            &WsMessage::Position {
                event: PositionEventType::PositionOpened,
                position: test_position("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    // =========================================================================
    // Wildcard event
    // =========================================================================

    #[test]
    fn test_wildcard_event() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["*"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Should match any event type for AAPL
        assert!(filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order: test_order("AAPL"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Position {
                event: PositionEventType::PositionOpened,
                position: test_position("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::QuoteData {
                quote: test_quote("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    // =========================================================================
    // Candle events
    // =========================================================================

    #[test]
    fn test_candle_specific_timeframe() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["candle_1m"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Should match 1m candle
        assert!(filter.matches(
            &WsMessage::Candle {
                bar: test_bar("AAPL", Timeframe::OneMinute),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        // Should NOT match 5m candle
        assert!(!filter.matches(
            &WsMessage::Candle {
                bar: test_bar("AAPL", Timeframe::FiveMinutes),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    #[test]
    fn test_candle_wildcard() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["candle_*"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Should match any candle timeframe
        assert!(filter.matches(
            &WsMessage::Candle {
                bar: test_bar("AAPL", Timeframe::OneMinute),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Candle {
                bar: test_bar("AAPL", Timeframe::FiveMinutes),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Candle {
                bar: test_bar("AAPL", Timeframe::OneHour),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    #[test]
    fn test_bars_alias_matches_candles() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["bars"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // "bars" should match any candle timeframe (same as "candle_*")
        assert!(filter.matches(
            &WsMessage::Candle {
                bar: test_bar("AAPL", Timeframe::OneMinute),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::Candle {
                bar: test_bar("AAPL", Timeframe::FiveMinutes),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        // But non-candle events should NOT match
        assert!(!filter.matches(
            &WsMessage::QuoteData {
                quote: test_quote("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    // =========================================================================
    // Quote events
    // =========================================================================

    #[test]
    fn test_quote_events() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["quote"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        assert!(filter.matches(
            &WsMessage::QuoteData {
                quote: test_quote("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(!filter.matches(
            &WsMessage::QuoteData {
                quote: test_quote("TSLA"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    // =========================================================================
    // Trade events
    // =========================================================================

    #[test]
    fn test_trade_events() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["trade_update"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        assert!(filter.matches(
            &WsMessage::Trade {
                event: TradeEventType::TradeFilled,
                trade: test_trade("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    // =========================================================================
    // Account events (global, no symbol)
    // =========================================================================

    #[test]
    fn test_account_events() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["account_update"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Account events have no symbol, should match if account_update is subscribed
        assert!(filter.matches(
            &WsMessage::Account {
                event: AccountEventType::BalanceUpdated,
                account: test_account(),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    #[test]
    fn test_account_events_not_subscribed() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["order_update"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Should NOT match if account_update not in subscriptions
        assert!(!filter.matches(
            &WsMessage::Account {
                event: AccountEventType::BalanceUpdated,
                account: test_account(),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));
    }

    // =========================================================================
    // Platform isolation
    // =========================================================================

    #[test]
    fn test_platform_isolation() {
        let subs = vec![test_subscription(
            TradingPlatform::AlpacaPaper,
            "AAPL",
            vec!["order_update"],
        )];
        let filter = SubscriptionFilter::new(&subs);

        // Should NOT match different platform
        assert!(!filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order: test_order("AAPL"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::BinanceSpotLive
        ));
    }

    // =========================================================================
    // Multiple subscriptions
    // =========================================================================

    #[test]
    fn test_multiple_subscriptions() {
        let subs = vec![
            test_subscription(TradingPlatform::AlpacaPaper, "AAPL", vec!["order_update"]),
            test_subscription(
                TradingPlatform::BinanceSpotLive,
                "BTCUSDT",
                vec!["position_update"],
            ),
        ];
        let filter = SubscriptionFilter::new(&subs);

        // Alpaca AAPL order
        assert!(filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order: test_order("AAPL"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        // Binance BTCUSDT position
        assert!(filter.matches(
            &WsMessage::Position {
                event: PositionEventType::PositionOpened,
                position: test_position("BTCUSDT"),
                timestamp: Utc::now(),
            },
            TradingPlatform::BinanceSpotLive
        ));

        // Wrong combinations
        assert!(!filter.matches(
            &WsMessage::Position {
                event: PositionEventType::PositionOpened,
                position: test_position("AAPL"),
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(!filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order: test_order("BTCUSDT"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::BinanceSpotLive
        ));
    }

    // =========================================================================
    // Empty filter
    // =========================================================================

    #[test]
    fn test_empty_filter_passes_all() {
        let filter = SubscriptionFilter::new(&[]);

        // Empty filter = pass all events (gateway starts with REST-only, no static subscriptions)
        assert!(filter.matches(
            &WsMessage::Order {
                event: OrderEventType::OrderCreated,
                order: test_order("AAPL"),
                parent_order_id: None,
                timestamp: Utc::now(),
            },
            TradingPlatform::AlpacaPaper
        ));

        assert!(filter.matches(
            &WsMessage::QuoteData {
                quote: test_quote("TSLA"),
                timestamp: Utc::now(),
            },
            TradingPlatform::BinanceSpotLive
        ));
    }
}
