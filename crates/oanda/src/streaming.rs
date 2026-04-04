//! Oanda HTTP streaming provider for real-time price and transaction events.
//!
//! Oanda uses HTTP chunked transfer encoding (NDJSON) instead of WebSocket for
//! streaming. This module wraps two concurrent HTTP streams behind the
//! [`WebSocketProvider`] trait so the rest of the gateway treats Oanda identically
//! to Alpaca/Binance.
//!
//! # Streams
//!
//! - **Price stream**: `GET /v3/accounts/{id}/pricing/stream?instruments=...`
//!   Emits [`WsMessage::QuoteData`] for each price update.
//! - **Transaction stream**: `GET /v3/accounts/{id}/transactions/stream`
//!   Emits [`WsMessage::Order`] for fills, cancels, and new orders.

use secrecy::{ExposeSecret, SecretBox};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::BytesMut;
use chrono::Utc;
use reqwest::Client;
use rust_decimal::Decimal;
use std::str::FromStr;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::adapter::OandaAdapter;
use super::types::{OandaPriceStreamMessage, OandaTransactionStreamLine};
use crate::credentials::OandaCredentials;
use tektii_gateway_core::models::{
    Order, OrderStatus, OrderType, Quote, RejectReason, Side, TimeInForce, TradingPlatform,
};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{
    EventAckMessage, InternalTradingEvent, OrderEventType, WsMessage,
};
use tektii_gateway_core::websocket::provider::{EventStream, ProviderConfig, WebSocketProvider};

// ============================================================================
// Constants
// ============================================================================

/// Base URL for Oanda practice streaming endpoints.
const OANDA_PRACTICE_STREAM_URL: &str = "https://stream-fxpractice.oanda.com";
/// Base URL for Oanda live streaming endpoints.
const OANDA_LIVE_STREAM_URL: &str = "https://stream-fxtrade.oanda.com";

/// Heartbeat timeout -- if no data arrives within this window, reconnect.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum reconnection backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Initial reconnection backoff.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

// ============================================================================
// Error type (internal to streaming)
// ============================================================================

/// Errors that can occur during HTTP stream reading.
#[derive(Debug)]
enum StreamError {
    /// Network/HTTP error.
    Network(String),
    /// No data received within the heartbeat timeout.
    HeartbeatTimeout,
    /// Stream was cancelled via token.
    Cancelled,
    /// Permanent auth error (401/403) -- do not retry.
    PermanentAuth(String),
}

// ============================================================================
// Provider
// ============================================================================

/// Oanda HTTP streaming provider implementing [`WebSocketProvider`].
///
/// Wraps two concurrent HTTP chunked-transfer streams (pricing + transactions)
/// behind the standard WebSocket provider trait.
#[derive(Clone)]
pub struct OandaWebSocketProvider {
    /// Base URL for streaming endpoints.
    stream_url: String,
    /// Bearer API token.
    api_token: Arc<SecretBox<String>>,
    /// Oanda account ID.
    account_id: String,
    /// Platform identifier (OandaPractice or OandaLive).
    platform: TradingPlatform,
    /// Currently subscribed instruments in Oanda format (e.g., "EUR_USD").
    instruments: Arc<RwLock<Vec<String>>>,
    /// Stored config from initial connect() for reconnection.
    stored_config: Arc<RwLock<Option<ProviderConfig>>>,
    /// Channel sender for events to the strategy.
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<WsMessage>>>>,
    /// Top-level cancellation token (cancels all streams).
    cancel_token: CancellationToken,
    /// Separate token for price stream -- cancelled on subscribe/unsubscribe to
    /// respawn with updated instrument list.
    price_stream_cancel: Arc<RwLock<CancellationToken>>,
    /// HTTP client (shared across reconnections).
    client: Client,
}

impl OandaWebSocketProvider {
    /// Create a new provider from credentials.
    pub fn new(credentials: &OandaCredentials, platform: TradingPlatform) -> Self {
        let stream_url = credentials
            .stream_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::OandaLive => OANDA_LIVE_STREAM_URL.to_string(),
                _ => OANDA_PRACTICE_STREAM_URL.to_string(),
            });

        Self {
            stream_url,
            api_token: tektii_gateway_core::arc_secret(&credentials.api_token),
            account_id: credentials.account_id.clone(),
            platform,
            instruments: Arc::new(RwLock::new(Vec::new())),
            event_tx: Arc::new(RwLock::new(None)),
            cancel_token: CancellationToken::new(),
            stored_config: Arc::new(RwLock::new(None)),
            price_stream_cancel: Arc::new(RwLock::new(CancellationToken::new())),
            client: Client::new(),
        }
    }

    // ========================================================================
    // Internal trading stream (for EventRouter)
    // ========================================================================

    /// Start the transaction stream and emit events on the internal broadcast channel.
    ///
    /// This is used by the registry to feed order events into the `EventRouter` for
    /// position synthesis. Only spawns the transaction stream (no price stream).
    pub fn connect_internal_trading_stream(
        &self,
        internal_tx: broadcast::Sender<InternalTradingEvent>,
        cancel_token: CancellationToken,
        platform: TradingPlatform,
    ) -> Result<(), WebSocketError> {
        let provider = self.clone();

        tokio::spawn(async move {
            run_transaction_stream_internal(
                &provider.client,
                &provider.stream_url,
                provider.api_token.expose_secret(),
                &provider.account_id,
                platform,
                internal_tx,
                cancel_token,
            )
            .await;
        });

        Ok(())
    }

    // ========================================================================
    // Price stream lifecycle
    // ========================================================================

    /// Spawn (or respawn) the price stream task with the current instrument list.
    async fn spawn_price_stream(&self) {
        // Cancel existing price stream if running.
        let old_cancel = {
            let mut guard = self.price_stream_cancel.write().await;
            let old = guard.clone();
            *guard = CancellationToken::new();
            old
        };
        old_cancel.cancel();

        let instruments = self.instruments.read().await.clone();
        if instruments.is_empty() {
            debug!("No instruments to stream -- price stream not started");
            return;
        }

        let new_cancel = self.price_stream_cancel.read().await.clone();
        let parent_cancel = self.cancel_token.clone();
        let event_tx = self.event_tx.clone();
        let client = self.client.clone();
        let stream_url = self.stream_url.clone();
        let api_token = Arc::clone(&self.api_token);
        let account_id = self.account_id.clone();
        let platform = self.platform;

        tokio::spawn(async move {
            run_price_stream(
                &client,
                &stream_url,
                api_token.expose_secret(),
                &account_id,
                &instruments,
                platform,
                event_tx,
                new_cancel,
                parent_cancel,
            )
            .await;
        });
    }
}

// ============================================================================
// WebSocketProvider trait implementation
// ============================================================================

#[async_trait]
impl WebSocketProvider for OandaWebSocketProvider {
    async fn connect(&self, config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        *self.stored_config.write().await = Some(config.clone());

        // Store instruments (passed through as-is to Oanda).
        self.instruments.write().await.clone_from(&config.symbols);

        // Create event channel.
        let (tx, rx) = mpsc::unbounded_channel();
        *self.event_tx.write().await = Some(tx);

        // Spawn price stream.
        self.spawn_price_stream().await;

        // Spawn transaction stream.
        let provider = self.clone();
        let cancel = self.cancel_token.clone();
        tokio::spawn(async move {
            run_transaction_stream(
                &provider.client,
                &provider.stream_url,
                provider.api_token.expose_secret(),
                &provider.account_id,
                provider.platform,
                provider.event_tx.clone(),
                cancel,
            )
            .await;
        });

        info!(
            platform = %self.platform,
            "Oanda streaming connected"
        );

        Ok(rx)
    }

    async fn subscribe(
        &self,
        symbols: Vec<String>,
        _event_types: Vec<String>,
    ) -> Result<(), WebSocketError> {
        {
            let mut instruments = self.instruments.write().await;
            for symbol in &symbols {
                if !instruments.contains(symbol) {
                    instruments.push(symbol.clone());
                }
            }
        }

        // Respawn price stream with updated instruments.
        self.spawn_price_stream().await;

        debug!(
            platform = %self.platform,
            ?symbols,
            "Oanda subscribe -- respawned price stream"
        );
        Ok(())
    }

    async fn unsubscribe(&self, symbols: Vec<String>) -> Result<(), WebSocketError> {
        {
            let mut instruments = self.instruments.write().await;
            for symbol in &symbols {
                instruments.retain(|i| i != symbol);
            }
        }

        // Respawn price stream with updated instruments.
        self.spawn_price_stream().await;

        debug!(
            platform = %self.platform,
            ?symbols,
            "Oanda unsubscribe -- respawned price stream"
        );
        Ok(())
    }

    async fn handle_ack(&self, _ack: EventAckMessage) -> Result<(), WebSocketError> {
        // No-op for live trading -- no time control needed.
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), WebSocketError> {
        self.cancel_token.cancel();
        info!(
            platform = %self.platform,
            "Oanda streaming disconnected"
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
// Price stream
// ============================================================================

/// Run the price stream with reconnection.
#[allow(clippy::too_many_arguments)]
async fn run_price_stream(
    client: &Client,
    stream_url: &str,
    api_token: &str,
    account_id: &str,
    instruments: &[String],
    platform: TradingPlatform,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<WsMessage>>>>,
    stream_cancel: CancellationToken,
    parent_cancel: CancellationToken,
) {
    let instrument_param = instruments.join(",");
    let url = format!(
        "{stream_url}/v3/accounts/{account_id}/pricing/stream?instruments={instrument_param}"
    );
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let result = run_single_price_stream(
            client,
            &url,
            api_token,
            platform,
            &event_tx,
            &stream_cancel,
            &parent_cancel,
        )
        .await;

        match result {
            StreamError::Cancelled => {
                debug!(platform = %platform, "Price stream cancelled");
                return;
            }
            StreamError::PermanentAuth(msg) => {
                error!(platform = %platform, error = %msg, "Price stream permanent auth error -- stopping");
                return;
            }
            StreamError::HeartbeatTimeout => {
                warn!(
                    platform = %platform,
                    backoff_secs = backoff.as_secs(),
                    "Price stream heartbeat timeout -- reconnecting"
                );
            }
            StreamError::Network(msg) => {
                warn!(
                    platform = %platform,
                    error = %msg,
                    backoff_secs = backoff.as_secs(),
                    "Price stream error -- reconnecting"
                );
            }
        }

        tokio::select! {
            () = tokio::time::sleep(backoff) => {},
            () = stream_cancel.cancelled() => return,
            () = parent_cancel.cancelled() => return,
        }

        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Run a single price stream connection. Returns when the stream ends.
async fn run_single_price_stream(
    client: &Client,
    url: &str,
    api_token: &str,
    platform: TradingPlatform,
    event_tx: &Arc<RwLock<Option<mpsc::UnboundedSender<WsMessage>>>>,
    stream_cancel: &CancellationToken,
    parent_cancel: &CancellationToken,
) -> StreamError {
    let mut response = match client.get(url).bearer_auth(api_token).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return StreamError::PermanentAuth(format!("HTTP {status}"));
            }
            if !status.is_success() {
                return StreamError::Network(format!("HTTP {status}"));
            }
            resp
        }
        Err(e) => return StreamError::Network(e.to_string()),
    };

    let mut buffer = BytesMut::new();
    let mut deadline = Instant::now() + HEARTBEAT_TIMEOUT;

    loop {
        tokio::select! {
            chunk = response.chunk() => {
                match chunk {
                    Ok(Some(data)) => {
                        deadline = Instant::now() + HEARTBEAT_TIMEOUT;
                        buffer.extend_from_slice(&data);
                        process_price_buffer(&mut buffer, platform, event_tx).await;
                    },
                    Ok(None) => {
                        return StreamError::Network("Stream ended (EOF)".to_string());
                    },
                    Err(e) => {
                        return StreamError::Network(e.to_string());
                    },
                }
            },
            () = tokio::time::sleep_until(deadline) => {
                return StreamError::HeartbeatTimeout;
            },
            () = stream_cancel.cancelled() => {
                return StreamError::Cancelled;
            },
            () = parent_cancel.cancelled() => {
                return StreamError::Cancelled;
            },
        }
    }
}

/// Process complete NDJSON lines from the buffer and emit quote events.
async fn process_price_buffer(
    buffer: &mut BytesMut,
    platform: TradingPlatform,
    event_tx: &Arc<RwLock<Option<mpsc::UnboundedSender<WsMessage>>>>,
) {
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let line = buffer.split_to(pos + 1);
        let line_str = String::from_utf8_lossy(&line).trim().to_string();

        if line_str.is_empty() {
            continue;
        }

        match serde_json::from_str::<OandaPriceStreamMessage>(&line_str) {
            Ok(OandaPriceStreamMessage::Price(price)) => {
                let messages = price_to_quote(
                    &price.instrument,
                    &price.bids,
                    &price.asks,
                    &price.time,
                    platform,
                );
                let tx_guard = event_tx.read().await;
                if let Some(tx) = tx_guard.as_ref() {
                    for msg in messages {
                        let _ = tx.send(msg);
                    }
                }
            }
            Ok(OandaPriceStreamMessage::Heartbeat(_)) => {
                // Heartbeat -- no message emitted, deadline already updated by caller.
            }
            Err(e) => {
                warn!(
                    platform = %platform,
                    error = %e,
                    line = %line_str,
                    "Failed to parse price stream NDJSON line -- skipping"
                );
            }
        }
    }
}

// ============================================================================
// Transaction stream
// ============================================================================

/// Run the transaction stream with reconnection (strategy-facing channel).
async fn run_transaction_stream(
    client: &Client,
    stream_url: &str,
    api_token: &str,
    account_id: &str,
    platform: TradingPlatform,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<WsMessage>>>>,
    cancel_token: CancellationToken,
) {
    let url = format!("{stream_url}/v3/accounts/{account_id}/transactions/stream");
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let result =
            run_single_transaction_stream(client, &url, api_token, platform, &cancel_token).await;

        match &result.0 {
            StreamError::Cancelled => {
                debug!(platform = %platform, "Transaction stream cancelled");
                return;
            }
            StreamError::PermanentAuth(msg) => {
                error!(
                    platform = %platform,
                    error = %msg,
                    "Transaction stream permanent auth error -- stopping"
                );
                return;
            }
            StreamError::HeartbeatTimeout => {
                warn!(
                    platform = %platform,
                    backoff_secs = backoff.as_secs(),
                    "Transaction stream heartbeat timeout -- reconnecting"
                );
            }
            StreamError::Network(msg) => {
                warn!(
                    platform = %platform,
                    error = %msg,
                    backoff_secs = backoff.as_secs(),
                    "Transaction stream error -- reconnecting"
                );
            }
        }

        // Emit buffered messages before sleeping for backoff.
        let tx_guard = event_tx.read().await;
        if let Some(tx) = tx_guard.as_ref() {
            for msg in result.1 {
                let _ = tx.send(msg);
            }
        }
        drop(tx_guard);

        tokio::select! {
            () = tokio::time::sleep(backoff) => {},
            () = cancel_token.cancelled() => return,
        }

        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Run the transaction stream with reconnection (internal broadcast channel).
async fn run_transaction_stream_internal(
    client: &Client,
    stream_url: &str,
    api_token: &str,
    account_id: &str,
    platform: TradingPlatform,
    internal_tx: broadcast::Sender<InternalTradingEvent>,
    cancel_token: CancellationToken,
) {
    let url = format!("{stream_url}/v3/accounts/{account_id}/transactions/stream");
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let result =
            run_single_transaction_stream(client, &url, api_token, platform, &cancel_token).await;

        match &result.0 {
            StreamError::Cancelled => {
                debug!(platform = %platform, "Internal transaction stream cancelled");
                return;
            }
            StreamError::PermanentAuth(msg) => {
                error!(
                    platform = %platform,
                    error = %msg,
                    "Internal transaction stream permanent auth error -- stopping"
                );
                return;
            }
            StreamError::HeartbeatTimeout => {
                warn!(
                    platform = %platform,
                    backoff_secs = backoff.as_secs(),
                    "Internal transaction stream heartbeat timeout -- reconnecting"
                );
            }
            StreamError::Network(msg) => {
                warn!(
                    platform = %platform,
                    error = %msg,
                    backoff_secs = backoff.as_secs(),
                    "Internal transaction stream error -- reconnecting"
                );
            }
        }

        // Emit buffered messages on internal channel.
        for msg in result.1 {
            let event = InternalTradingEvent::new(msg, platform);
            let _ = internal_tx.send(event);
        }

        tokio::select! {
            () = tokio::time::sleep(backoff) => {},
            () = cancel_token.cancelled() => return,
        }

        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Run a single transaction stream connection. Returns the error reason and
/// any messages parsed before the error occurred.
async fn run_single_transaction_stream(
    client: &Client,
    url: &str,
    api_token: &str,
    platform: TradingPlatform,
    cancel_token: &CancellationToken,
) -> (StreamError, Vec<WsMessage>) {
    let mut response = match client.get(url).bearer_auth(api_token).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return (StreamError::PermanentAuth(format!("HTTP {status}")), vec![]);
            }
            if !status.is_success() {
                return (StreamError::Network(format!("HTTP {status}")), vec![]);
            }
            resp
        }
        Err(e) => return (StreamError::Network(e.to_string()), vec![]),
    };

    let mut buffer = BytesMut::new();
    let mut deadline = Instant::now() + HEARTBEAT_TIMEOUT;
    let mut messages = Vec::new();

    loop {
        tokio::select! {
            chunk = response.chunk() => {
                match chunk {
                    Ok(Some(data)) => {
                        deadline = Instant::now() + HEARTBEAT_TIMEOUT;
                        buffer.extend_from_slice(&data);
                        process_transaction_buffer(&mut buffer, platform, &mut messages);
                    },
                    Ok(None) => {
                        return (StreamError::Network("Stream ended (EOF)".to_string()), messages);
                    },
                    Err(e) => {
                        return (StreamError::Network(e.to_string()), messages);
                    },
                }
            },
            () = tokio::time::sleep_until(deadline) => {
                return (StreamError::HeartbeatTimeout, messages);
            },
            () = cancel_token.cancelled() => {
                return (StreamError::Cancelled, messages);
            },
        }
    }
}

/// Process complete NDJSON lines from the buffer and collect order events.
fn process_transaction_buffer(
    buffer: &mut BytesMut,
    platform: TradingPlatform,
    messages: &mut Vec<WsMessage>,
) {
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let line = buffer.split_to(pos + 1);
        let line_str = String::from_utf8_lossy(&line).trim().to_string();

        if line_str.is_empty() {
            continue;
        }

        match serde_json::from_str::<OandaTransactionStreamLine>(&line_str) {
            Ok(line) if line.transaction_type == "HEARTBEAT" => {
                // Heartbeat -- no message emitted.
            }
            Ok(line) => {
                let events = transaction_to_messages(&line, platform);
                messages.extend(events);
            }
            Err(e) => {
                warn!(
                    platform = %platform,
                    error = %e,
                    line = %line_str,
                    "Failed to parse transaction stream NDJSON line -- skipping"
                );
            }
        }
    }
}

// ============================================================================
// Message conversion helpers
// ============================================================================

/// Map an Oanda reject reason string to the gateway's `RejectReason` enum.
fn oanda_reject_reason(reason: &str) -> RejectReason {
    match reason {
        "INSUFFICIENT_MARGIN" => RejectReason::InsufficientMargin,
        "INSUFFICIENT_LIQUIDITY" | "INSUFFICIENT_FUNDS" => RejectReason::InsufficientBalance,
        "INSTRUMENT_NOT_TRADEABLE" => RejectReason::SymbolNotTradeable,
        "MARKET_HALTED" => RejectReason::MarketClosed,
        _ => RejectReason::Unknown,
    }
}

/// Convert a streaming price update to a `WsMessage::QuoteData`.
fn price_to_quote(
    instrument: &str,
    bids: &[super::types::OandaPriceBucket],
    asks: &[super::types::OandaPriceBucket],
    _time: &str,
    platform: TradingPlatform,
) -> Vec<WsMessage> {
    let Some(bid) = bids.first().and_then(|b| Decimal::from_str(&b.price).ok()) else {
        return vec![];
    };
    let Some(ask) = asks.first().and_then(|a| Decimal::from_str(&a.price).ok()) else {
        return vec![];
    };

    // Mid price as last.
    let mid = (bid + ask) / Decimal::from(2);
    let symbol = instrument.to_string();

    let quote = Quote {
        symbol,
        provider: platform.header_value().to_string(),
        bid,
        bid_size: bids.first().map(|b| Decimal::from(b.liquidity)),
        ask,
        ask_size: asks.first().map(|a| Decimal::from(a.liquidity)),
        last: mid,
        volume: None,
        timestamp: Utc::now(),
    };

    vec![WsMessage::quote(quote)]
}

/// Convert a streaming transaction to zero or more `WsMessage`s.
fn transaction_to_messages(
    tx: &OandaTransactionStreamLine,
    platform: TradingPlatform,
) -> Vec<WsMessage> {
    match tx.transaction_type.as_str() {
        "ORDER_FILL" => transaction_fill_to_messages(tx, platform),
        "ORDER_CANCEL" => transaction_cancel_to_messages(tx, platform),
        "LIMIT_ORDER" | "STOP_ORDER" | "MARKET_IF_TOUCHED_ORDER" => {
            transaction_new_order_to_messages(tx, platform)
        }
        "MARKET_ORDER_REJECT" => transaction_reject_to_messages(tx, platform),
        // Ignored transaction types: STOP_LOSS_ORDER, TAKE_PROFIT_ORDER,
        // TRAILING_STOP_LOSS_ORDER, DAILY_FINANCING, etc.
        other => {
            debug!(
                platform = %platform,
                transaction_type = other,
                id = ?tx.id,
                "Ignoring transaction type"
            );
            vec![]
        }
    }
}

/// ORDER_FILL -> Filled order event.
fn transaction_fill_to_messages(
    tx: &OandaTransactionStreamLine,
    _platform: TradingPlatform,
) -> Vec<WsMessage> {
    let symbol = tx
        .instrument
        .as_deref()
        .map(String::from)
        .unwrap_or_default();

    let (side, quantity) = tx
        .units
        .as_deref()
        .and_then(|u| OandaAdapter::from_oanda_units(u).ok())
        .unwrap_or((Side::Buy, Decimal::ZERO));

    let fill_price = tx.price.as_deref().and_then(|p| Decimal::from_str(p).ok());

    let order_id = tx
        .order_id
        .as_deref()
        .or(tx.id.as_deref())
        .unwrap_or("unknown");

    let order = Order {
        id: order_id.to_string(),
        client_order_id: None,
        symbol,
        side,
        order_type: OrderType::Market,
        quantity,
        filled_quantity: quantity,
        remaining_quantity: Decimal::ZERO,
        limit_price: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        average_fill_price: fill_price,
        status: OrderStatus::Filled,
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
    };

    vec![WsMessage::Order {
        event: OrderEventType::OrderFilled,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    }]
}

/// ORDER_CANCEL -> Cancelled order event.
fn transaction_cancel_to_messages(
    tx: &OandaTransactionStreamLine,
    _platform: TradingPlatform,
) -> Vec<WsMessage> {
    let symbol = tx
        .instrument
        .as_deref()
        .map(String::from)
        .unwrap_or_default();

    let order_id = tx
        .order_id
        .as_deref()
        .or(tx.id.as_deref())
        .unwrap_or("unknown");

    let order = Order {
        id: order_id.to_string(),
        client_order_id: None,
        symbol,
        side: Side::Buy,
        order_type: OrderType::Limit,
        quantity: Decimal::ZERO,
        filled_quantity: Decimal::ZERO,
        remaining_quantity: Decimal::ZERO,
        limit_price: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        average_fill_price: None,
        status: OrderStatus::Cancelled,
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
    };

    vec![WsMessage::Order {
        event: OrderEventType::OrderCancelled,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    }]
}

/// LIMIT_ORDER / STOP_ORDER / MARKET_IF_TOUCHED_ORDER -> Open order event.
fn transaction_new_order_to_messages(
    tx: &OandaTransactionStreamLine,
    _platform: TradingPlatform,
) -> Vec<WsMessage> {
    let symbol = tx
        .instrument
        .as_deref()
        .map(String::from)
        .unwrap_or_default();

    let (side, quantity) = tx
        .units
        .as_deref()
        .and_then(|u| OandaAdapter::from_oanda_units(u).ok())
        .unwrap_or((Side::Buy, Decimal::ZERO));

    let trigger_price = tx.price.as_deref().and_then(|p| Decimal::from_str(p).ok());

    let order_type = match tx.transaction_type.as_str() {
        "LIMIT_ORDER" => OrderType::Limit,
        "STOP_ORDER" => OrderType::Stop,
        _ => OrderType::StopLimit,
    };

    let (limit_price, stop_price) = match order_type {
        OrderType::Limit => (trigger_price, None),
        OrderType::Stop | OrderType::StopLimit => (None, trigger_price),
        _ => (None, None),
    };

    let order = Order {
        id: tx.id.clone().unwrap_or_default(),
        client_order_id: None,
        symbol,
        side,
        order_type,
        quantity,
        filled_quantity: Decimal::ZERO,
        remaining_quantity: quantity,
        limit_price,
        stop_price,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        average_fill_price: None,
        status: OrderStatus::Open,
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
    };

    vec![WsMessage::Order {
        event: OrderEventType::OrderCreated,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    }]
}

/// MARKET_ORDER_REJECT -> Rejected order event.
fn transaction_reject_to_messages(
    tx: &OandaTransactionStreamLine,
    _platform: TradingPlatform,
) -> Vec<WsMessage> {
    let symbol = tx
        .instrument
        .as_deref()
        .map(String::from)
        .unwrap_or_default();

    let (side, quantity) = tx
        .units
        .as_deref()
        .and_then(|u| OandaAdapter::from_oanda_units(u).ok())
        .unwrap_or((Side::Buy, Decimal::ZERO));

    let order = Order {
        id: tx.id.clone().unwrap_or_default(),
        client_order_id: None,
        symbol,
        side,
        order_type: OrderType::Market,
        quantity,
        filled_quantity: Decimal::ZERO,
        remaining_quantity: quantity,
        limit_price: None,
        stop_price: None,
        stop_loss: None,
        take_profit: None,
        trailing_distance: None,
        trailing_type: None,
        average_fill_price: None,
        status: OrderStatus::Rejected,
        reject_reason: tx.reject_reason.as_deref().map(oanda_reject_reason),
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
    };

    vec![WsMessage::Order {
        event: OrderEventType::OrderRejected,
        order,
        parent_order_id: None,
        timestamp: Utc::now(),
    }]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OandaPriceBucket;

    /// Helper to create a test transaction stream line with sensible defaults.
    fn test_tx(id: &str, tx_type: &str) -> OandaTransactionStreamLine {
        OandaTransactionStreamLine {
            transaction_type: tx_type.to_string(),
            id: Some(id.to_string()),
            instrument: None,
            units: None,
            price: None,
            time: None,
            order_id: None,
            reason: None,
            reject_reason: None,
            last_transaction_id: None,
        }
    }

    #[test]
    fn price_to_quote_basic() {
        let bids = vec![OandaPriceBucket {
            price: "1.08520".to_string(),
            liquidity: 1_000_000,
        }];
        let asks = vec![OandaPriceBucket {
            price: "1.08530".to_string(),
            liquidity: 2_000_000,
        }];

        let msgs = price_to_quote(
            "EUR_USD",
            &bids,
            &asks,
            "2024-01-15T10:30:00Z",
            TradingPlatform::OandaPractice,
        );
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::QuoteData { quote, .. } => {
                assert_eq!(quote.symbol, "EUR_USD");
                assert_eq!(quote.bid, Decimal::from_str("1.08520").unwrap());
                assert_eq!(quote.ask, Decimal::from_str("1.08530").unwrap());
                // Mid = (1.08520 + 1.08530) / 2 = 1.08525
                assert_eq!(quote.last, Decimal::from_str("1.08525").unwrap());
                assert_eq!(quote.bid_size, Some(Decimal::from(1_000_000)));
                assert_eq!(quote.ask_size, Some(Decimal::from(2_000_000)));
                assert_eq!(quote.provider, "oanda-practice");
            }
            other => panic!("Expected QuoteData, got {other:?}"),
        }
    }

    #[test]
    fn price_to_quote_empty_bids() {
        let msgs = price_to_quote(
            "EUR_USD",
            &[],
            &[],
            "2024-01-15T10:30:00Z",
            TradingPlatform::OandaPractice,
        );
        assert!(
            msgs.is_empty(),
            "Should produce no messages when bids are empty"
        );
    }

    #[test]
    fn price_to_quote_multi_tier_uses_best() {
        let bids = vec![
            OandaPriceBucket {
                price: "1.27200".to_string(),
                liquidity: 1_000_000,
            },
            OandaPriceBucket {
                price: "1.27195".to_string(),
                liquidity: 5_000_000,
            },
        ];
        let asks = vec![
            OandaPriceBucket {
                price: "1.27210".to_string(),
                liquidity: 1_000_000,
            },
            OandaPriceBucket {
                price: "1.27215".to_string(),
                liquidity: 5_000_000,
            },
        ];

        let msgs = price_to_quote(
            "GBP_USD",
            &bids,
            &asks,
            "2024-01-15T10:30:00Z",
            TradingPlatform::OandaLive,
        );
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::QuoteData { quote, .. } => {
                assert_eq!(quote.symbol, "GBP_USD");
                assert_eq!(quote.bid, Decimal::from_str("1.27200").unwrap());
                assert_eq!(quote.ask, Decimal::from_str("1.27210").unwrap());
                assert_eq!(quote.provider, "oanda-live");
            }
            other => panic!("Expected QuoteData, got {other:?}"),
        }
    }

    #[test]
    fn transaction_fill_to_messages_basic() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("10000".to_string()),
            price: Some("1.08525".to_string()),
            time: Some("2024-01-15T10:30:00Z".to_string()),
            order_id: Some("6357".to_string()),
            reason: Some("MARKET_ORDER".to_string()),
            ..test_tx("6360", "ORDER_FILL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { event, order, .. } => {
                assert_eq!(*event, OrderEventType::OrderFilled);
                assert_eq!(order.id, "6357");
                assert_eq!(order.symbol, "EUR_USD");
                assert_eq!(order.side, Side::Buy);
                assert_eq!(order.quantity, Decimal::from(10000));
                assert_eq!(
                    order.average_fill_price,
                    Some(Decimal::from_str("1.08525").unwrap())
                );
                assert_eq!(order.status, OrderStatus::Filled);
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_fill_sell_side() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("GBP_USD".to_string()),
            units: Some("-5000".to_string()),
            price: Some("1.27200".to_string()),
            ..test_tx("6361", "ORDER_FILL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { order, .. } => {
                assert_eq!(order.side, Side::Sell);
                assert_eq!(order.quantity, Decimal::from(5000));
                // No orderId => use transaction id.
                assert_eq!(order.id, "6361");
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_cancel_to_messages_basic() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            order_id: Some("6356".to_string()),
            reason: Some("CLIENT_REQUEST".to_string()),
            ..test_tx("6365", "ORDER_CANCEL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { event, order, .. } => {
                assert_eq!(*event, OrderEventType::OrderCancelled);
                assert_eq!(order.id, "6356");
                assert_eq!(order.status, OrderStatus::Cancelled);
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_new_limit_order() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("10000".to_string()),
            price: Some("1.08500".to_string()),
            ..test_tx("6370", "LIMIT_ORDER")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { event, order, .. } => {
                assert_eq!(*event, OrderEventType::OrderCreated);
                assert_eq!(order.id, "6370");
                assert_eq!(order.order_type, OrderType::Limit);
                assert_eq!(
                    order.limit_price,
                    Some(Decimal::from_str("1.08500").unwrap())
                );
                assert_eq!(order.status, OrderStatus::Open);
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_new_stop_order() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("GBP_USD".to_string()),
            units: Some("-5000".to_string()),
            price: Some("1.27000".to_string()),
            ..test_tx("6371", "STOP_ORDER")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { order, .. } => {
                assert_eq!(order.order_type, OrderType::Stop);
                assert_eq!(
                    order.stop_price,
                    Some(Decimal::from_str("1.27000").unwrap())
                );
                assert_eq!(order.side, Side::Sell);
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_reject_to_messages_basic() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("10000".to_string()),
            reject_reason: Some("INSUFFICIENT_MARGIN".to_string()),
            ..test_tx("6375", "MARKET_ORDER_REJECT")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { event, order, .. } => {
                assert_eq!(*event, OrderEventType::OrderRejected);
                assert_eq!(order.status, OrderStatus::Rejected);
                assert_eq!(order.reject_reason, Some(RejectReason::InsufficientMargin));
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_ignored_types_produce_no_messages() {
        for tx_type in &[
            "STOP_LOSS_ORDER",
            "TAKE_PROFIT_ORDER",
            "TRAILING_STOP_LOSS_ORDER",
            "DAILY_FINANCING",
        ] {
            let tx = test_tx("9999", tx_type);

            let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
            assert!(
                msgs.is_empty(),
                "Transaction type '{tx_type}' should produce no messages"
            );
        }
    }

    #[test]
    fn ndjson_buffer_processing() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"HEARTBEAT","time":"2024-01-15T10:30:05.000000000Z","lastTransactionID":"6360"}
{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderId":"6357"}
"#,
        );

        let mut messages = Vec::new();
        process_transaction_buffer(&mut buffer, TradingPlatform::OandaPractice, &mut messages);

        // Heartbeat produces no messages, ORDER_FILL produces one.
        assert_eq!(messages.len(), 1);
        assert!(buffer.is_empty(), "Buffer should be fully consumed");
    }

    #[test]
    fn ndjson_partial_line_stays_in_buffer() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(br#"{"type":"HEARTBEAT","time":"2024-01-"#);

        let mut messages = Vec::new();
        process_transaction_buffer(&mut buffer, TradingPlatform::OandaPractice, &mut messages);

        assert!(messages.is_empty());
        assert!(!buffer.is_empty(), "Partial line should remain in buffer");
    }

    #[test]
    fn ndjson_malformed_line_skipped() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(b"not valid json\n");

        let mut messages = Vec::new();
        process_transaction_buffer(&mut buffer, TradingPlatform::OandaPractice, &mut messages);

        assert!(
            messages.is_empty(),
            "Malformed line should produce no messages"
        );
        assert!(
            buffer.is_empty(),
            "Malformed line should be consumed from buffer"
        );
    }

    #[test]
    fn subscribe_updates_instruments() {
        let creds = OandaCredentials::new("test", "test-account");
        let provider = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaPractice);

        // Verify initial state.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let instruments = provider.instruments.read().await;
            assert!(instruments.is_empty());
        });
    }

    #[test]
    fn default_stream_urls() {
        let creds = OandaCredentials::new("test", "test-account");

        let practice = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaPractice);
        assert_eq!(practice.stream_url, OANDA_PRACTICE_STREAM_URL);

        let live = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaLive);
        assert_eq!(live.stream_url, OANDA_LIVE_STREAM_URL);
    }

    #[test]
    fn custom_stream_url_override() {
        let creds =
            OandaCredentials::new("test", "test-account").with_stream_url("http://localhost:9999");

        let provider = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaPractice);
        assert_eq!(provider.stream_url, "http://localhost:9999");
    }
}
