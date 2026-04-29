//! Saxo Bank WebSocket streaming provider.
//!
//! Connects to Saxo's proprietary binary WebSocket streaming API and translates
//! events into the standard [`WsMessage`] format. Follows Saxo's subscription
//! model: create subscriptions via REST, receive delta updates over WebSocket.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::header::AUTHORIZATION;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::delta::SnapshotStore;
use super::error::SaxoError;
use super::http::SaxoHttpClient;
use super::instruments::SaxoInstrumentMap;
use super::streaming::parse_frame;
use super::types::{
    SaxoOrder, SaxoPortfolioSubscriptionArguments, SaxoPortfolioSubscriptionRequest,
    SaxoSubscriptionArguments, SaxoSubscriptionRequest, SaxoSubscriptionResponse,
};
use crate::adapter::SaxoAdapter;
use crate::credentials::SaxoCredentials;
use tektii_gateway_core::models::{Account, Order, Quote, TradingPlatform};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{
    AccountEventType, ConnectionEventType, EventAckMessage, InternalTradingEvent, OrderEventType,
    WsErrorCode, WsMessage,
};
use tektii_gateway_core::websocket::provider::{
    EventStream, ProviderConfig, ProviderEvent, WebSocketProvider,
};

// ============================================================================
// Constants
// ============================================================================

/// WebSocket URL for Saxo SIM environment.
const SAXO_SIM_WS_URL: &str = "wss://streaming.saxobank.com/sim/openapi/streamingws";
/// WebSocket URL for Saxo LIVE environment.
const SAXO_LIVE_WS_URL: &str = "wss://streaming.saxobank.com/openapi/streamingws";

/// Heartbeat timeout.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Fraction of token lifetime at which to trigger re-authorization.
const TOKEN_REAUTH_FRACTION: f64 = 0.75;

/// Default token lifetime in seconds.
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 1200;

/// Reference ID for the portfolio balance subscription.
const BALANCE_REFERENCE_ID: &str = "balance_account";

/// Reference ID for the portfolio order subscription.
const ORDER_REFERENCE_ID: &str = "orders_account";

/// Margin utilisation threshold for warning (80%).
const MARGIN_WARNING_THRESHOLD: f64 = 80.0;

/// Margin utilisation threshold for critical warning (90%).
const MARGIN_CRITICAL_THRESHOLD: f64 = 90.0;

/// Margin utilisation threshold for margin call (100%).
const MARGIN_CALL_THRESHOLD: f64 = 100.0;

/// Debounce period.
const MARGIN_DEBOUNCE: Duration = Duration::from_secs(60);

// ============================================================================
// Internal types
// ============================================================================

struct SubscriptionEntry {
    symbol: String,
    #[allow(dead_code)]
    uic: u32,
    #[allow(dead_code)]
    asset_type: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct HeartbeatEntry {
    reference_id: String,
    reason: String,
}

struct MarginMonitor {
    warning_80: Option<tokio::time::Instant>,
    warning_90: Option<tokio::time::Instant>,
    margin_call: Option<tokio::time::Instant>,
}

impl MarginMonitor {
    fn new() -> Self {
        Self {
            warning_80: None,
            warning_90: None,
            margin_call: None,
        }
    }

    fn check_utilisation(&mut self, utilisation_pct: f64) -> Vec<(AccountEventType, String)> {
        let now = tokio::time::Instant::now();
        let mut events = Vec::new();

        if utilisation_pct >= MARGIN_CALL_THRESHOLD {
            if Self::should_emit(self.margin_call, now) {
                self.margin_call = Some(now);
                events.push((
                    AccountEventType::MarginCall,
                    format!(
                        "Margin call: utilisation at {utilisation_pct:.1}% (>= {MARGIN_CALL_THRESHOLD}%)"
                    ),
                ));
            }
        } else if utilisation_pct >= MARGIN_CRITICAL_THRESHOLD {
            if Self::should_emit(self.warning_90, now) {
                self.warning_90 = Some(now);
                events.push((
                    AccountEventType::MarginWarning,
                    format!(
                        "Critical margin warning: utilisation at {utilisation_pct:.1}% (>= {MARGIN_CRITICAL_THRESHOLD}%)"
                    ),
                ));
            }
        } else if utilisation_pct >= MARGIN_WARNING_THRESHOLD
            && Self::should_emit(self.warning_80, now)
        {
            self.warning_80 = Some(now);
            events.push((
                AccountEventType::MarginWarning,
                format!(
                    "Margin warning: utilisation at {utilisation_pct:.1}% (>= {MARGIN_WARNING_THRESHOLD}%)"
                ),
            ));
        }

        events
    }

    fn should_emit(last_emitted: Option<tokio::time::Instant>, now: tokio::time::Instant) -> bool {
        match last_emitted {
            Some(last) => now.duration_since(last) >= MARGIN_DEBOUNCE,
            None => true,
        }
    }
}

// ============================================================================
// Provider
// ============================================================================

/// Saxo Bank WebSocket streaming provider implementing [`WebSocketProvider`].
pub struct SaxoWebSocketProvider {
    platform: TradingPlatform,
    snapshot_store: Arc<SnapshotStore>,
    subscriptions: Arc<Mutex<HashMap<String, SubscriptionEntry>>>,
    cancel_token: CancellationToken,
    context_id: Arc<Mutex<Option<String>>>,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    http_client: Arc<SaxoHttpClient>,
    instrument_map: Arc<RwLock<Option<SaxoInstrumentMap>>>,
    ws_url: String,
    account_key: String,
    stored_config: Arc<RwLock<Option<ProviderConfig>>>,
}

impl SaxoWebSocketProvider {
    /// Create a new Saxo WebSocket provider from credentials.
    pub fn new(
        credentials: &SaxoCredentials,
        platform: TradingPlatform,
    ) -> Result<Self, SaxoError> {
        let http_client = SaxoHttpClient::new(credentials, platform)?;

        let ws_url = credentials
            .ws_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::SaxoLive => SAXO_LIVE_WS_URL.to_string(),
                _ => SAXO_SIM_WS_URL.to_string(),
            });

        Ok(Self {
            platform,
            snapshot_store: Arc::new(SnapshotStore::new()),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            cancel_token: CancellationToken::new(),
            context_id: Arc::new(Mutex::new(None)),
            event_tx: Arc::new(RwLock::new(None)),
            http_client: Arc::new(http_client),
            instrument_map: Arc::new(RwLock::new(None)),
            ws_url,
            account_key: credentials.account_key.clone(),
            stored_config: Arc::new(RwLock::new(None)),
        })
    }

    // ========================================================================
    // Subscription management
    // ========================================================================

    async fn create_price_subscription(
        &self,
        context_id: &str,
        reference_id: &str,
        uic: u32,
        asset_type: &str,
    ) -> Result<SaxoSubscriptionResponse, SaxoError> {
        let request = SaxoSubscriptionRequest {
            arguments: SaxoSubscriptionArguments {
                uic,
                asset_type: asset_type.to_string(),
            },
            context_id: context_id.to_string(),
            reference_id: reference_id.to_string(),
            refresh_rate: 0,
        };

        let response: SaxoSubscriptionResponse = self
            .http_client
            .post("/trade/v1/infoprices/subscriptions", &request)
            .await
            .map_err(|e| SaxoError::SubscriptionFailed {
                reference_id: reference_id.to_string(),
                message: e.to_string(),
            })?;

        Ok(response)
    }

    async fn delete_subscription(
        &self,
        context_id: &str,
        reference_id: &str,
    ) -> Result<(), SaxoError> {
        let path = format!("/trade/v1/infoprices/subscriptions/{context_id}/{reference_id}");
        self.http_client.delete(&path).await?;
        Ok(())
    }

    async fn subscribe_symbol(&self, symbol: &str, context_id: &str) -> Result<(), SaxoError> {
        let map_guard = self.instrument_map.read().await;
        let instrument_map = map_guard
            .as_ref()
            .ok_or_else(|| SaxoError::Config("Instrument map not loaded".to_string()))?;

        let info = instrument_map.resolve_symbol(symbol)?;
        let reference_id = reference_id_for_symbol(symbol);

        {
            let subs = self.subscriptions.lock().await;
            if subs.contains_key(&reference_id) {
                debug!(symbol = %symbol, "Already subscribed, skipping");
                return Ok(());
            }
        }

        let uic = info.uic;
        let asset_type = info.asset_type.clone();

        let response = self
            .create_price_subscription(context_id, &reference_id, uic, &asset_type)
            .await?;

        self.snapshot_store
            .set_snapshot(&reference_id, response.snapshot);

        {
            let mut subs = self.subscriptions.lock().await;
            subs.insert(
                reference_id.clone(),
                SubscriptionEntry {
                    symbol: symbol.to_string(),
                    uic,
                    asset_type,
                },
            );
        }

        debug!(
            symbol = %symbol,
            reference_id = %reference_id,
            uic = uic,
            "Saxo price subscription created"
        );

        Ok(())
    }

    async fn unsubscribe_symbol(&self, symbol: &str, context_id: &str) -> Result<(), SaxoError> {
        let reference_id = reference_id_for_symbol(symbol);

        let had_entry = {
            let mut subs = self.subscriptions.lock().await;
            subs.remove(&reference_id).is_some()
        };

        if !had_entry {
            debug!(symbol = %symbol, "Not subscribed, skipping unsubscribe");
            return Ok(());
        }

        self.snapshot_store.remove(&reference_id);

        if let Err(e) = self.delete_subscription(context_id, &reference_id).await {
            warn!(
                symbol = %symbol,
                error = %e,
                "Failed to delete Saxo subscription (will be cleaned up on disconnect)"
            );
        }

        debug!(symbol = %symbol, "Saxo price subscription removed");
        Ok(())
    }

    // ========================================================================
    // Portfolio subscriptions
    // ========================================================================

    async fn subscribe_balance(&self, context_id: &str) -> Result<(), SaxoError> {
        let request = SaxoPortfolioSubscriptionRequest {
            arguments: SaxoPortfolioSubscriptionArguments {
                account_key: self.account_key.clone(),
            },
            context_id: context_id.to_string(),
            reference_id: BALANCE_REFERENCE_ID.to_string(),
            refresh_rate: 1000,
        };

        let response: SaxoSubscriptionResponse = self
            .http_client
            .post("/port/v1/balances/subscriptions", &request)
            .await
            .map_err(|e| SaxoError::SubscriptionFailed {
                reference_id: BALANCE_REFERENCE_ID.to_string(),
                message: e.to_string(),
            })?;

        self.snapshot_store
            .set_snapshot(BALANCE_REFERENCE_ID, response.snapshot);

        debug!("Saxo balance subscription created");
        Ok(())
    }

    async fn subscribe_orders(&self, context_id: &str) -> Result<(), SaxoError> {
        let request = SaxoPortfolioSubscriptionRequest {
            arguments: SaxoPortfolioSubscriptionArguments {
                account_key: self.account_key.clone(),
            },
            context_id: context_id.to_string(),
            reference_id: ORDER_REFERENCE_ID.to_string(),
            refresh_rate: 0,
        };

        let response: SaxoSubscriptionResponse = self
            .http_client
            .post("/port/v1/orders/subscriptions", &request)
            .await
            .map_err(|e| SaxoError::SubscriptionFailed {
                reference_id: ORDER_REFERENCE_ID.to_string(),
                message: e.to_string(),
            })?;

        self.snapshot_store
            .set_snapshot(ORDER_REFERENCE_ID, response.snapshot);

        debug!("Saxo order subscription created");
        Ok(())
    }

    /// Create all initial price and portfolio subscriptions.
    async fn create_initial_subscriptions(&self, symbols: &[String], context_id: &str) {
        for symbol in symbols {
            if let Err(e) = self.subscribe_symbol(symbol, context_id).await {
                warn!(
                    symbol = %symbol,
                    error = %e,
                    "Failed to create Saxo subscription, skipping"
                );
            }
        }

        info!(
            platform = %self.platform,
            subscriptions = self.snapshot_store.len(),
            "Saxo price subscriptions created"
        );

        if let Err(e) = self.subscribe_balance(context_id).await {
            warn!(
                error = %e,
                "Failed to create Saxo balance subscription — margin monitoring disabled"
            );
        }
        if let Err(e) = self.subscribe_orders(context_id).await {
            warn!(
                error = %e,
                "Failed to create Saxo order subscription — liquidation detection disabled"
            );
        }
    }

    /// Start the internal trading stream for order events.
    ///
    /// Saxo order events flow through the main WebSocket stream via the order
    /// subscription (`/port/v1/orders/subscriptions`). The registry automatically
    /// forwards `WsMessage::Order` events to the internal broadcast channel,
    /// so a separate internal stream is not needed.
    pub fn connect_internal_trading_stream(
        &self,
        _internal_tx: tokio::sync::broadcast::Sender<InternalTradingEvent>,
        _cancel_token: CancellationToken,
        platform: TradingPlatform,
    ) -> Result<(), WebSocketError> {
        debug!(
            platform = %platform,
            "Saxo order events flow through main stream — no separate internal stream needed"
        );
        Ok(())
    }
}

// ============================================================================
// WebSocketProvider trait implementation
// ============================================================================

#[async_trait]
impl WebSocketProvider for SaxoWebSocketProvider {
    async fn connect(&self, config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        *self.stored_config.write().await = Some(config.clone());

        let context_id = uuid::Uuid::new_v4().to_string();
        {
            let mut ctx = self.context_id.lock().await;
            *ctx = Some(context_id.clone());
        }

        // Load instrument map
        let instrument_map = SaxoInstrumentMap::load(&self.http_client, &["FxSpot"])
            .await
            .map_err(|e| {
                WebSocketError::ConfigError(format!("Failed to load Saxo instruments: {e}"))
            })?;
        info!(count = instrument_map.len(), "Loaded Saxo instrument map");
        {
            let mut map = self.instrument_map.write().await;
            *map = Some(instrument_map);
        }

        // Get access token for WebSocket connection
        let token =
            self.http_client.auth().access_token().await.map_err(|e| {
                WebSocketError::ConfigError(format!("Failed to get Saxo token: {e}"))
            })?;

        // Open WebSocket connection
        let ws_connect_url = format!("{}/connect?contextId={}", self.ws_url, context_id);
        let request = Request::builder()
            .uri(&ws_connect_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Host", extract_host(&ws_connect_url))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .map_err(|e| {
                WebSocketError::ConnectionFailed(format!("Failed to build WS request: {e}"))
            })?;

        let (ws_stream, _response) = connect_async(request).await.map_err(|e| {
            WebSocketError::ConnectionFailed(format!("Saxo WS connect failed: {e}"))
        })?;

        info!(
            platform = %self.platform,
            context_id = %context_id,
            "Saxo WebSocket connected"
        );

        // Create event channel
        let (tx, rx) = mpsc::unbounded_channel();
        *self.event_tx.write().await = Some(tx);

        self.create_initial_subscriptions(&config.symbols, &context_id)
            .await;

        // Split WebSocket for reader task
        let (ws_write, ws_read) = ws_stream.split();

        // Spawn binary frame reader task
        let reader_cancel = self.cancel_token.clone();
        let reader_store = self.snapshot_store.clone();
        let reader_subs = self.subscriptions.clone();
        let reader_event_tx = self.event_tx.clone();
        let reader_platform = self.platform;
        let reader_instrument_map = self.instrument_map.clone();

        tokio::spawn(async move {
            run_frame_reader(
                ws_read,
                FrameReaderContext {
                    snapshot_store: reader_store,
                    subscriptions: reader_subs,
                    event_tx: reader_event_tx,
                    platform: reader_platform,
                    cancel_token: reader_cancel,
                    heartbeat_timeout: HEARTBEAT_TIMEOUT,
                    instrument_map: reader_instrument_map,
                },
            )
            .await;
        });

        // Spawn token re-authorization task
        let reauth_cancel = self.cancel_token.clone();
        let reauth_http = self.http_client.clone();
        let reauth_context_id = context_id.clone();
        let reauth_ws_url = self.ws_url.clone();
        let reauth_platform = self.platform;

        let reauth_interval =
            Duration::from_secs(DEFAULT_TOKEN_LIFETIME_SECS).mul_f64(TOKEN_REAUTH_FRACTION);

        tokio::spawn(async move {
            run_token_reauth(
                reauth_http,
                reauth_context_id,
                reauth_ws_url,
                reauth_platform,
                reauth_cancel,
                reauth_interval,
            )
            .await;
        });

        drop(ws_write);

        info!(
            platform = %self.platform,
            "Saxo streaming fully initialized"
        );

        Ok(rx)
    }

    async fn subscribe(
        &self,
        symbols: Vec<String>,
        _event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        let context_id = {
            let ctx = self.context_id.lock().await;
            ctx.clone().ok_or_else(|| {
                WebSocketError::ConfigError("Not connected — call connect() first".to_string())
            })?
        };

        for symbol in &symbols {
            self.subscribe_symbol(symbol, &context_id)
                .await
                .map_err(|e| WebSocketError::ProviderError(e.to_string()))?;
        }

        debug!(
            platform = %self.platform,
            ?symbols,
            "Saxo subscribe complete"
        );
        Ok(())
    }

    async fn unsubscribe(&self, symbols: Vec<String>) -> Result<(), WebSocketError> {
        let context_id = {
            let ctx = self.context_id.lock().await;
            ctx.clone().ok_or_else(|| {
                WebSocketError::ConfigError("Not connected — call connect() first".to_string())
            })?
        };

        for symbol in &symbols {
            self.unsubscribe_symbol(symbol, &context_id)
                .await
                .map_err(|e| WebSocketError::ProviderError(e.to_string()))?;
        }

        debug!(
            platform = %self.platform,
            ?symbols,
            "Saxo unsubscribe complete"
        );
        Ok(())
    }

    async fn handle_ack(&self, _ack: EventAckMessage) -> Result<(), WebSocketError> {
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), WebSocketError> {
        self.cancel_token.cancel();
        info!(
            platform = %self.platform,
            "Saxo streaming disconnected"
        );
        Ok(())
    }

    async fn reconnect(&self) -> Result<EventStream, WebSocketError> {
        let config = self.stored_config.read().await.clone().ok_or_else(|| {
            WebSocketError::ConnectionClosed("Cannot reconnect: no stored config".to_string())
        })?;

        self.connect(config).await
    }
}

// ============================================================================
// Binary frame reader task
// ============================================================================

struct FrameReaderContext {
    snapshot_store: Arc<SnapshotStore>,
    subscriptions: Arc<Mutex<HashMap<String, SubscriptionEntry>>>,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    platform: TradingPlatform,
    cancel_token: CancellationToken,
    heartbeat_timeout: Duration,
    instrument_map: Arc<RwLock<Option<SaxoInstrumentMap>>>,
}

async fn run_frame_reader(
    mut ws_read: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ctx: FrameReaderContext,
) {
    let mut heartbeat_deadline = tokio::time::Instant::now() + ctx.heartbeat_timeout;
    let mut margin_monitor = MarginMonitor::new();
    let mut disconnect_error: Option<String> = None;

    loop {
        tokio::select! {
            () = ctx.cancel_token.cancelled() => {
                info!(platform = %ctx.platform, "Saxo frame reader cancelled");
                break;
            }
            () = tokio::time::sleep_until(heartbeat_deadline) => {
                warn!(platform = %ctx.platform, "Saxo heartbeat timeout — connection may be dead");
                disconnect_error = Some("Heartbeat timeout — connection may be dead".to_string());
                break;
            }
            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        heartbeat_deadline = tokio::time::Instant::now() + ctx.heartbeat_timeout;

                        match parse_frame(&data) {
                            Ok(messages) => {
                                for message in messages {
                                    process_message(
                                        &message,
                                        &ctx.snapshot_store,
                                        &ctx.subscriptions,
                                        &ctx.event_tx,
                                        ctx.platform,
                                        &mut margin_monitor,
                                        &ctx.instrument_map,
                                    )
                                    .await;
                                }
                            }
                            Err(e) => {
                                warn!(
                                    platform = %ctx.platform,
                                    error = %e,
                                    "Failed to parse Saxo frame, skipping"
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!(platform = %ctx.platform, "Saxo WebSocket closed");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        error!(
                            platform = %ctx.platform,
                            error = %e,
                            "Saxo WebSocket read error"
                        );
                        disconnect_error = Some(format!("WebSocket read error: {e}"));
                        break;
                    }
                }
            }
        }
    }

    // Signal downstream consumers
    {
        let tx_guard = ctx.event_tx.read().await;
        if let Some(tx) = tx_guard.as_ref() {
            let msg = WsMessage::Connection {
                event: ConnectionEventType::Disconnecting,
                error: disconnect_error,
                broker: None,
                gap_duration_ms: None,
                timestamp: Utc::now(),
            };
            if tx.send(msg.into()).is_err() {
                debug!(platform = %ctx.platform, "Disconnect signal not sent (receiver already dropped)");
            }
        }
    }

    *ctx.event_tx.write().await = None;
    ctx.cancel_token.cancel();
}

// ============================================================================
// Message processing
// ============================================================================

async fn process_message(
    message: &super::streaming::SaxoMessage,
    snapshot_store: &SnapshotStore,
    subscriptions: &Mutex<HashMap<String, SubscriptionEntry>>,
    event_tx: &RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>,
    platform: TradingPlatform,
    margin_monitor: &mut MarginMonitor,
    instrument_map: &RwLock<Option<SaxoInstrumentMap>>,
) {
    if message.is_control() {
        handle_control_message(message, snapshot_store, platform);
        return;
    }

    let payload: serde_json::Value = match message.deserialize_json() {
        Ok(v) => v,
        Err(e) => {
            warn!(
                reference_id = %message.reference_id,
                error = %e,
                "Failed to deserialize Saxo data payload"
            );
            return;
        }
    };

    match message.reference_id.as_str() {
        BALANCE_REFERENCE_ID => {
            process_balance_update(&payload, snapshot_store, event_tx, platform, margin_monitor)
                .await;
        }
        ORDER_REFERENCE_ID => {
            process_order_update(&payload, snapshot_store, event_tx, platform, instrument_map)
                .await;
        }
        _ => {
            process_price_update(
                message,
                &payload,
                snapshot_store,
                subscriptions,
                event_tx,
                platform,
            )
            .await;
        }
    }
}

async fn process_price_update(
    message: &super::streaming::SaxoMessage,
    payload: &serde_json::Value,
    snapshot_store: &SnapshotStore,
    subscriptions: &Mutex<HashMap<String, SubscriptionEntry>>,
    event_tx: &RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>,
    platform: TradingPlatform,
) {
    let symbol = {
        let subs = subscriptions.lock().await;
        if let Some(entry) = subs.get(&message.reference_id) {
            entry.symbol.clone()
        } else {
            debug!(
                reference_id = %message.reference_id,
                "Unknown reference ID, skipping"
            );
            return;
        }
    };

    let merged = match snapshot_store.apply_delta(&message.reference_id, payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                reference_id = %message.reference_id,
                error = %e,
                "Failed to apply Saxo delta"
            );
            return;
        }
    };

    if let Some(quote) = snapshot_to_quote(&merged, &symbol) {
        let ws_msg = WsMessage::quote(quote);
        let tx_guard = event_tx.read().await;
        if let Some(tx) = tx_guard.as_ref()
            && tx.send(ws_msg.into()).is_err()
        {
            debug!(platform = %platform, "Event channel closed");
        }
    }
}

/// Build an [`Account`] from a merged Saxo balance snapshot.
fn build_account_from_snapshot(merged: &serde_json::Value) -> Account {
    /// Extract an f64 field from a JSON value and convert to Decimal.
    fn decimal_field(value: &serde_json::Value, key: &str) -> Decimal {
        Decimal::from_f64(
            value
                .get(key)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
        )
        .unwrap_or_default()
    }

    Account {
        balance: decimal_field(merged, "CashBalance"),
        equity: decimal_field(merged, "TotalValue"),
        margin_used: decimal_field(merged, "MarginUsedByCurrentPositions"),
        margin_available: decimal_field(merged, "MarginAvailable"),
        unrealized_pnl: decimal_field(merged, "UnrealizedPositionsValue"),
        currency: merged
            .get("Currency")
            .and_then(|v| v.as_str())
            .unwrap_or("USD")
            .to_string(),
    }
}

async fn process_balance_update(
    payload: &serde_json::Value,
    snapshot_store: &SnapshotStore,
    event_tx: &RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>,
    platform: TradingPlatform,
    margin_monitor: &mut MarginMonitor,
) {
    let merged = match snapshot_store.apply_delta(BALANCE_REFERENCE_ID, payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "Failed to apply Saxo balance delta");
            return;
        }
    };

    let utilisation_pct = if let Some(pct) = merged
        .get("MarginUtilizationPct")
        .and_then(serde_json::Value::as_f64)
    {
        pct
    } else {
        let margin_used = merged
            .get("MarginUsedByCurrentPositions")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let margin_available = merged
            .get("MarginAvailable")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let total = margin_used + margin_available;
        if total > 0.0 {
            (margin_used / total) * 100.0
        } else {
            0.0
        }
    };

    let events = margin_monitor.check_utilisation(utilisation_pct);
    if events.is_empty() {
        return;
    }

    let account = build_account_from_snapshot(&merged);

    let tx_guard = event_tx.read().await;
    if let Some(tx) = tx_guard.as_ref() {
        for (event_type, msg) in events {
            match event_type {
                AccountEventType::MarginCall => {
                    error!(platform = %platform, %msg);
                }
                AccountEventType::MarginWarning if utilisation_pct >= MARGIN_CRITICAL_THRESHOLD => {
                    error!(platform = %platform, %msg);
                }
                _ => {
                    warn!(platform = %platform, %msg);
                }
            }

            let ws_msg = WsMessage::Account {
                event: event_type,
                account: account.clone(),
                timestamp: Utc::now(),
            };
            if tx.send(ws_msg.into()).is_err() {
                debug!(platform = %platform, "Event channel closed");
                break;
            }
        }
    }
}

// ============================================================================
// Order event translation
// ============================================================================

/// Determine the `OrderEventType` from a Saxo order status string and fill amount.
///
/// Returns `None` for transient statuses that should not generate events.
fn saxo_status_to_event_type(status: &str, filled_amount: f64) -> Option<OrderEventType> {
    match status {
        "Working" => {
            if filled_amount > 0.0 {
                Some(OrderEventType::OrderPartiallyFilled)
            } else {
                Some(OrderEventType::OrderCreated)
            }
        }
        "Filled" | "FinalFill" => Some(OrderEventType::OrderFilled),
        "PartiallyFilled" => Some(OrderEventType::OrderPartiallyFilled),
        "Cancelled" | "Canceled" => Some(OrderEventType::OrderCancelled),
        "Rejected" => Some(OrderEventType::OrderRejected),
        "Expired" => Some(OrderEventType::OrderExpired),
        // Transient statuses — no event
        "LockedPlacementPending" | "Placed" | "NotWorking" => None,
        other => {
            debug!(status = %other, "Unknown Saxo order status, skipping event");
            None
        }
    }
}

/// Translate a merged Saxo order snapshot (JSON) into a gateway [`Order`].
///
/// Returns `None` if the snapshot cannot be fully translated (e.g. partial delta,
/// unknown UIC, or missing required fields). This is intentionally graceful —
/// not every delta contains enough data for a full order.
fn translate_order_from_snapshot(
    merged: &serde_json::Value,
    instrument_map: &SaxoInstrumentMap,
) -> Option<Order> {
    let saxo_order: SaxoOrder = match serde_json::from_value(merged.clone()) {
        Ok(o) => o,
        Err(e) => {
            debug!(error = %e, "Failed to deserialize Saxo order snapshot, skipping");
            return None;
        }
    };

    let Ok(resolved_symbol) = instrument_map.resolve_uic(saxo_order.uic, &saxo_order.asset_type)
    else {
        debug!(
            uic = saxo_order.uic,
            asset_type = %saxo_order.asset_type,
            "Unknown UIC in order update, skipping"
        );
        return None;
    };
    let symbol = resolved_symbol.to_string();

    let side = SaxoAdapter::from_saxo_buy_sell(&saxo_order.buy_sell).ok()?;
    let order_type = SaxoAdapter::from_saxo_order_type(&saxo_order.order_type).ok()?;
    let status = SaxoAdapter::from_saxo_order_status(&saxo_order.status).ok()?;

    let tif = saxo_order
        .duration
        .as_ref()
        .and_then(|d| SaxoAdapter::from_saxo_duration(d).ok())
        .unwrap_or(tektii_gateway_core::models::TimeInForce::Gtc);

    let quantity = Decimal::from_f64(saxo_order.amount).unwrap_or_default();
    let filled_quantity = Decimal::from_f64(saxo_order.filled_amount).unwrap_or_default();
    let remaining = quantity - filled_quantity;

    let (limit_price, stop_price) = match order_type {
        tektii_gateway_core::models::OrderType::Limit => {
            (saxo_order.price.and_then(Decimal::from_f64), None)
        }
        tektii_gateway_core::models::OrderType::Stop => {
            (None, saxo_order.price.and_then(Decimal::from_f64))
        }
        tektii_gateway_core::models::OrderType::StopLimit => (
            saxo_order.stop_price.and_then(Decimal::from_f64),
            saxo_order.price.and_then(Decimal::from_f64),
        ),
        _ => (None, None),
    };

    let trailing_distance = saxo_order
        .trailing_stop_distance_to_market
        .and_then(Decimal::from_f64);

    let now = Utc::now();

    Some(Order {
        id: saxo_order.order_id,
        client_order_id: saxo_order.external_reference,
        symbol,
        side,
        order_type,
        time_in_force: tif,
        quantity,
        filled_quantity,
        remaining_quantity: remaining,
        limit_price,
        stop_price,
        average_fill_price: saxo_order.average_fill_price.and_then(Decimal::from_f64),
        status,
        stop_loss: None,
        take_profit: None,
        trailing_distance,
        trailing_type: None,
        reject_reason: None,
        position_id: None,
        reduce_only: None,
        post_only: None,
        hidden: None,
        display_quantity: None,
        oco_group_id: None,
        correlation_id: None,
        created_at: now,
        updated_at: now,
    })
}

async fn process_order_update(
    payload: &serde_json::Value,
    snapshot_store: &SnapshotStore,
    event_tx: &RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>,
    platform: TradingPlatform,
    instrument_map: &RwLock<Option<SaxoInstrumentMap>>,
) {
    let merged = match snapshot_store.apply_delta(ORDER_REFERENCE_ID, payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "Failed to apply Saxo order delta");
            return;
        }
    };

    if is_forced_liquidation(&merged) {
        let order_id = merged
            .get("OrderId")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let uic = merged
            .get("Uic")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        error!(
            platform = %platform,
            order_id = %order_id,
            uic = uic,
            "Forced liquidation detected — broker-initiated order filled"
        );

        let ws_msg = WsMessage::error_with_details(
            WsErrorCode::Liquidation,
            format!("Forced liquidation detected: order {order_id} (UIC {uic})"),
            serde_json::json!({
                "order_id": order_id,
                "uic": uic,
                "payload": merged,
            }),
        );

        let tx_guard = event_tx.read().await;
        if let Some(tx) = tx_guard.as_ref()
            && tx.send(ws_msg.into()).is_err()
        {
            debug!(platform = %platform, "Event channel closed");
        }
    }

    // Emit WsMessage::Order for normal order lifecycle events
    let map_guard = instrument_map.read().await;
    let Some(map) = map_guard.as_ref() else {
        return; // Instruments not loaded yet
    };

    let status_str = merged
        .get("Status")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let filled_amount = merged
        .get("FilledAmount")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);

    let Some(event_type) = saxo_status_to_event_type(status_str, filled_amount) else {
        return; // Transient or unknown status
    };

    let Some(order) = translate_order_from_snapshot(&merged, map) else {
        return; // Translation failed (partial delta, unknown UIC, etc.)
    };

    let order_id = order.id.clone();
    let ws_msg = WsMessage::Order {
        event: event_type,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    };

    let tx_guard = event_tx.read().await;
    if let Some(tx) = tx_guard.as_ref() {
        if tx.send(ws_msg.into()).is_err() {
            debug!(platform = %platform, "Event channel closed");
        } else {
            debug!(
                platform = %platform,
                order_id = %order_id,
                event = ?event_type,
                "Emitted Saxo order event"
            );
        }
    }
}

fn is_forced_liquidation(order: &serde_json::Value) -> bool {
    let has_no_external_ref = order
        .get("ExternalReference")
        .is_none_or(|v| v.is_null() || v.as_str().is_some_and(str::is_empty));
    let is_filled = order
        .get("Status")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == "Filled" || s == "FinalFill");

    has_no_external_ref && is_filled
}

fn handle_control_message(
    message: &super::streaming::SaxoMessage,
    snapshot_store: &SnapshotStore,
    platform: TradingPlatform,
) {
    match message.reference_id.as_str() {
        "_heartbeat" => {
            let entries: Vec<HeartbeatEntry> = match message.deserialize_json() {
                Ok(v) => v,
                Err(e) => {
                    debug!(error = %e, "Failed to parse heartbeat payload");
                    return;
                }
            };

            for entry in &entries {
                match entry.reason.as_str() {
                    "NoNewData" => {}
                    "SubscriptionTemporarilyDisabled" => {
                        warn!(
                            platform = %platform,
                            reference_id = %entry.reference_id,
                            "Saxo subscription temporarily disabled by server"
                        );
                    }
                    other => {
                        debug!(
                            platform = %platform,
                            reference_id = %entry.reference_id,
                            reason = %other,
                            "Saxo heartbeat"
                        );
                    }
                }
            }
        }
        "_resetsubscriptions" => {
            warn!(
                platform = %platform,
                "Saxo server requested subscription reset — clearing snapshots"
            );
            // The snapshot store will be rebuilt when subscriptions are re-created
            let _ = snapshot_store;
        }
        "_disconnect" => {
            warn!(
                platform = %platform,
                "Saxo server requested disconnect"
            );
        }
        other => {
            debug!(
                platform = %platform,
                control_id = %other,
                "Unknown Saxo control message"
            );
        }
    }
}

// ============================================================================
// Token re-authorization task
// ============================================================================

async fn run_token_reauth(
    http_client: Arc<SaxoHttpClient>,
    context_id: String,
    ws_url: String,
    platform: TradingPlatform,
    cancel_token: CancellationToken,
    interval: Duration,
) {
    loop {
        tokio::select! {
            () = cancel_token.cancelled() => {
                debug!(platform = %platform, "Token re-auth task cancelled");
                break;
            }
            () = tokio::time::sleep(interval) => {
                match http_client.auth().access_token().await {
                    Ok(token) => {
                        let url = format!(
                            "{ws_url}/authorize?contextId={context_id}"
                        );
                        let result = reqwest::Client::new()
                            .put(&url)
                            .header(AUTHORIZATION, format!("Bearer {token}"))
                            .send()
                            .await;

                        match result {
                            Ok(resp) if resp.status().is_success() => {
                                debug!(platform = %platform, "Saxo streaming token re-authorized");
                            }
                            Ok(resp) => {
                                warn!(
                                    platform = %platform,
                                    status = %resp.status(),
                                    "Saxo streaming re-auth failed"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    platform = %platform,
                                    error = %e,
                                    "Saxo streaming re-auth HTTP error"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            platform = %platform,
                            error = %e,
                            "Failed to get token for re-auth"
                        );
                    }
                }
            }
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Generate a reference ID from a symbol (e.g., "EURUSD:FxSpot" -> "price_eurusd_fxspot").
fn reference_id_for_symbol(symbol: &str) -> String {
    format!("price_{}", symbol.to_lowercase().replace(':', "_"))
}

/// Extract host from a URL for the WebSocket Host header.
fn extract_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("streaming.saxobank.com")
        .to_string()
}

/// Convert a merged Saxo price snapshot to a `Quote`.
fn snapshot_to_quote(snapshot: &serde_json::Value, symbol: &str) -> Option<Quote> {
    let quote = snapshot.get("Quote")?;
    let bid = quote.get("Bid").and_then(serde_json::Value::as_f64)?;
    let ask = quote.get("Ask").and_then(serde_json::Value::as_f64)?;
    let mid = quote
        .get("Mid")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or((bid + ask) / 2.0);

    Some(Quote {
        symbol: symbol.to_string(),
        provider: "saxo".to_string(),
        bid: Decimal::from_f64(bid).unwrap_or_default(),
        bid_size: None,
        ask: Decimal::from_f64(ask).unwrap_or_default(),
        ask_size: None,
        last: Decimal::from_f64(mid).unwrap_or_default(),
        volume: None,
        timestamp: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_id_generation() {
        assert_eq!(
            reference_id_for_symbol("EURUSD:FxSpot"),
            "price_eurusd_fxspot"
        );
        assert_eq!(
            reference_id_for_symbol("US500:CfdOnIndex"),
            "price_us500_cfdonindex"
        );
    }

    #[test]
    fn extract_host_from_wss_url() {
        assert_eq!(
            extract_host("wss://streaming.saxobank.com/sim/openapi/streamingws"),
            "streaming.saxobank.com"
        );
    }

    #[test]
    fn snapshot_to_quote_extracts_bid_ask_mid() {
        let snapshot = serde_json::json!({
            "Quote": {
                "Bid": 1.08500,
                "Ask": 1.08520,
                "Mid": 1.08510
            }
        });
        let quote = snapshot_to_quote(&snapshot, "EURUSD:FxSpot").unwrap();
        assert_eq!(quote.symbol, "EURUSD:FxSpot");
        assert_eq!(quote.provider, "saxo");
    }

    #[test]
    fn snapshot_to_quote_returns_none_without_quote() {
        let snapshot = serde_json::json!({"price": 1.23});
        assert!(snapshot_to_quote(&snapshot, "EURUSD:FxSpot").is_none());
    }

    #[test]
    fn forced_liquidation_detection_filled_without_external_ref() {
        let order = serde_json::json!({
            "OrderId": "12345",
            "Status": "Filled",
            "Uic": 21
        });
        assert!(is_forced_liquidation(&order));
    }

    #[test]
    fn forced_liquidation_detection_with_external_ref_is_not_forced() {
        let order = serde_json::json!({
            "OrderId": "12345",
            "Status": "Filled",
            "ExternalReference": "my-order-1",
            "Uic": 21
        });
        assert!(!is_forced_liquidation(&order));
    }

    #[test]
    fn forced_liquidation_detection_working_without_ref_is_not_forced() {
        let order = serde_json::json!({
            "OrderId": "12345",
            "Status": "Working",
            "Uic": 21
        });
        assert!(!is_forced_liquidation(&order));
    }

    // ========================================================================
    // Order event translation tests
    // ========================================================================

    #[test]
    fn saxo_status_to_event_type_mapping() {
        use tektii_gateway_core::websocket::messages::OrderEventType;

        assert_eq!(
            saxo_status_to_event_type("Working", 0.0),
            Some(OrderEventType::OrderCreated)
        );
        assert_eq!(
            saxo_status_to_event_type("Filled", 0.0),
            Some(OrderEventType::OrderFilled)
        );
        assert_eq!(
            saxo_status_to_event_type("FinalFill", 0.0),
            Some(OrderEventType::OrderFilled)
        );
        assert_eq!(
            saxo_status_to_event_type("PartiallyFilled", 0.0),
            Some(OrderEventType::OrderPartiallyFilled)
        );
        assert_eq!(
            saxo_status_to_event_type("Cancelled", 0.0),
            Some(OrderEventType::OrderCancelled)
        );
        assert_eq!(
            saxo_status_to_event_type("Canceled", 0.0),
            Some(OrderEventType::OrderCancelled)
        );
        assert_eq!(
            saxo_status_to_event_type("Rejected", 0.0),
            Some(OrderEventType::OrderRejected)
        );
        assert_eq!(
            saxo_status_to_event_type("Expired", 0.0),
            Some(OrderEventType::OrderExpired)
        );

        // Transient statuses → None
        assert_eq!(
            saxo_status_to_event_type("LockedPlacementPending", 0.0),
            None
        );
        assert_eq!(saxo_status_to_event_type("Placed", 0.0), None);
        assert_eq!(saxo_status_to_event_type("NotWorking", 0.0), None);
        assert_eq!(saxo_status_to_event_type("SomethingUnknown", 0.0), None);
    }

    #[test]
    fn partial_fill_detected_from_working_with_filled_amount() {
        assert_eq!(
            saxo_status_to_event_type("Working", 500.0),
            Some(OrderEventType::OrderPartiallyFilled)
        );
    }

    fn test_instrument_map() -> SaxoInstrumentMap {
        use std::collections::HashMap;
        let mut uic_to_symbol = HashMap::new();
        uic_to_symbol.insert((21, "FxSpot".to_string()), "EURUSD:FxSpot".to_string());
        SaxoInstrumentMap {
            symbol_to_saxo: HashMap::new(),
            uic_to_symbol,
        }
    }

    #[test]
    fn translate_order_from_snapshot_valid() {
        let map = test_instrument_map();
        let snapshot = serde_json::json!({
            "OrderId": "ORD-123",
            "Uic": 21,
            "AssetType": "FxSpot",
            "BuySell": "Buy",
            "OrderType": "Limit",
            "Amount": 10000.0,
            "FilledAmount": 0.0,
            "Price": 1.0850,
            "Status": "Working",
            "ExternalReference": "my-ref-1"
        });

        let order = translate_order_from_snapshot(&snapshot, &map).unwrap();
        assert_eq!(order.id, "ORD-123");
        assert_eq!(order.symbol, "EURUSD:FxSpot");
        assert_eq!(order.side, tektii_gateway_core::models::Side::Buy);
        assert_eq!(
            order.order_type,
            tektii_gateway_core::models::OrderType::Limit
        );
        assert_eq!(order.client_order_id, Some("my-ref-1".to_string()));
        assert_eq!(order.status, tektii_gateway_core::models::OrderStatus::Open);
    }

    #[test]
    fn translate_order_from_snapshot_missing_fields_returns_none() {
        let map = test_instrument_map();
        // Missing required fields like OrderType, BuySell
        let snapshot = serde_json::json!({
            "OrderId": "ORD-123",
            "Uic": 21
        });
        assert!(translate_order_from_snapshot(&snapshot, &map).is_none());
    }

    #[test]
    fn translate_order_from_snapshot_unknown_uic_returns_none() {
        let map = test_instrument_map();
        let snapshot = serde_json::json!({
            "OrderId": "ORD-456",
            "Uic": 999,
            "AssetType": "FxSpot",
            "BuySell": "Sell",
            "OrderType": "Market",
            "Amount": 5000.0,
            "FilledAmount": 0.0,
            "Status": "Working"
        });
        assert!(translate_order_from_snapshot(&snapshot, &map).is_none());
    }
}
