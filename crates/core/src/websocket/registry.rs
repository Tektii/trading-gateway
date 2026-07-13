//! Provider registry for managing multiple WebSocket provider connections.
//!
//! The registry is responsible for:
//! - Accepting already-connected provider instances
//! - Managing the lifecycle of provider connections
//! - Broadcasting events from providers to connected strategies
//! - Supporting shared provider models (one connection per platform, many strategies)
//!
//! Unlike the original proxy registry, this gateway registry does NOT construct
//! providers. Providers are created and connected externally, then registered
//! via [`ProviderRegistry::register_provider`].

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::{RwLock, broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::adapter::TradingAdapter;
use crate::config::ReconnectionConfig;
use crate::correlation::CorrelationStore;
use crate::events::router::EventRouter;
use crate::models::{Order, PositionSide, Side, TradingPlatform};
use crate::subscription::filter::SubscriptionFilter;
use crate::websocket::ack::AckBridge;
use crate::websocket::connection::WsConnectionManager;
use crate::websocket::error::WebSocketError;
use crate::websocket::messages::{InternalTradingEvent, OrderEventType, WsMessage};
use crate::websocket::provider::{EventStream, ProviderEvent, WebSocketProvider};
use crate::websocket::reconnection::ReconnectionHandler;
use crate::websocket::staleness::StalenessTracker;

/// Shared provider state for all strategy connections.
struct SharedProvider {
    /// Provider implementation.
    provider: Box<dyn WebSocketProvider>,

    /// Event stream from provider.
    stream: Option<EventStream>,

    /// Background task handling events.
    task: Option<JoinHandle<()>>,

    /// Platform type.
    platform_type: TradingPlatform,

    /// Symbols subscribed to.
    symbols: Vec<String>,

    /// Event types subscribed to.
    event_types: Vec<String>,

    /// Per-instrument staleness tracker (stale during broker disconnect).
    staleness: Arc<StalenessTracker>,

    /// Whether the provider already filters events at the source
    /// ([`WebSocketProvider::filters_events_upstream`]). Captured at register
    /// time to avoid re-locking the provider on every event in the broadcast
    /// loop. When `true`, the registry skips its own subscription filter.
    filters_events_upstream: bool,
}

/// Registry for managing WebSocket providers.
///
/// Supports multiple providers simultaneously (e.g., Alpaca and Binance).
/// Each provider gets its own connection and event stream.
///
/// # Internal Event Channel
///
/// The registry provides an internal broadcast channel for order updates,
/// allowing internal services (like `EventRouter`) to receive fill and
/// cancellation events WITHOUT connecting as WebSocket clients.
///
/// # Event Routing
///
/// The registry can route events through per-adapter `EventRouters`, which:
/// - Update `StateManager` with order/position state
/// - Route fill events to `ExitHandler` for SL/TP placement
/// - Then broadcast to connected strategies
pub struct ProviderRegistry {
    /// Shared providers for all connections, keyed by platform.
    shared_providers: Arc<RwLock<HashMap<TradingPlatform, SharedProvider>>>,

    /// Set of connected strategy connection IDs.
    connected_strategies: Arc<RwLock<HashSet<Uuid>>>,

    /// Connection manager for strategy WebSocket connections.
    connection_manager: Arc<WsConnectionManager>,

    /// `EventRouters` for per-adapter event processing.
    event_routers: Arc<RwLock<HashMap<TradingPlatform, Arc<EventRouter>>>>,

    /// Trading adapters for placing exit orders during event processing.
    trading_adapters: Arc<RwLock<HashMap<TradingPlatform, Arc<dyn TradingAdapter>>>>,

    /// Internal broadcast channel for order updates with position context.
    internal_broadcast_sender: broadcast::Sender<InternalTradingEvent>,

    /// Subscription filter for event filtering before broadcast.
    subscription_filter: Arc<SubscriptionFilter>,

    /// ACK bridge for Tektii adapter (optional).
    tektii_ack_bridge: Arc<RwLock<Option<Arc<dyn AckBridge>>>>,

    /// Cancellation token for shutdown.
    cancellation_token: CancellationToken,

    /// Reconnection configuration for broker WebSocket streams.
    reconnection_config: ReconnectionConfig,

    /// Correlation store for enriching order events with gateway-assigned correlation IDs.
    correlation_store: Arc<CorrelationStore>,

    /// Price source for trailing stop tracking (receives market data quotes).
    price_source: Arc<RwLock<Option<Arc<crate::trailing_stop::WebSocketPriceSource>>>>,

    /// Latch that flips to `true` when the strategy connects and never reverts.
    /// The gateway serves a single strategy; the Tektii engine provider awaits
    /// this before connecting, so the engine never starts its simulation before
    /// the strategy is attached to consume the events it emits on connect.
    strategy_connected: watch::Sender<bool>,
}

impl ProviderRegistry {
    /// Create a new, empty provider registry.
    ///
    /// Providers are registered later via [`Self::register_provider`].
    ///
    /// # Arguments
    ///
    /// * `connection_manager` - Manager for strategy WebSocket connections
    /// * `subscription_filter` - Filter for event filtering before broadcast
    /// * `cancellation_token` - Token for graceful shutdown
    pub fn new(
        connection_manager: Arc<WsConnectionManager>,
        subscription_filter: SubscriptionFilter,
        cancellation_token: CancellationToken,
        reconnection_config: ReconnectionConfig,
        correlation_store: Arc<CorrelationStore>,
    ) -> Self {
        let (internal_tx, _) = broadcast::channel(256);

        Self {
            shared_providers: Arc::new(RwLock::new(HashMap::new())),
            connected_strategies: Arc::new(RwLock::new(HashSet::new())),
            connection_manager,
            event_routers: Arc::new(RwLock::new(HashMap::new())),
            trading_adapters: Arc::new(RwLock::new(HashMap::new())),
            internal_broadcast_sender: internal_tx,
            subscription_filter: Arc::new(subscription_filter),
            tektii_ack_bridge: Arc::new(RwLock::new(None)),
            cancellation_token,
            reconnection_config,
            correlation_store,
            price_source: Arc::new(RwLock::new(None)),
            strategy_connected: watch::channel(false).0,
        }
    }

    /// Set the WebSocket price source for trailing stop tracking.
    ///
    /// When set, `QuoteData` events from provider streams will be forwarded
    /// to the price source for trailing stop price tracking.
    pub async fn set_price_source(&self, source: Arc<crate::trailing_stop::WebSocketPriceSource>) {
        *self.price_source.write().await = Some(source);
    }

    /// Ensure the provider is subscribed to quote events for a symbol.
    ///
    /// If the symbol is already in the provider's subscription list, this is a no-op.
    /// Otherwise, sends a dynamic subscribe request to the provider.
    pub async fn ensure_quote_subscription(
        &self,
        symbol: &str,
    ) -> Result<(), crate::error::GatewayError> {
        let mut providers = self.shared_providers.write().await;

        for (_, shared) in providers.iter_mut() {
            if shared.symbols.contains(&symbol.to_string())
                && shared.event_types.iter().any(|e| e == "quote")
            {
                return Ok(());
            }

            if let Err(e) = shared
                .provider
                .subscribe(vec![symbol.to_string()], vec!["quote".to_string()])
                .await
            {
                tracing::warn!(
                    symbol,
                    error = %e,
                    "Failed to dynamically subscribe to quote events"
                );
                continue;
            }

            if !shared.symbols.contains(&symbol.to_string()) {
                shared.symbols.push(symbol.to_string());
            }
            if !shared.event_types.contains(&"quote".to_string()) {
                shared.event_types.push("quote".to_string());
            }

            tracing::debug!(
                symbol,
                platform = ?shared.platform_type,
                "Dynamically subscribed to quote events for trailing stop"
            );
            return Ok(());
        }

        // No provider available — not an error, quotes may arrive later
        tracing::debug!(symbol, "No provider available for quote subscription");
        Ok(())
    }

    /// Register an already-connected provider.
    ///
    /// The provider should already be connected and ready to stream events.
    /// The registry will start consuming its event stream and routing events
    /// to connected strategies.
    ///
    /// # Arguments
    ///
    /// * `platform` - The platform this provider serves
    /// * `provider` - Connected provider instance
    /// * `stream` - Event stream from the provider
    /// * `symbols` - Symbols the provider is subscribed to
    /// * `event_types` - Event types the provider is subscribed to
    ///
    /// # Errors
    ///
    /// Returns `WebSocketError::ConfigError` if the platform is already registered.
    pub async fn register_provider(
        &self,
        platform: TradingPlatform,
        provider: Box<dyn WebSocketProvider>,
        stream: EventStream,
        symbols: Vec<String>,
        event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        {
            let shared = self.shared_providers.read().await;
            if shared.contains_key(&platform) {
                return Err(WebSocketError::ConfigError(format!(
                    "Platform {platform} is already registered"
                )));
            }
        }

        let filters_events_upstream = provider.filters_events_upstream();
        let shared = SharedProvider {
            provider,
            stream: Some(stream),
            task: None,
            platform_type: platform,
            symbols,
            event_types,
            staleness: Arc::new(StalenessTracker::new()),
            filters_events_upstream,
        };

        {
            let mut shared_guard = self.shared_providers.write().await;
            shared_guard.insert(platform, shared);
        }

        self.spawn_provider_event_task(platform).await;

        info!(platform = %platform, "Provider registered and broadcasting started");
        Ok(())
    }

    /// Register an `EventRouter` for a platform.
    ///
    /// When events arrive from this platform, they will be routed through the
    /// `EventRouter` before broadcasting to strategies. The `EventRouter` updates
    /// `StateManager` and routes fills to `ExitHandler`.
    ///
    /// # Arguments
    ///
    /// * `platform` - The platform to register the router for
    /// * `router` - The `EventRouter` instance
    /// * `adapter` - The `TradingAdapter` for placing exit orders
    ///
    /// # Errors
    ///
    /// Returns `WebSocketError::ConfigError` if the adapter is already set.
    pub async fn register_event_router(
        &self,
        platform: TradingPlatform,
        router: Arc<EventRouter>,
        adapter: Arc<dyn TradingAdapter>,
    ) -> Result<(), WebSocketError> {
        if router.set_adapter(Arc::clone(&adapter)).is_err() {
            return Err(WebSocketError::ConfigError(format!(
                "EventRouter adapter already set for platform {platform} — double registration is a bug",
            )));
        }

        {
            let mut routers = self.event_routers.write().await;
            routers.insert(platform, router);
        }
        {
            let mut adapters = self.trading_adapters.write().await;
            adapters.insert(platform, adapter);
        }
        info!(platform = %platform, "EventRouter registered for platform");
        Ok(())
    }

    /// Get the `EventRouter` for a platform (if registered).
    pub async fn get_event_router(&self, platform: TradingPlatform) -> Option<Arc<EventRouter>> {
        let routers = self.event_routers.read().await;
        routers.get(&platform).cloned()
    }

    /// Get the `TradingAdapter` for a platform (if registered).
    pub async fn get_trading_adapter(
        &self,
        platform: TradingPlatform,
    ) -> Option<Arc<dyn TradingAdapter>> {
        let adapters = self.trading_adapters.read().await;
        adapters.get(&platform).cloned()
    }

    /// Check if event routing is enabled for a platform.
    pub async fn has_event_router(&self, platform: TradingPlatform) -> bool {
        let routers = self.event_routers.read().await;
        routers.contains_key(&platform)
    }

    /// Set the ACK bridge for routing strategy ACKs.
    ///
    /// This should be called when setting up a provider that requires
    /// acknowledgment-based time synchronization (e.g., Tektii engine).
    pub async fn set_tektii_ack_bridge(&self, bridge: Arc<dyn AckBridge>) {
        let mut ack_bridge = self.tektii_ack_bridge.write().await;
        *ack_bridge = Some(bridge);
        drop(ack_bridge);
        info!("ACK bridge registered");
    }

    /// Handle strategy ACK by forwarding the acked `event_id`s to the ACK
    /// bridge, which releases exactly those events (strict-ID release).
    ///
    /// The gateway echoes the engine `event_id` on every outbound engine
    /// frame and the SDK returns it in `events_processed`, so an ACK names
    /// precisely what it acknowledges. Gateway-local events (connection /
    /// data-freshness / error / rate-limit) carry no engine id and produce
    /// empty ACKs, which release nothing — they can never pop an engine
    /// event and advance the sim ahead of the strategy. Unknown ids release
    /// nothing either, and an ACK for a delivered event releases it even
    /// when an older, never-delivered event is still pending — a delivery
    /// loss costs the engine one ACK-window timeout instead of wedging the
    /// run.
    pub async fn handle_strategy_ack(&self, events_processed: &[String]) {
        let bridge = self.tektii_ack_bridge.read().await;
        if let Some(ref b) = *bridge {
            if !events_processed.is_empty() {
                info!(
                    acked = events_processed.len(),
                    "Forwarding strategy ACK to bridge"
                );
            }
            b.handle_strategy_ack(events_processed).await;
        } else {
            debug!("No ACK bridge set - ACK is informational only");
        }
    }

    /// Subscribe to internal order updates.
    ///
    /// Returns a receiver that will receive `InternalTradingEvent` wrappers
    /// containing order events for fills, cancellations, and other state changes.
    #[must_use]
    pub fn subscribe_order_updates(&self) -> broadcast::Receiver<InternalTradingEvent> {
        self.internal_broadcast_sender.subscribe()
    }

    /// Get the internal order updates sender.
    #[must_use]
    pub fn order_updates_sender(&self) -> broadcast::Sender<InternalTradingEvent> {
        self.internal_broadcast_sender.clone()
    }

    /// Register a strategy connection to receive events.
    pub async fn register_strategy_connection(&self, conn_id: Uuid) {
        let mut strategies = self.connected_strategies.write().await;
        strategies.insert(conn_id);
        self.strategy_connected.send_replace(true);
        info!(
            conn_id = %conn_id,
            total_strategies = strategies.len(),
            "Strategy connection registered"
        );
    }

    /// Wait until the strategy has connected to the gateway.
    ///
    /// The gateway serves a single strategy. Returns immediately if it has
    /// already connected. The Tektii engine provider awaits this before opening
    /// its WebSocket: the engine begins replaying its simulation the moment the
    /// gateway connects, so connecting before the strategy is attached would
    /// emit the first events into the void and desynchronise the ACK stream.
    pub async fn wait_for_strategy(&self) {
        let mut rx = self.strategy_connected.subscribe();
        // Latch never reverts once true, so a single satisfied check is enough.
        let _ = rx.wait_for(|connected| *connected).await;
    }

    /// Unregister a strategy connection.
    pub async fn unregister_strategy_connection(&self, conn_id: Uuid) {
        let mut strategies = self.connected_strategies.write().await;
        strategies.remove(&conn_id);
        info!(
            conn_id = %conn_id,
            total_strategies = strategies.len(),
            "Strategy connection unregistered"
        );
    }

    /// Get number of connected strategies.
    #[must_use]
    pub async fn connected_strategy_count(&self) -> usize {
        self.connected_strategies.read().await.len()
    }

    /// Check if at least one shared provider is connected.
    #[must_use]
    pub async fn is_shared_provider_connected(&self) -> bool {
        !self.shared_providers.read().await.is_empty()
    }

    /// Get list of currently connected platforms.
    #[must_use]
    pub async fn connected_platforms(&self) -> Vec<TradingPlatform> {
        self.shared_providers.read().await.keys().copied().collect()
    }

    /// Get the number of connected shared providers.
    #[must_use]
    pub async fn shared_provider_count(&self) -> usize {
        self.shared_providers.read().await.len()
    }

    /// Check if all required platforms are connected.
    #[must_use]
    pub async fn are_all_platforms_connected(
        &self,
        required_platforms: &HashSet<TradingPlatform>,
    ) -> bool {
        let connected = self.shared_providers.read().await;
        required_platforms.iter().all(|p| connected.contains_key(p))
    }

    /// Get stale instruments for a platform.
    ///
    /// Returns `None` if the platform is not registered.
    /// Returns `Some(vec![])` if all instruments have fresh data.
    pub async fn get_staleness(&self, platform: TradingPlatform) -> Option<Vec<String>> {
        let shared = self.shared_providers.read().await;
        shared
            .get(&platform)
            .map(|p| p.staleness.stale_instruments())
    }

    /// Get stale instruments with their stale-since timestamps.
    ///
    /// Used to send initial staleness snapshot to newly connected strategies.
    pub async fn get_staleness_with_times(
        &self,
        platform: TradingPlatform,
    ) -> Option<Vec<(String, DateTime<Utc>)>> {
        let shared = self.shared_providers.read().await;
        shared
            .get(&platform)
            .map(|p| p.staleness.stale_instruments_with_times())
    }

    /// Spawn a task to process internal trading events and synthesize position events.
    ///
    /// This task receives `InternalTradingEvent` from the internal channel and:
    /// 1. Handles reconnection signals (triggers reconciliation)
    /// 2. Synthesizes position events from fill data
    ///
    /// Note: order event forwarding to strategy clients is handled by
    /// `spawn_provider_event_task`, not here (avoids duplicate delivery).
    pub fn spawn_position_synthesis_task(&self) {
        let mut receiver = self.internal_broadcast_sender.subscribe();
        let event_routers = self.event_routers.clone();
        let cancel = self.cancellation_token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        info!("Position synthesis task cancelled");
                        break;
                    }
                    result = receiver.recv() => {
                        match result {
                            Ok(internal_event) => {
                                if matches!(
                                    &internal_event.message,
                                    WsMessage::Connection {
                                        event: crate::websocket::messages::ConnectionEventType::Connected,
                                        ..
                                    }
                                ) {
                                    let router = {
                                        let routers = event_routers.read().await;
                                        routers.get(&internal_event.platform).cloned()
                                    };
                                    if let Some(router) = router {
                                        info!(
                                            platform = ?internal_event.platform,
                                            "Received reconnect signal, starting reconciliation"
                                        );
                                        router.reconcile_after_reconnect().await;
                                    }
                                    continue;
                                }

                                // Only process fill events for position synthesis
                                // (order event forwarding to strategies is handled by
                                // spawn_provider_event_task — not duplicated here)
                                let order = match &internal_event.message {
                                    WsMessage::Order { order, event, .. } => {
                                        if matches!(event, OrderEventType::OrderFilled | OrderEventType::OrderPartiallyFilled) {
                                            order.clone()
                                        } else {
                                            continue;
                                        }
                                    }
                                    _ => continue,
                                };

                                let router = {
                                    let routers = event_routers.read().await;
                                    routers.get(&internal_event.platform).cloned()
                                };

                                let Some(router) = router else {
                                    continue;
                                };

                                let position_qty = internal_event.context
                                    .as_ref()
                                    .and_then(|ctx| ctx.position_qty)
                                    .unwrap_or_else(|| {
                                        let last_fill_qty = internal_event.context
                                            .as_ref()
                                            .and_then(|ctx| ctx.last_fill_qty);
                                        Self::calculate_position_qty_from_fill(&router, &order, last_fill_qty)
                                    });

                                debug!(
                                    symbol = %order.symbol,
                                    position_qty = %position_qty,
                                    platform = ?internal_event.platform,
                                    has_provider_qty = internal_event.context.as_ref().and_then(|c| c.position_qty).is_some(),
                                    "Synthesizing position from fill"
                                );

                                router
                                    .synthesize_position_from_fill(&order, position_qty)
                                    .await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!(
                                    lagged_count = n,
                                    "Position synthesis task lagged behind, some events may have been missed"
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                info!("Internal order updates channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Calculate `position_qty` from `StateManager` and fill delta when provider
    /// doesn't supply it (e.g., Binance execution reports).
    ///
    /// The calculation:
    /// - BUY: `previous_qty` + `fill_delta`
    /// - SELL: `previous_qty` - `fill_delta`
    fn calculate_position_qty_from_fill(
        router: &Arc<EventRouter>,
        order: &Order,
        last_fill_qty: Option<Decimal>,
    ) -> Decimal {
        let previous_qty = router
            .state_manager()
            .get_position_by_symbol(&order.symbol)
            .map_or(Decimal::ZERO, |p| {
                if p.side == PositionSide::Short {
                    -p.quantity
                } else {
                    p.quantity
                }
            });

        let fill_delta = last_fill_qty.unwrap_or(order.filled_quantity);

        match order.side {
            Side::Buy => previous_qty + fill_delta,
            Side::Sell => previous_qty - fill_delta,
        }
    }

    /// Spawn background task for a platform to broadcast events.
    ///
    /// If an `EventRouter` is registered for this platform, events are routed
    /// through it for state management and exit order handling before
    /// broadcasting to strategies.
    ///
    /// When the provider stream closes, the task will attempt automatic
    /// reconnection with exponential backoff (unless the provider doesn't
    /// support reconnection, e.g., Tektii engine).
    #[allow(clippy::too_many_lines)]
    async fn spawn_provider_event_task(&self, platform: TradingPlatform) {
        let shared_providers_clone = self.shared_providers.clone();
        let connected_strategies_clone = self.connected_strategies.clone();
        let connection_manager = self.connection_manager.clone();
        let internal_tx = self.internal_broadcast_sender.clone();
        let cancel = self.cancellation_token.clone();
        let event_routers = self.event_routers.clone();
        let trading_adapters = self.trading_adapters.clone();
        let subscription_filter = self.subscription_filter.clone();
        let reconnection_config = self.reconnection_config.clone();
        let correlation_store = self.correlation_store.clone();
        let price_source = self.price_source.clone();
        let tektii_ack_bridge = self.tektii_ack_bridge.clone();

        let task = tokio::spawn(async move {
            // Snapshot the price source once (set during startup, never changes)
            let price_source_snapshot = price_source.read().await.clone();

            // Take the stream and staleness tracker from the provider
            let (
                stream_opt,
                staleness,
                supports_reconnection,
                filters_events_upstream,
                end_of_stream_is_completion,
            ) = {
                let mut shared_guard = shared_providers_clone.write().await;
                if let Some(p) = shared_guard.get_mut(&platform) {
                    (
                        p.stream.take(),
                        Arc::clone(&p.staleness),
                        p.provider.supports_reconnection(),
                        p.filters_events_upstream,
                        p.provider.end_of_stream_is_completion(),
                    )
                } else {
                    (None, Arc::new(StalenessTracker::new()), false, false, false)
                }
            };

            let Some(mut stream) = stream_opt else {
                return;
            };

            // Tracks whether the provider relayed an in-band `BacktestComplete`
            // terminal during this session (tektii mode). When set, the
            // stream-close path must NOT synthesize a second terminal — the real
            // one already round-tripped its flush-ack. The synthesized terminal
            // stays a fallback for providers that signal end-of-backtest only by
            // closing the stream (older engine, or an aborted run).
            let mut backtest_terminal_relayed = false;

            'outer: loop {
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            info!(platform = %platform, "Platform task received shutdown signal");
                            break 'outer;
                        }
                        msg = stream.recv() => {
                            if let Some(envelope) = msg {
                                let ProviderEvent { msg: event, engine_event_id } = envelope;
                                debug!(platform = %platform, "Broadcasting event: {:?}", event);

                                // Record that the provider relayed a real
                                // end-of-backtest terminal — suppresses the
                                // synthesized fallback on the stream-close path
                                // so the strategy sees exactly one terminal.
                                if matches!(event, WsMessage::BacktestComplete { .. }) {
                                    backtest_terminal_relayed = true;
                                }

                                if let Some((symbol, stale_since)) =
                                    Self::maybe_clear_staleness(&event, &staleness)
                                {
                                    connection_manager
                                        .broadcast(WsMessage::data_fresh(
                                            platform,
                                            symbol,
                                            stale_since,
                                        ))
                                        .await;
                                }

                                if let WsMessage::QuoteData { ref quote, ref timestamp } = event
                                    && let Some(ref source) = price_source_snapshot {
                                        source.handle_quote(
                                            &quote.symbol,
                                            quote.last,
                                            u64::try_from(timestamp.timestamp_millis()).unwrap_or(0),
                                        );
                                    }

                                if let WsMessage::RateLimit { event: ref rate_event, .. } = event {
                                    metrics::counter!(
                                        "gateway_rate_limit_events_total",
                                        "platform" => platform.header_value(),
                                        "event_type" => rate_event.metric_label(),
                                    )
                                    .increment(1);
                                }

                                Self::route_event_through_router(
                                    &event,
                                    platform,
                                    &event_routers,
                                    &trading_adapters,
                                ).await;

                                let event = match event {
                                    WsMessage::Order { event: evt, mut order, parent_order_id, timestamp } => {
                                        order.correlation_id = correlation_store.get(&order.id);

                                        if matches!(evt,
                                            OrderEventType::OrderFilled
                                            | OrderEventType::OrderRejected
                                            | OrderEventType::OrderCancelled
                                            | OrderEventType::OrderExpired
                                        ) {
                                            correlation_store.remove(&order.id);
                                        }

                                        WsMessage::Order { event: evt, order, parent_order_id, timestamp }
                                    }
                                    other => other,
                                };

                                // Broadcast to connected strategies. Providers that
                                // filter upstream (e.g., Tektii backtest engine, which
                                // resolves SUBSCRIPTIONS at startup against its
                                // instrument catalog) bypass the registry's filter —
                                // re-filtering with weaker pattern semantics is the
                                // class of bug this guards against.
                                let should_broadcast = filters_events_upstream
                                    || subscription_filter.matches(&event, platform);
                                let delivered = if should_broadcast {
                                    let strategies = connected_strategies_clone.read().await;
                                    let mut any_delivered = false;
                                    for conn_id in strategies.iter() {
                                        match connection_manager
                                            .send_to(
                                                conn_id,
                                                event.clone(),
                                                engine_event_id.clone(),
                                            )
                                            .await
                                        {
                                            Ok(()) => any_delivered = true,
                                            Err(err) => warn!(
                                                platform = %platform,
                                                conn_id = %conn_id,
                                                error = %err,
                                                "Failed to send engine event to strategy connection"
                                            ),
                                        }
                                    }
                                    any_delivered
                                } else {
                                    // Registry filter declined to broadcast. Treat the
                                    // event as delivered so the engine cannot deadlock
                                    // waiting on an event the registry deliberately
                                    // dropped (deadlock avoidance). Moot for
                                    // tektii backtest mode (filters upstream); live and
                                    // other providers rely on this branch.
                                    debug!(
                                        platform = %platform,
                                        "Event filtered out by subscription"
                                    );
                                    true
                                };

                                // Only mark the engine event sent once it was actually
                                // handed to at least one strategy connection (or the
                                // registry filter declined it). Only `sent = true`
                                // events are releasable by a strategy ACK — a strategy
                                // cannot legitimately ack an event it never received,
                                // so a lost delivery stays pending-undelivered and the
                                // engine's own ACK timeout surfaces the loss loudly
                                // (one timed-out window; later events still release by
                                // their own ids).
                                if let Some(id) = engine_event_id {
                                    if delivered {
                                        let bridge_opt = {
                                            let guard = tektii_ack_bridge.read().await;
                                            guard.clone()
                                        };
                                        if let Some(bridge) = bridge_opt {
                                            bridge.mark_sent(vec![id]).await;
                                        }
                                    } else {
                                        warn!(
                                            platform = %platform,
                                            event_id = %id,
                                            "Engine event not delivered to any strategy \
                                             connection; leaving pending-undelivered \
                                             (not marking sent)"
                                        );
                                    }
                                }

                                if matches!(event, WsMessage::Order {
                                    event: OrderEventType::OrderFilled
                                        | OrderEventType::OrderPartiallyFilled
                                        | OrderEventType::OrderCancelled,
                                    ..
                                }) {
                                    let internal_event = InternalTradingEvent::new(event.clone(), platform);
                                    let _ = internal_tx.send(internal_event);

                                    if let WsMessage::Order { ref order, event: ref evt, .. } = event {
                                        debug!(
                                            platform = %platform,
                                            order_id = %order.id,
                                            event = ?evt,
                                            "Emitted order update to internal channel"
                                        );
                                    }
                                }
                            } else {
                                warn!(platform = %platform, "Platform stream closed");
                                break; // Stream closed — attempt reconnection
                            }
                        }
                    }
                }

                if !supports_reconnection {
                    if backtest_terminal_relayed {
                        // The provider already relayed a real, in-band
                        // `backtest_complete` terminal and its flush-ack
                        // round-tripped to the engine (tektii mode). This
                        // stream-close is the engine tearing down *after* the
                        // handshake — emit nothing further so the strategy sees
                        // exactly one terminal. The completion is still counted.
                        info!(
                            platform = %platform,
                            "Provider stream ended after in-band backtest_complete; exiting"
                        );
                        metrics::counter!(
                            "gateway_backtest_completions_total",
                            "platform" => platform.header_value(),
                        )
                        .increment(1);
                        break 'outer;
                    }

                    // No in-band terminal was relayed. Fall back to synthesizing
                    // a terminal from the bare stream-close.
                    //
                    // In tektii/backtest mode the engine ending its stream is
                    // treated as end-of-backtest, so forward a distinct
                    // `backtest_complete` terminal instead of `broker_disconnected`
                    // — this lets a connected strategy (e.g. the canary capture)
                    // finalize and flush its dataset rather than exit via a
                    // feed-loss path. Live providers keep emitting
                    // `broker_disconnected`.
                    //
                    // NOTE: on this fallback path the gateway cannot distinguish a
                    // clean completion from an aborted run — without an in-band
                    // terminal the engine read loop collapses a graceful Close, a
                    // transport error, and a bare end-of-stream into one signal, so
                    // a truncated run also surfaces as `backtest_complete`. A
                    // consumer that needs completeness guarantees must validate the
                    // dataset itself. (When the engine emits its in-band terminal,
                    // that positive signal is taken via the branch above instead.)
                    let msg = if end_of_stream_is_completion {
                        info!(
                            platform = %platform,
                            "Provider stream ended (end of backtest), notifying strategies and exiting"
                        );
                        metrics::counter!(
                            "gateway_backtest_completions_total",
                            "platform" => platform.header_value(),
                        )
                        .increment(1);
                        WsMessage::backtest_complete(platform)
                    } else {
                        info!(platform = %platform, "Provider does not support reconnection, exiting");
                        WsMessage::broker_disconnected(platform)
                    };
                    connection_manager.broadcast(msg).await;
                    break 'outer;
                }

                let disconnect_msg = WsMessage::broker_disconnected(platform);
                connection_manager.broadcast(disconnect_msg).await;
                metrics::counter!(
                    "gateway_broker_disconnections_total",
                    "platform" => platform.header_value(),
                )
                .increment(1);

                let symbols = {
                    let shared_guard = shared_providers_clone.read().await;
                    shared_guard
                        .get(&platform)
                        .map(|p| p.symbols.clone())
                        .unwrap_or_default()
                };
                staleness.mark_all_stale(&symbols);

                if !symbols.is_empty() {
                    let msg = WsMessage::data_stale(platform, symbols);
                    connection_manager.broadcast(msg).await;
                }

                let mut handler = ReconnectionHandler::new(reconnection_config.clone());
                handler.on_disconnect();

                let reconnected = loop {
                    let Some(delay) = handler.next_backoff() else {
                        handler.on_gave_up();
                        let msg = WsMessage::broker_connection_failed(platform);
                        connection_manager.broadcast(msg).await;
                        error!(
                            platform = %platform,
                            "Broker reconnection failed after max retry duration, giving up"
                        );
                        metrics::counter!(
                            "gateway_broker_reconnect_failures_total",
                            "platform" => platform.header_value(),
                        )
                        .increment(1);
                        break false;
                    };

                    metrics::counter!(
                        "gateway_broker_reconnect_attempts_total",
                        "platform" => platform.header_value(),
                    )
                    .increment(1);
                    info!(
                        platform = %platform,
                        attempt = handler.attempt_count(),
                        delay_ms = delay.as_millis(),
                        "Attempting broker reconnection"
                    );

                    tokio::select! {
                        () = cancel.cancelled() => {
                            info!(platform = %platform, "Shutdown during reconnection backoff");
                            break 'outer;
                        }
                        () = tokio::time::sleep(delay) => {}
                    }

                    let result = {
                        let shared_guard = shared_providers_clone.read().await;
                        if let Some(p) = shared_guard.get(&platform) {
                            Some(p.provider.reconnect().await)
                        } else {
                            None
                        }
                    };

                    match result {
                        Some(Ok(new_stream)) => {
                            let gap = handler.on_reconnect_success();
                            metrics::counter!(
                                "gateway_broker_reconnections_total",
                                "platform" => platform.header_value(),
                            )
                            .increment(1);
                            info!(
                                platform = %platform,
                                gap_ms = gap.as_millis(),
                                "Broker reconnected successfully"
                            );

                            stream = new_stream;

                            // Trigger reconciliation (catches fills during gap)
                            let router = {
                                let routers = event_routers.read().await;
                                routers.get(&platform).cloned()
                            };
                            if let Some(router) = router {
                                info!(platform = %platform, "Starting post-reconnect reconciliation");
                                router.reconcile_after_reconnect().await;
                            }

                            let msg = WsMessage::broker_reconnected(platform, gap);
                            connection_manager.broadcast(msg).await;

                            break true;
                        }
                        Some(Err(WebSocketError::PermanentAuthError(e))) => {
                            error!(
                                platform = %platform,
                                error = %e,
                                "Permanent auth error during reconnection, giving up"
                            );
                            let msg = WsMessage::broker_connection_failed(platform);
                            connection_manager.broadcast(msg).await;
                            break false;
                        }
                        Some(Err(e)) => {
                            warn!(
                                platform = %platform,
                                error = %e,
                                attempt = handler.attempt_count(),
                                "Reconnection attempt failed, will retry"
                            );
                        }
                        None => {
                            error!(platform = %platform, "Provider disappeared during reconnection");
                            break false;
                        }
                    }
                };

                if !reconnected {
                    break 'outer;
                }
            }

            debug!(platform = %platform, "Platform task completed");
        });

        let mut shared_guard = self.shared_providers.write().await;
        if let Some(p) = shared_guard.get_mut(&platform) {
            p.task = Some(task);
        } else {
            warn!(
                platform = %platform,
                "Platform removed before task handle could be stored, aborting task"
            );
            task.abort();
        }
    }

    /// Route an event through the `EventRouter` for state management.
    async fn route_event_through_router(
        event: &WsMessage,
        platform: TradingPlatform,
        event_routers: &Arc<RwLock<HashMap<TradingPlatform, Arc<EventRouter>>>>,
        _trading_adapters: &Arc<RwLock<HashMap<TradingPlatform, Arc<dyn TradingAdapter>>>>,
    ) {
        let router = {
            let routers = event_routers.read().await;
            routers.get(&platform).cloned()
        };

        let Some(router) = router else {
            return;
        };

        match event {
            WsMessage::Order {
                event: event_type,
                order,
                parent_order_id,
                ..
            } => {
                debug!(
                    platform = %platform,
                    order_id = %order.id,
                    event_type = ?event_type,
                    "Routing order event through EventRouter"
                );

                router
                    .handle_order_event(*event_type, order, parent_order_id.as_deref())
                    .await;
            }
            WsMessage::Position {
                event: event_type,
                position,
                ..
            } => {
                debug!(
                    platform = %platform,
                    symbol = %position.symbol,
                    event_type = ?event_type,
                    "Routing position event through EventRouter"
                );

                router.handle_position_event(*event_type, position).await;
            }
            _ => {}
        }
    }

    /// Clear staleness for an instrument when a market data event arrives.
    ///
    /// Returns the symbol and its `stale_since` time if this was the first
    /// fresh tick after reconnect, so the caller can broadcast a `DataFresh`
    /// event.
    fn maybe_clear_staleness<'a>(
        event: &'a WsMessage,
        staleness: &StalenessTracker,
    ) -> Option<(&'a str, DateTime<Utc>)> {
        let symbol = match event {
            WsMessage::Candle { bar, .. } => Some(bar.symbol.as_str()),
            WsMessage::QuoteData { quote, .. } => Some(quote.symbol.as_str()),
            WsMessage::Trade { trade, .. } => Some(trade.symbol.as_str()),
            _ => None,
        };
        symbol.and_then(|sym| {
            staleness.mark_fresh(sym).map(|stale_since| {
                info!(
                    symbol = sym,
                    "First fresh tick after reconnect — data no longer stale"
                );
                (sym, stale_since)
            })
        })
    }

    /// Disconnect a specific shared provider.
    pub async fn disconnect_shared_provider(
        &self,
        platform: TradingPlatform,
    ) -> Result<(), WebSocketError> {
        info!(platform = %platform, "Disconnecting shared provider");

        let mut shared_providers = self.shared_providers.write().await;

        if let Some(mut instance) = shared_providers.remove(&platform) {
            if let Some(task) = instance.task.take() {
                task.abort();
            }
            instance.provider.disconnect().await?;
            info!(platform = %platform, "Shared provider disconnected successfully");
            Ok(())
        } else {
            Err(WebSocketError::ConnectionNotFound(format!(
                "Shared provider for platform '{platform}' not found"
            )))
        }
    }

    /// Shutdown all providers.
    pub async fn shutdown(&self) {
        info!("Shutting down provider registry");

        self.cancellation_token.cancel();

        // Disconnect all shared providers
        {
            let mut shared_providers = self.shared_providers.write().await;
            for (platform, mut instance) in shared_providers.drain() {
                if let Some(task) = instance.task.take() {
                    task.abort();
                }
                if let Err(e) = instance.provider.disconnect().await {
                    error!(platform = %platform, error = %e, "Error disconnecting shared provider");
                } else {
                    info!(platform = %platform, "Shared provider disconnected");
                }
            }
        }

        // Clear connected strategies
        {
            let mut strategies = self.connected_strategies.write().await;
            strategies.clear();
        }

        info!("Provider registry shutdown complete");
    }
}

#[async_trait::async_trait]
impl crate::trailing_stop::QuoteSubscriber for ProviderRegistry {
    async fn ensure_subscribed(&self, symbol: &str) -> Result<(), crate::error::GatewayError> {
        self.ensure_quote_subscription(symbol).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry() -> Arc<ProviderRegistry> {
        Arc::new(ProviderRegistry::new(
            Arc::new(WsConnectionManager::new()),
            SubscriptionFilter::new(&[]),
            CancellationToken::new(),
            ReconnectionConfig::default(),
            Arc::new(CorrelationStore::new()),
        ))
    }

    #[tokio::test]
    async fn wait_for_strategy_blocks_until_the_strategy_connects() {
        let registry = test_registry();
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.wait_for_strategy().await })
        };

        // No strategy yet — the waiter must still be parked.
        assert!(!waiter.is_finished());

        registry.register_strategy_connection(Uuid::new_v4()).await;

        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter should resolve once the strategy connects")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn wait_for_strategy_returns_immediately_when_already_connected() {
        let registry = test_registry();
        registry.register_strategy_connection(Uuid::new_v4()).await;

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            registry.wait_for_strategy(),
        )
        .await
        .expect("should return immediately when the strategy is already connected");
    }
}
