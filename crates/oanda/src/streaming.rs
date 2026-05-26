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
use chrono::{DateTime, Timelike, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::adapter::OandaAdapter;
use super::types::{
    OandaCandle, OandaCandlesResponse, OandaPriceStreamMessage, OandaTransactionStreamLine,
};
use crate::credentials::OandaCredentials;
use tektii_gateway_core::models::{
    Bar, Order, OrderStatus, OrderType, Quote, RejectReason, Side, TimeInForce, Timeframe,
    TradingPlatform,
};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{
    EventAckMessage, InternalTradingEvent, OrderEventType, WsMessage,
};
use tektii_gateway_core::websocket::provider::{
    EventStream, ProviderConfig, ProviderEvent, WebSocketProvider,
};

// ============================================================================
// Constants
// ============================================================================

/// Base URL for Oanda practice streaming endpoints.
const OANDA_PRACTICE_STREAM_URL: &str = "https://stream-fxpractice.oanda.com";
/// Base URL for Oanda live streaming endpoints.
const OANDA_LIVE_STREAM_URL: &str = "https://stream-fxtrade.oanda.com";

/// Base URL for Oanda practice REST endpoints (candles live here, NOT the stream host).
const OANDA_PRACTICE_REST_URL: &str = "https://api-fxpractice.oanda.com";
/// Base URL for Oanda live REST endpoints.
const OANDA_LIVE_REST_URL: &str = "https://api-fxtrade.oanda.com";

/// Granularity requested for the live candle poll (matches `Timeframe::OneMinute`).
const CANDLE_GRANULARITY: &str = "M1";
/// Number of candles fetched per poll. The in-progress (`complete=false`) candle plus a
/// couple of closed ones, so a single missed poll (process pause, late finalize) is
/// recovered next poll instead of dropping a bar -- important for the canary's completeness.
const CANDLE_POLL_COUNT: u32 = 3;
/// Seconds to wait past each minute boundary before polling, giving Oanda time to
/// finalize the candle that just closed.
const CANDLE_POLL_OFFSET_SECS: i64 = 3;

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
    /// Base URL for REST endpoints (candle polling -- candles are not streamed).
    rest_url: String,
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
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    /// Top-level cancellation token (cancels all streams).
    cancel_token: CancellationToken,
    /// Separate token for price stream -- cancelled on subscribe/unsubscribe to
    /// respawn with updated instrument list.
    price_stream_cancel: Arc<RwLock<CancellationToken>>,
    /// Separate token for the candle poll -- cancelled-and-replaced on each spawn so a
    /// reconnect (which re-runs `connect()`) does not leave a duplicate poll task running.
    candle_poll_cancel: Arc<RwLock<CancellationToken>>,
    /// Separate token for the transaction stream -- cancelled-and-replaced on each spawn so a
    /// reconnect (which re-runs `connect()`) does not leave a duplicate stream task running and
    /// emit duplicate order events on the strategy channel (TEK-601).
    transaction_stream_cancel: Arc<RwLock<CancellationToken>>,
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

        // Candles live on the REST host, derived the same way the adapter does.
        let rest_url = credentials
            .rest_url
            .clone()
            .unwrap_or_else(|| match platform {
                TradingPlatform::OandaLive => OANDA_LIVE_REST_URL.to_string(),
                _ => OANDA_PRACTICE_REST_URL.to_string(),
            });

        Self {
            stream_url,
            rest_url,
            api_token: tektii_gateway_core::arc_secret(&credentials.api_token),
            account_id: credentials.account_id.clone(),
            platform,
            instruments: Arc::new(RwLock::new(Vec::new())),
            event_tx: Arc::new(RwLock::new(None)),
            cancel_token: CancellationToken::new(),
            stored_config: Arc::new(RwLock::new(None)),
            price_stream_cancel: Arc::new(RwLock::new(CancellationToken::new())),
            candle_poll_cancel: Arc::new(RwLock::new(CancellationToken::new())),
            transaction_stream_cancel: Arc::new(RwLock::new(CancellationToken::new())),
            client: Client::new(),
        }
    }

    /// Adopt a shared outbound event sender so the matching `OandaAdapter` can
    /// publish events (e.g. REST-sourced order fills that Oanda's transaction
    /// stream never delivers) onto the same stream this provider feeds to
    /// strategy clients. Must be called before `connect`, which writes the
    /// active sender into this shared handle.
    #[must_use]
    pub fn with_event_tx(
        mut self,
        event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    ) -> Self {
        self.event_tx = event_tx;
        self
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

    /// Spawn (or respawn) the transaction stream task.
    ///
    /// Cancel-and-replace mirrors `spawn_price_stream` / `spawn_candle_poll`: each call
    /// cancels the previous task before spawning a new one, so a reconnect (which re-runs
    /// `connect()`) does not leave the old generation running. Without this, both old and
    /// new tasks read the same `event_tx` (repointed by `connect()`) and every order event
    /// is delivered N+1 times after N reconnects (TEK-601). The stream loop also stops on
    /// the top-level `cancel_token` (disconnect).
    async fn spawn_transaction_stream(&self) {
        // Cancel any existing transaction stream, then install a fresh token.
        let new_cancel = {
            let mut guard = self.transaction_stream_cancel.write().await;
            guard.cancel();
            *guard = CancellationToken::new();
            guard.clone()
        };

        let parent_cancel = self.cancel_token.clone();
        let event_tx = self.event_tx.clone();
        let client = self.client.clone();
        let stream_url = self.stream_url.clone();
        let api_token = Arc::clone(&self.api_token);
        let account_id = self.account_id.clone();
        let platform = self.platform;

        tokio::spawn(async move {
            run_transaction_stream(
                &client,
                &stream_url,
                api_token.expose_secret(),
                &account_id,
                platform,
                event_tx,
                new_cancel,
                parent_cancel,
            )
            .await;
        });
    }

    /// Spawn (or respawn) the M1 candle poll task with the current instrument list.
    ///
    /// Reads the shared instrument list at the top of every iteration, so it never needs
    /// respawning on subscribe/unsubscribe. It is cancelled-and-replaced here (like the
    /// price stream) so that a reconnect -- which re-runs `connect()` -- replaces the old
    /// poll task rather than leaving a duplicate running. It also stops on the top-level
    /// `cancel_token` (disconnect).
    async fn spawn_candle_poll(&self) {
        // Cancel any existing candle poll, then install a fresh token.
        let new_cancel = {
            let mut guard = self.candle_poll_cancel.write().await;
            guard.cancel();
            *guard = CancellationToken::new();
            guard.clone()
        };

        let instruments = Arc::clone(&self.instruments);
        let parent_cancel = self.cancel_token.clone();
        let event_tx = self.event_tx.clone();
        let client = self.client.clone();
        let rest_url = self.rest_url.clone();
        let api_token = Arc::clone(&self.api_token);
        let platform = self.platform;

        tokio::spawn(async move {
            run_candle_poll(
                &client,
                &rest_url,
                api_token.expose_secret(),
                instruments,
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

        // Spawn the candle poll only if the strategy asked for candles -- Oanda has no
        // candle stream, so we REST-poll /candles on the minute boundary (see TEK-599).
        if wants_candles(&config.event_types) {
            self.spawn_candle_poll().await;
        }

        // Spawn transaction stream (cancel-and-replace so reconnect does not duplicate -- TEK-601).
        self.spawn_transaction_stream().await;

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
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
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
    event_tx: &Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
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
    event_tx: &Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
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
                        let _ = tx.send(msg.into());
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
// Candle poll
// ============================================================================

/// Whether the requested event types include candles/bars.
///
/// Mirrors the Alpaca adapter's mapping so subscriptions behave consistently across
/// providers. Gates the candle poll so we don't run a 60s loop nobody consumes.
fn wants_candles(event_types: &[String]) -> bool {
    event_types.iter().any(|e| {
        let e = e.to_ascii_lowercase();
        matches!(e.as_str(), "candle" | "candles" | "bar" | "bars") || e.starts_with("candle_")
    })
}

/// Start of the next minute after `now` (sub-minute components truncated).
fn next_minute_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    let truncated = now
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    truncated + chrono::Duration::minutes(1)
}

/// Map an Oanda candle to a gateway [`Bar`], or `None` if it lacks mid prices.
///
/// Mirrors `OandaAdapter::get_bars` (mid OHLC, tick volume) so live candles are
/// identical to historical ones. The provider string intentionally differs: this uses
/// `platform.header_value()` (e.g. "oanda-practice") for consistency with the streamed
/// quotes/orders, whereas `get_bars` hardcodes "oanda". Strategies key off OHLC, not the
/// provider label, so this divergence is deliberate -- do not "fix" it.
///
/// Price/volume basis (TEK-599): uses **mid** OHLC (Oanda's `/candles` default, `price=M`)
/// and Oanda's **tick volume** (count of price ticks, not traded notional -- forex is OTC
/// with no consolidated volume). This matches both `get_bars` and the engine's single-series
/// `Candle` (`crates/tektii/src/websocket.rs`), keeping the canary's live-vs-backtest
/// comparison apples-to-apples *provided the engine's forex dataset is also mid + tick volume*
/// (a canary-dataset concern owned by the repoint, TEK-597).
fn candle_to_bar(symbol: &str, candle: &OandaCandle, platform: TradingPlatform) -> Option<Bar> {
    let mid = candle.mid.as_ref()?;
    let timestamp = DateTime::parse_from_rfc3339(&candle.time)
        .ok()?
        .with_timezone(&Utc);

    Some(Bar {
        symbol: symbol.to_string(),
        provider: platform.header_value().to_string(),
        timeframe: Timeframe::OneMinute,
        open: Decimal::from_str(&mid.o).unwrap_or_default(),
        high: Decimal::from_str(&mid.h).unwrap_or_default(),
        low: Decimal::from_str(&mid.l).unwrap_or_default(),
        close: Decimal::from_str(&mid.c).unwrap_or_default(),
        volume: Decimal::from(candle.volume),
        timestamp,
    })
}

/// Failure mode of a candle poll.
#[derive(Debug)]
enum CandlePollError {
    /// Permanent auth failure (401/403) -- the poll task should stop, matching how the
    /// price/transaction streams treat `PermanentAuth`.
    Auth(String),
    /// Transient error (network, 5xx, decode) -- log and retry on the next boundary.
    Transient(String),
}

impl std::fmt::Display for CandlePollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(msg) | Self::Transient(msg) => write!(f, "{msg}"),
        }
    }
}

/// Fetch recent M1 candles for an instrument and return the **closed** ones, oldest-first.
///
/// Requests `CANDLE_POLL_COUNT` candles so a single missed poll is recoverable; the
/// in-progress (`complete=false`) candle is filtered out. Oanda returns candles oldest->newest,
/// so the filtered result is already ascending.
async fn poll_candles(
    client: &Client,
    rest_url: &str,
    api_token: &str,
    symbol: &str,
) -> Result<Vec<OandaCandle>, CandlePollError> {
    let url = format!(
        "{rest_url}/v3/instruments/{symbol}/candles?granularity={CANDLE_GRANULARITY}&count={CANDLE_POLL_COUNT}"
    );

    let response = client
        .get(&url)
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| CandlePollError::Transient(e.to_string()))?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(CandlePollError::Auth(format!("HTTP {status}")));
    }
    if !status.is_success() {
        return Err(CandlePollError::Transient(format!("HTTP {status}")));
    }

    let resp: OandaCandlesResponse = response
        .json()
        .await
        .map_err(|e| CandlePollError::Transient(e.to_string()))?;

    Ok(resp.candles.into_iter().filter(|c| c.complete).collect())
}

/// Poll every instrument once and emit any newly-closed candles.
///
/// Timer-free so it can be tested directly against a mock server. `last_emitted` is the
/// per-instrument watermark (timestamp of the last emitted candle) carried across calls:
///
/// * First poll for a symbol: emit only the **newest** closed candle, so startup does not
///   replay historical candles.
/// * Later polls: emit every closed candle strictly newer than the watermark, in order, so a
///   single missed boundary recovers the skipped bar(s) rather than dropping them. A candle
///   returned repeatedly (quiet/closed market) is suppressed because it is not past the watermark.
///
/// Returns `false` if a permanent auth failure was hit, signalling the caller to stop polling.
async fn poll_and_emit(
    client: &Client,
    rest_url: &str,
    api_token: &str,
    symbols: &[String],
    platform: TradingPlatform,
    event_tx: &Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    last_emitted: &mut HashMap<String, DateTime<Utc>>,
) -> bool {
    for symbol in symbols {
        let candles = match poll_candles(client, rest_url, api_token, symbol).await {
            Ok(candles) => candles,
            Err(CandlePollError::Auth(msg)) => {
                error!(
                    platform = %platform,
                    symbol = %symbol,
                    error = %msg,
                    "Candle poll auth failure -- stopping candle poll"
                );
                return false;
            }
            Err(CandlePollError::Transient(msg)) => {
                warn!(
                    platform = %platform,
                    symbol = %symbol,
                    error = %msg,
                    "Candle poll error -- will retry next minute"
                );
                continue;
            }
        };

        // Map closed candles to bars (ascending), then select those past the watermark.
        let bars: Vec<Bar> = candles
            .iter()
            .filter_map(|c| candle_to_bar(symbol, c, platform))
            .collect();
        let new_bars: Vec<Bar> = match last_emitted.get(symbol).copied() {
            None => bars.into_iter().next_back().into_iter().collect(),
            Some(watermark) => bars
                .into_iter()
                .filter(|b| b.timestamp > watermark)
                .collect(),
        };

        if new_bars.is_empty() {
            continue;
        }

        let tx_guard = event_tx.read().await;
        if let Some(tx) = tx_guard.as_ref() {
            for bar in new_bars {
                last_emitted.insert(symbol.clone(), bar.timestamp);
                let _ = tx.send(ProviderEvent::live(WsMessage::candle(bar)));
            }
        }
    }

    true
}

/// Poll Oanda's `/candles` endpoint on the minute boundary and emit `WsMessage::Candle`.
///
/// Boundary-aligned: each iteration recomputes the next minute from `Utc::now()` (so the
/// schedule self-corrects against poll latency) and wakes a few seconds past it, by which
/// point the just-closed candle is finalized. Transient errors are logged and retried next
/// minute (the 60s cadence is its own backoff); a permanent auth failure stops the task.
/// Also stops on `poll_cancel` (respawn) or `parent_cancel` (disconnect).
#[allow(clippy::too_many_arguments)]
async fn run_candle_poll(
    client: &Client,
    rest_url: &str,
    api_token: &str,
    instruments: Arc<RwLock<Vec<String>>>,
    platform: TradingPlatform,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    poll_cancel: CancellationToken,
    parent_cancel: CancellationToken,
) {
    let mut last_emitted: HashMap<String, DateTime<Utc>> = HashMap::new();

    loop {
        let now = Utc::now();
        let wake_at =
            next_minute_boundary(now) + chrono::Duration::seconds(CANDLE_POLL_OFFSET_SECS);
        let sleep_dur = (wake_at - now).to_std().unwrap_or(Duration::ZERO);

        tokio::select! {
            () = tokio::time::sleep(sleep_dur) => {}
            () = poll_cancel.cancelled() => {
                debug!(platform = %platform, "Candle poll cancelled (respawn)");
                return;
            }
            () = parent_cancel.cancelled() => {
                debug!(platform = %platform, "Candle poll cancelled (disconnect)");
                return;
            }
        }

        let symbols = instruments.read().await.clone();
        if symbols.is_empty() {
            continue;
        }

        let keep_polling = poll_and_emit(
            client,
            rest_url,
            api_token,
            &symbols,
            platform,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        if !keep_polling {
            return;
        }
    }
}

// ============================================================================
// Transaction stream
// ============================================================================

/// Run the transaction stream with reconnection (strategy-facing channel).
///
/// `stream_cancel` is the per-spawn token (cancel-and-replace on reconnect, TEK-601);
/// `parent_cancel` is the top-level disconnect token. Either firing terminates the loop.
#[allow(clippy::too_many_arguments)]
async fn run_transaction_stream(
    client: &Client,
    stream_url: &str,
    api_token: &str,
    account_id: &str,
    platform: TradingPlatform,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    stream_cancel: CancellationToken,
    parent_cancel: CancellationToken,
) {
    let url = format!("{stream_url}/v3/accounts/{account_id}/transactions/stream");
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let result = run_single_transaction_stream(
            client,
            &url,
            api_token,
            platform,
            &stream_cancel,
            &parent_cancel,
        )
        .await;

        // Cancelled / PermanentAuth return BEFORE the buffered-message flush below,
        // intentionally dropping `result.1`. On reconnect-driven cancel this prevents the
        // old generation from emitting its buffered events onto the new generation's
        // `event_tx` (which `connect()` has already repointed). Only Network /
        // HeartbeatTimeout fall through to the flush, since those are within the same
        // generation (the loop retries with the same `event_tx`).
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
                let _ = tx.send(msg.into());
            }
        }
        drop(tx_guard);

        tokio::select! {
            () = tokio::time::sleep(backoff) => {},
            () = stream_cancel.cancelled() => return,
            () = parent_cancel.cancelled() => return,
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
        // Registry owns a single lifecycle token here, so both select arms collapse onto it.
        let result = run_single_transaction_stream(
            client,
            &url,
            api_token,
            platform,
            &cancel_token,
            &cancel_token,
        )
        .await;

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
///
/// Selects on both `stream_cancel` (per-spawn, fires on respawn) and `parent_cancel`
/// (top-level, fires on disconnect) so the old generation always terminates.
async fn run_single_transaction_stream(
    client: &Client,
    url: &str,
    api_token: &str,
    platform: TradingPlatform,
    stream_cancel: &CancellationToken,
    parent_cancel: &CancellationToken,
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
            () = stream_cancel.cancelled() => {
                return (StreamError::Cancelled, messages);
            },
            () = parent_cancel.cancelled() => {
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
    use tektii_gateway_test_support::wiremock_helpers::{mount_json, start_mock_server};

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

    // ------------------------------------------------------------------
    // Candle poll (TEK-599)
    // ------------------------------------------------------------------

    #[test]
    fn default_rest_urls() {
        let creds = OandaCredentials::new("test", "test-account");

        let practice = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaPractice);
        assert_eq!(practice.rest_url, OANDA_PRACTICE_REST_URL);

        let live = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaLive);
        assert_eq!(live.rest_url, OANDA_LIVE_REST_URL);
    }

    #[test]
    fn custom_rest_url_override() {
        let creds =
            OandaCredentials::new("test", "test-account").with_rest_url("http://localhost:8888");

        let provider = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaPractice);
        assert_eq!(provider.rest_url, "http://localhost:8888");
    }

    #[test]
    fn wants_candles_matrix() {
        assert!(wants_candles(&["candle".to_string()]));
        assert!(wants_candles(&["candles".to_string()]));
        assert!(wants_candles(&["bar".to_string()]));
        assert!(wants_candles(&["bars".to_string()]));
        assert!(wants_candles(&["candle_1m".to_string()]));
        assert!(wants_candles(&["quote".to_string(), "CANDLE".to_string()]));

        assert!(!wants_candles(&["quote".to_string()]));
        assert!(!wants_candles(&[]));
    }

    #[test]
    fn next_minute_boundary_truncates() {
        let now = DateTime::parse_from_rfc3339("2024-01-15T10:30:42.5Z")
            .unwrap()
            .with_timezone(&Utc);
        let boundary = next_minute_boundary(now);
        assert_eq!(
            boundary,
            DateTime::parse_from_rfc3339("2024-01-15T10:31:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    /// Build an `OandaCandle` JSON value for tests.
    fn candle_value(time: &str, complete: bool) -> serde_json::Value {
        serde_json::json!({
            "time": time,
            "mid": { "o": "1.10000", "h": "1.10500", "l": "1.09500", "c": "1.10200" },
            "volume": 1000,
            "complete": complete,
        })
    }

    #[test]
    fn candle_to_bar_matches_get_bars_mapping() {
        let candle: OandaCandle =
            serde_json::from_value(candle_value("2024-01-15T10:00:00.000000000Z", true)).unwrap();

        let bar = candle_to_bar("EUR_USD", &candle, TradingPlatform::OandaPractice).unwrap();

        assert_eq!(bar.symbol, "EUR_USD");
        assert_eq!(bar.timeframe, Timeframe::OneMinute);
        assert_eq!(bar.open, Decimal::from_str("1.10000").unwrap());
        assert_eq!(bar.high, Decimal::from_str("1.10500").unwrap());
        assert_eq!(bar.low, Decimal::from_str("1.09500").unwrap());
        assert_eq!(bar.close, Decimal::from_str("1.10200").unwrap());
        assert_eq!(bar.volume, Decimal::from(1000));
        assert_eq!(
            bar.timestamp,
            DateTime::parse_from_rfc3339("2024-01-15T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn candle_to_bar_skips_missing_mid() {
        let candle: OandaCandle = serde_json::from_value(serde_json::json!({
            "time": "2024-01-15T10:00:00.000000000Z",
            "volume": 1000,
            "complete": true,
        }))
        .unwrap();

        assert!(candle_to_bar("EUR_USD", &candle, TradingPlatform::OandaPractice).is_none());
    }

    #[test]
    fn candle_to_bar_provider_is_header_value() {
        let candle: OandaCandle =
            serde_json::from_value(candle_value("2024-01-15T10:00:00.000000000Z", true)).unwrap();

        let practice = candle_to_bar("EUR_USD", &candle, TradingPlatform::OandaPractice).unwrap();
        assert_eq!(practice.provider, "oanda-practice");

        let live = candle_to_bar("EUR_USD", &candle, TradingPlatform::OandaLive).unwrap();
        assert_eq!(live.provider, "oanda-live");
    }

    #[tokio::test]
    async fn poll_candles_fetches_and_maps() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({ "candles": [candle_value("2024-01-15T10:00:00.000000000Z", true)] }),
        )
        .await;

        let candles = poll_candles(&Client::new(), &base_url, "test-token", "EUR_USD")
            .await
            .unwrap();

        assert_eq!(candles.len(), 1);
        let bar = candle_to_bar("EUR_USD", &candles[0], TradingPlatform::OandaPractice).unwrap();
        assert_eq!(bar.close, Decimal::from_str("1.10200").unwrap());
    }

    #[tokio::test]
    async fn poll_candles_filters_incomplete_keeps_ascending() {
        // Response: a closed candle followed by the in-progress (incomplete) minute.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({
                "candles": [
                    candle_value("2024-01-15T10:00:00.000000000Z", true),
                    candle_value("2024-01-15T10:01:00.000000000Z", false),
                ]
            }),
        )
        .await;

        let candles = poll_candles(&Client::new(), &base_url, "test-token", "EUR_USD")
            .await
            .unwrap();

        assert_eq!(candles.len(), 1, "incomplete candle must be filtered out");
        assert_eq!(candles[0].time, "2024-01-15T10:00:00.000000000Z");
    }

    #[tokio::test]
    async fn poll_candles_empty_when_none_complete() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({ "candles": [candle_value("2024-01-15T10:01:00.000000000Z", false)] }),
        )
        .await;

        let candles = poll_candles(&Client::new(), &base_url, "test-token", "EUR_USD")
            .await
            .unwrap();
        assert!(
            candles.is_empty(),
            "no closed candle should yield empty vec"
        );
    }

    #[tokio::test]
    async fn poll_candles_server_error_is_transient() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            500,
            serde_json::json!({ "errorMessage": "boom" }),
        )
        .await;

        let err = poll_candles(&Client::new(), &base_url, "test-token", "EUR_USD")
            .await
            .unwrap_err();
        assert!(
            matches!(err, CandlePollError::Transient(ref m) if m.contains("500")),
            "expected Transient HTTP 500, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn poll_candles_unauthorized_is_auth() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            401,
            serde_json::json!({ "errorMessage": "bad token" }),
        )
        .await;

        let err = poll_candles(&Client::new(), &base_url, "test-token", "EUR_USD")
            .await
            .unwrap_err();
        assert!(
            matches!(err, CandlePollError::Auth(_)),
            "401 should map to Auth, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn poll_and_emit_emits_candle_then_dedups() {
        // The mock always returns the same closed candle. The first poll must emit it;
        // a second poll (same timestamp) must be suppressed by the dedup map.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({ "candles": [candle_value("2024-01-15T10:00:00.000000000Z", true)] }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let symbols = vec!["EUR_USD".to_string()];
        let mut last_emitted = HashMap::new();
        let client = Client::new();

        // First poll: one candle emitted.
        poll_and_emit(
            &client,
            &base_url,
            "test-token",
            &symbols,
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        let event = rx.try_recv().expect("first poll should emit a candle");
        match event.msg {
            WsMessage::Candle { bar, .. } => {
                assert_eq!(bar.symbol, "EUR_USD");
                assert_eq!(bar.close, Decimal::from_str("1.10200").unwrap());
                assert_eq!(bar.provider, "oanda-practice");
            }
            other => panic!("expected Candle, got {other:?}"),
        }

        // Second poll: same candle, dedup suppresses it.
        poll_and_emit(
            &client,
            &base_url,
            "test-token",
            &symbols,
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "dedup should suppress re-emitting the same candle"
        );
    }

    #[tokio::test]
    async fn poll_and_emit_no_candle_when_incomplete() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({ "candles": [candle_value("2024-01-15T10:01:00.000000000Z", false)] }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let mut last_emitted = HashMap::new();

        poll_and_emit(
            &Client::new(),
            &base_url,
            "test-token",
            &["EUR_USD".to_string()],
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "no closed candle should produce no message"
        );
    }

    #[tokio::test]
    async fn poll_and_emit_first_poll_emits_only_newest() {
        // On the first poll for a symbol, only the newest closed candle is emitted -- older
        // candles in the response are NOT replayed as history.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({
                "candles": [
                    candle_value("2024-01-15T10:00:00.000000000Z", true),
                    candle_value("2024-01-15T10:01:00.000000000Z", true),
                ]
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let mut last_emitted = HashMap::new();

        let ok = poll_and_emit(
            &Client::new(),
            &base_url,
            "test-token",
            &["EUR_USD".to_string()],
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;
        assert!(ok);

        let event = rx.try_recv().expect("should emit the newest candle");
        match event.msg {
            WsMessage::Candle { bar, .. } => {
                assert_eq!(
                    bar.timestamp,
                    DateTime::parse_from_rfc3339("2024-01-15T10:01:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                    "only the newest candle should be emitted on first poll"
                );
            }
            other => panic!("expected Candle, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "older candle must not be replayed");
    }

    #[tokio::test]
    async fn poll_and_emit_recovers_candles_after_watermark() {
        // With an existing watermark, every closed candle strictly newer than it is emitted
        // in order -- recovering bars skipped by a missed poll.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({
                "candles": [
                    candle_value("2024-01-15T10:00:00.000000000Z", true),
                    candle_value("2024-01-15T10:01:00.000000000Z", true),
                    candle_value("2024-01-15T10:02:00.000000000Z", true),
                ]
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        // Watermark at 10:00 -- the gateway already emitted that minute.
        let mut last_emitted = HashMap::from([(
            "EUR_USD".to_string(),
            DateTime::parse_from_rfc3339("2024-01-15T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )]);

        poll_and_emit(
            &Client::new(),
            &base_url,
            "test-token",
            &["EUR_USD".to_string()],
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        let mut times: Vec<String> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let WsMessage::Candle { bar, .. } = ev.msg {
                times.push(bar.timestamp.to_rfc3339());
            }
        }
        assert_eq!(
            times,
            vec![
                "2024-01-15T10:01:00+00:00".to_string(),
                "2024-01-15T10:02:00+00:00".to_string(),
            ],
            "should emit both candles past the watermark, in ascending order"
        );
    }

    #[tokio::test]
    async fn poll_and_emit_stops_on_auth_failure() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            403,
            serde_json::json!({ "errorMessage": "forbidden" }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let mut last_emitted = HashMap::new();

        let ok = poll_and_emit(
            &Client::new(),
            &base_url,
            "test-token",
            &["EUR_USD".to_string()],
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        assert!(!ok, "auth failure should signal the poll to stop");
        assert!(rx.try_recv().is_err(), "no candle on auth failure");
    }

    #[tokio::test]
    async fn poll_and_emit_dedup_is_per_symbol() {
        // Two symbols with the SAME candle timestamp must BOTH emit -- proving the dedup
        // map is keyed per symbol, not a single shared watermark.
        let (server, base_url) = start_mock_server().await;
        let ts = "2024-01-15T10:00:00.000000000Z";
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({ "candles": [candle_value(ts, true)] }),
        )
        .await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/GBP_USD/candles",
            200,
            serde_json::json!({ "candles": [candle_value(ts, true)] }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let symbols = vec!["EUR_USD".to_string(), "GBP_USD".to_string()];
        let mut last_emitted = HashMap::new();
        let client = Client::new();

        poll_and_emit(
            &client,
            &base_url,
            "test-token",
            &symbols,
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        let mut got: Vec<String> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let WsMessage::Candle { bar, .. } = ev.msg {
                got.push(bar.symbol);
            }
        }
        got.sort();
        assert_eq!(
            got,
            vec!["EUR_USD".to_string(), "GBP_USD".to_string()],
            "both symbols should emit despite identical timestamps"
        );

        // Second pass: both deduped.
        poll_and_emit(
            &client,
            &base_url,
            "test-token",
            &symbols,
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;
        assert!(
            rx.try_recv().is_err(),
            "second pass should dedup both symbols"
        );
    }

    #[tokio::test]
    async fn run_candle_poll_returns_on_cancel() {
        // Pre-cancel the poll token: the first `select!` must take the cancel arm and
        // return immediately, without waiting for the minute boundary.
        let poll_cancel = CancellationToken::new();
        poll_cancel.cancel();
        let parent_cancel = CancellationToken::new();

        let instruments = Arc::new(RwLock::new(vec!["EUR_USD".to_string()]));
        let (tx, _rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));

        let res = tokio::time::timeout(
            Duration::from_secs(5),
            run_candle_poll(
                &Client::new(),
                "http://127.0.0.1:1",
                "test-token",
                instruments,
                TradingPlatform::OandaPractice,
                event_tx,
                poll_cancel,
                parent_cancel,
            ),
        )
        .await;

        assert!(
            res.is_ok(),
            "run_candle_poll should return promptly when cancelled"
        );
    }

    // ------------------------------------------------------------------
    // Transaction stream lifecycle (TEK-601)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn reconnect_cancels_previous_transaction_stream_token() {
        // Regression for TEK-601: on the old code, `connect()` spawned the transaction
        // stream with the top-level `cancel_token`, so a reconnect (which re-runs
        // `connect()`) left the previous task alive and every order event was
        // delivered N+1 times after N reconnects. The fix gives the transaction stream
        // its own cancel-and-replace token. This test pins that invariant by asserting
        // the gen-0 token is cancelled after `reconnect()` and a fresh, live gen-1
        // token replaces it.
        //
        // Endpoints are pointed at an unmounted mock so the spawned stream tasks fail
        // fast with 404s and never produce data -- the lifecycle assertions are on the
        // provider's tokens, not the tasks' output.
        let (_server, base_url) = start_mock_server().await;
        let creds = OandaCredentials::new("test-token", "test-account")
            .with_stream_url(&base_url)
            .with_rest_url(&base_url);
        let provider = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaPractice);

        let config = ProviderConfig {
            platform: TradingPlatform::OandaPractice,
            symbols: vec!["EUR_USD".to_string()],
            event_types: vec!["quote".to_string()],
            credentials: None,
            tektii_params: None,
        };

        let _rx_gen0 = provider
            .connect(config)
            .await
            .expect("connect should succeed");
        let gen0_token = provider.transaction_stream_cancel.read().await.clone();
        assert!(
            !gen0_token.is_cancelled(),
            "gen-0 transaction-stream token should be live after connect"
        );

        let _rx_gen1 = provider
            .reconnect()
            .await
            .expect("reconnect should succeed");
        assert!(
            gen0_token.is_cancelled(),
            "reconnect must cancel the previous transaction-stream token (TEK-601)"
        );

        let gen1_token = provider.transaction_stream_cancel.read().await.clone();
        assert!(
            !gen1_token.is_cancelled(),
            "gen-1 transaction-stream token should be live after reconnect"
        );

        // Cleanly tear down spawned tasks.
        provider.disconnect().await.expect("disconnect");
    }
}
