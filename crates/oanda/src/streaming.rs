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
use super::dedupe::{PUBLISHED_TRANSACTIONS, PublishedTransactions};
use super::types::{
    OandaAccountResponse, OandaCandle, OandaCandlesResponse, OandaPriceStreamMessage,
    OandaTransactionStreamLine,
};
use crate::credentials::OandaCredentials;
use tektii_gateway_core::models::{
    Account, Bar, Order, OrderStatus, OrderType, Quote, RejectReason, Side, TimeInForce, Timeframe,
    Trade, TradingPlatform,
};
use tektii_gateway_core::websocket::error::WebSocketError;
use tektii_gateway_core::websocket::messages::{
    AccountEventType, EventAckMessage, InternalTradingEvent, OrderEventType, WsMessage,
};
use tektii_gateway_core::websocket::provider::{
    EventStream, ProviderConfig, ProviderEvent, WebSocketProvider,
};

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
/// Upper bound on the per-fill account-summary fetch. The fetch is awaited inside
/// the transaction stream's chunk loop, so an unbounded hang (the shared client has
/// no default timeout) would stall heartbeat-timeout detection and reconnect/disconnect
/// cancellation. Kept below `HEARTBEAT_TIMEOUT` so a stalled fetch cannot defer a
/// reconnect by more than one fetch window; on timeout the snapshot is simply skipped.
pub(crate) const ACCOUNT_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum reconnection backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Initial reconnection backoff.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Consecutive failed candle-poll cycles tolerated before the poll gives up and stops.
/// A single transient OANDA blip (the failure this guards) recovers on the next attempt;
/// only a sustained run -- roughly three minutes once the backoff saturates at
/// `MAX_BACKOFF` -- indicates a real outage worth stopping for and surfacing loudly.
const MAX_CONSECUTIVE_CANDLE_POLL_FAILURES: u32 = 8;

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

/// Oanda HTTP streaming provider implementing [`WebSocketProvider`].
///
/// Wraps two concurrent HTTP chunked-transfer streams (pricing + transactions)
/// behind the standard WebSocket provider trait.
#[derive(Clone)]
pub struct OandaWebSocketProvider {
    /// Base URL for streaming endpoints.
    stream_url: String,
    /// Candles are not streamed -- polled from the REST host instead.
    rest_url: String,
    api_token: Arc<SecretBox<String>>,
    account_id: String,
    platform: TradingPlatform,
    /// Currently subscribed instruments in Oanda format (e.g., "EUR_USD").
    instruments: Arc<RwLock<Vec<String>>>,
    stored_config: Arc<RwLock<Option<ProviderConfig>>>,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    /// Top-level; cancels all streams. Per-stream tokens below cancel individually.
    cancel_token: CancellationToken,
    /// Separate token for price stream -- cancelled on subscribe/unsubscribe to
    /// respawn with updated instrument list.
    price_stream_cancel: Arc<RwLock<CancellationToken>>,
    /// Separate token for the candle poll -- cancelled-and-replaced on each spawn so a
    /// reconnect (which re-runs `connect()`) does not leave a duplicate poll task running.
    candle_poll_cancel: Arc<RwLock<CancellationToken>>,
    /// Separate token for the transaction stream -- cancelled-and-replaced on each spawn so a
    /// reconnect (which re-runs `connect()`) does not leave a duplicate stream task running and
    /// emit duplicate order events on the strategy channel.
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
    /// publish events (e.g. REST-sourced order fills) onto the same stream this
    /// provider feeds to strategy clients. The transaction stream delivers the
    /// same fills; the two paths are deduplicated by transaction id. Must be
    /// called before `connect`, which writes the active sender into this
    /// shared handle.
    #[must_use]
    pub fn with_event_tx(
        mut self,
        event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    ) -> Self {
        self.event_tx = event_tx;
        self
    }

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

    /// Spawn (or respawn) the price stream task with the current instrument list.
    async fn spawn_price_stream(&self) {
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
    /// is delivered N+1 times after N reconnects. The stream loop also stops on
    /// the top-level `cancel_token` (disconnect).
    async fn spawn_transaction_stream(&self) {
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
        let rest_url = self.rest_url.clone();
        let api_token = Arc::clone(&self.api_token);
        let account_id = self.account_id.clone();
        let platform = self.platform;

        tokio::spawn(async move {
            run_transaction_stream(
                &client,
                &stream_url,
                &rest_url,
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

#[async_trait]
impl WebSocketProvider for OandaWebSocketProvider {
    async fn connect(&self, config: ProviderConfig) -> Result<EventStream, WebSocketError> {
        *self.stored_config.write().await = Some(config.clone());

        self.instruments.write().await.clone_from(&config.symbols);

        let (tx, rx) = mpsc::unbounded_channel();
        *self.event_tx.write().await = Some(tx);

        self.spawn_price_stream().await;

        // Spawn the candle poll only if the strategy asked for candles -- Oanda has no
        // candle stream, so we REST-poll /candles on the minute boundary.
        if wants_candles(&config.event_types) {
            self.spawn_candle_poll().await;
        }

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
        // Belt-and-braces: cancel each per-task token in addition to the parent
        // cancel_token. Stream tasks today select! on both, but cancelling the
        // per-task tokens explicitly means a future spawn site that forgets to
        // wire in parent_cancel still terminates on disconnect.
        self.price_stream_cancel.read().await.cancel();
        self.candle_poll_cancel.read().await.cancel();
        self.transaction_stream_cancel.read().await.cancel();
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
/// Price/volume basis: uses **mid** OHLC (Oanda's `/candles` default, `price=M`)
/// and Oanda's **tick volume** (count of price ticks, not traded notional -- forex is OTC
/// with no consolidated volume). This matches both `get_bars` and the engine's single-series
/// `Candle` (`crates/tektii/src/websocket.rs`), keeping the canary's live-vs-backtest
/// comparison apples-to-apples *provided the engine's forex dataset is also mid + tick volume*
/// (a canary-dataset concern owned by the repoint).
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
/// Returns `true` if the cycle was healthy. Returns `false` if any symbol hit a retryable HTTP
/// failure (auth blip, 5xx, network, decode) -- the caller backs off and retries rather than
/// stopping. A 401/403 is treated as transient here: OANDA has been observed to return a
/// spurious 401 on the candle endpoint while the same token streams prices and reads the
/// account fine, so a momentary auth blip must not latch the candle feed off permanently.
async fn poll_and_emit(
    client: &Client,
    rest_url: &str,
    api_token: &str,
    symbols: &[String],
    platform: TradingPlatform,
    event_tx: &Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    last_emitted: &mut HashMap<String, DateTime<Utc>>,
) -> bool {
    let mut healthy = true;
    for symbol in symbols {
        let candles = match poll_candles(client, rest_url, api_token, symbol).await {
            Ok(candles) => candles,
            Err(CandlePollError::Auth(msg)) => {
                warn!(
                    platform = %platform,
                    symbol = %symbol,
                    error = %msg,
                    "Candle poll auth failure -- treating as transient, will retry with backoff"
                );
                healthy = false;
                continue;
            }
            Err(CandlePollError::Transient(msg)) => {
                warn!(
                    platform = %platform,
                    symbol = %symbol,
                    error = %msg,
                    "Candle poll error -- will retry with backoff"
                );
                healthy = false;
                continue;
            }
        };

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

    healthy
}

/// Poll Oanda's `/candles` endpoint on the minute boundary and emit `WsMessage::Candle`.
///
/// Boundary-aligned on the happy path: each iteration recomputes the next minute from
/// `Utc::now()` (so the schedule self-corrects against poll latency) and wakes a few seconds
/// past it, by which point the just-closed candle is finalized. After a failed cycle it
/// retries sooner on bounded exponential backoff (mirroring the transaction stream) so a
/// transient OANDA blip -- including a spurious 401 -- recovers within seconds instead of
/// stalling a full minute, and only gives up (with a loud `error!`) after
/// `MAX_CONSECUTIVE_CANDLE_POLL_FAILURES` consecutive failures, i.e. a sustained outage.
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
    let mut backoff = INITIAL_BACKOFF;
    let mut consecutive_failures: u32 = 0;

    loop {
        let sleep_dur = if consecutive_failures == 0 {
            let now = Utc::now();
            let wake_at =
                next_minute_boundary(now) + chrono::Duration::seconds(CANDLE_POLL_OFFSET_SECS);
            (wake_at - now).to_std().unwrap_or(Duration::ZERO)
        } else {
            backoff
        };

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

        let healthy = poll_and_emit(
            client,
            rest_url,
            api_token,
            &symbols,
            platform,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        if healthy {
            consecutive_failures = 0;
            backoff = INITIAL_BACKOFF;
        } else {
            consecutive_failures += 1;
            if consecutive_failures >= MAX_CONSECUTIVE_CANDLE_POLL_FAILURES {
                error!(
                    platform = %platform,
                    consecutive_failures,
                    "Candle poll failed for too many consecutive attempts -- stopping candle poll"
                );
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
}

/// Run the transaction stream with reconnection (strategy-facing channel).
///
/// `stream_cancel` is the per-spawn token (cancel-and-replace on reconnect);
/// `parent_cancel` is the top-level disconnect token. Either firing terminates the loop.
#[allow(clippy::too_many_arguments)]
async fn run_transaction_stream(
    client: &Client,
    stream_url: &str,
    rest_url: &str,
    api_token: &str,
    account_id: &str,
    platform: TradingPlatform,
    event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
    stream_cancel: CancellationToken,
    parent_cancel: CancellationToken,
) {
    let url = format!("{stream_url}/v3/accounts/{account_id}/transactions/stream");
    // The strategy-facing stream enriches each fill with an account snapshot
    // (summary lives on the REST host, not the stream host).
    let account_fetcher = AccountFetcher {
        client,
        rest_url,
        api_token,
        account_id,
        platform,
        timeout: ACCOUNT_FETCH_TIMEOUT,
    };
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let result = run_single_transaction_stream(
            client,
            &url,
            api_token,
            platform,
            &TransactionSink::Strategy {
                slot: &event_tx,
                dedupe: &PUBLISHED_TRANSACTIONS,
                account_fetcher: Some(&account_fetcher),
            },
            &stream_cancel,
            &parent_cancel,
        )
        .await;

        match &result {
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
            &TransactionSink::Internal(&internal_tx, platform),
            &cancel_token,
            &cancel_token,
        )
        .await;

        match &result {
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

        tokio::select! {
            () = tokio::time::sleep(backoff) => {},
            () = cancel_token.cancelled() => return,
        }

        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Destination for transaction-stream events, sent inline as lines are parsed.
enum TransactionSink<'a> {
    /// Strategy-facing provider channel. Shared with the adapter's REST
    /// paths, which publish the same fill/cancel transactions — `dedupe`
    /// arbitrates so each is delivered exactly once. Each `ORDER_FILL` is
    /// followed by one account snapshot from `account_fetcher` (when set).
    Strategy {
        slot: &'a Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>>,
        dedupe: &'a PublishedTransactions,
        account_fetcher: Option<&'a AccountFetcher<'a>>,
    },
    /// Internal EventRouter broadcast channel. No dedupe: the REST path
    /// publishes only to strategy clients, and sharing claims with the
    /// strategy stream would make the two streams compete for the same
    /// event (the EventRouter consumers self-guard). No account enrichment:
    /// the EventRouter synthesises positions from order events and never
    /// forwards account frames, so the fetch would be wasted.
    Internal(&'a broadcast::Sender<InternalTradingEvent>, TradingPlatform),
}

impl<'a> TransactionSink<'a> {
    /// Whether this transaction should be emitted, claiming its id when the
    /// REST path publishes the same transaction type. A disconnected strategy
    /// channel declines without claiming, so a dead channel never claims an
    /// event away from the REST path. (The slot can still be repointed
    /// between the claim and the send -- a narrow, accepted race: the slot is
    /// only rewritten on `connect`.)
    async fn claim_if_needed(&self, line: &OandaTransactionStreamLine) -> bool {
        let Self::Strategy { slot, dedupe, .. } = self else {
            return true;
        };
        if !rest_path_also_publishes(&line.transaction_type) {
            return true;
        }
        let Some(id) = line.id.as_deref() else {
            warn!(
                transaction_type = %line.transaction_type,
                "Transaction line carries no id -- emitting without dedupe"
            );
            return true;
        };
        if slot.read().await.is_none() {
            return false;
        }
        dedupe.try_claim(id)
    }

    const fn account_fetcher(&self) -> Option<&'a AccountFetcher<'a>> {
        match self {
            Self::Strategy {
                account_fetcher, ..
            } => *account_fetcher,
            Self::Internal(..) => None,
        }
    }

    async fn send(&self, msg: WsMessage) {
        match self {
            Self::Strategy { slot, .. } => {
                if let Some(tx) = slot.read().await.as_ref() {
                    let _ = tx.send(msg.into());
                }
            }
            Self::Internal(tx, platform) => {
                let _ = tx.send(InternalTradingEvent::new(msg, *platform));
            }
        }
    }
}

/// Run a single transaction stream connection, emitting events on `sink`
/// inline as they are parsed. Returns the reason the connection ended.
///
/// Selects on both `stream_cancel` (per-spawn, fires on respawn) and `parent_cancel`
/// (top-level, fires on disconnect) so the old generation always terminates.
async fn run_single_transaction_stream(
    client: &Client,
    url: &str,
    api_token: &str,
    platform: TradingPlatform,
    sink: &TransactionSink<'_>,
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
                        process_transaction_buffer(&mut buffer, platform, sink, stream_cancel, parent_cancel).await;
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

/// Transaction types the adapter's synchronous REST order path also publishes,
/// so the stream copy must win a dedupe claim before emitting.
fn rest_path_also_publishes(transaction_type: &str) -> bool {
    matches!(transaction_type, "ORDER_FILL" | "ORDER_CANCEL")
}

/// Process complete NDJSON lines from the buffer, sending events inline on
/// the sink as they are parsed (mirroring the price stream). See
/// [`TransactionSink`] for the per-sink dedupe and account-snapshot rules; a
/// lost claim suppresses the whole transaction -- its order event, derived
/// trade/financing frames, and account snapshot. Each `ORDER_FILL`'s snapshot
/// is sent inline so it is ordered immediately after the fill's own frames --
/// the canary attributes an account snapshot to the fill that precedes it on
/// the wire.
///
/// Bails between lines once either token fires, so a cancelled generation
/// (its `event_tx` already repointed to the new one) stops emitting mid-burst
/// instead of draining the rest of its buffer onto the new channel.
async fn process_transaction_buffer(
    buffer: &mut BytesMut,
    platform: TradingPlatform,
    sink: &TransactionSink<'_>,
    stream_cancel: &CancellationToken,
    parent_cancel: &CancellationToken,
) {
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        if stream_cancel.is_cancelled() || parent_cancel.is_cancelled() {
            return;
        }

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
                if !sink.claim_if_needed(&line).await {
                    debug!(
                        platform = %platform,
                        transaction_id = ?line.id,
                        transaction_type = %line.transaction_type,
                        "Transaction already published via the REST path (or no \
                         strategy channel) -- suppressing stream copy"
                    );
                    continue;
                }

                for msg in transaction_to_messages(&line, platform) {
                    sink.send(msg).await;
                }

                if line.transaction_type == "ORDER_FILL"
                    && let Some(fetcher) = sink.account_fetcher()
                    && let Some(account) = fetcher
                        .account_snapshot_for_fill(line.account_balance.as_deref())
                        .await
                {
                    sink.send(WsMessage::Account {
                        event: AccountEventType::BalanceUpdated,
                        account,
                        timestamp: parse_transaction_time(line.time.as_deref()),
                    })
                    .await;
                }
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

/// Fetches the OANDA account summary to enrich a per-fill account snapshot.
///
/// The streaming task has no `OandaAdapter` handle, so it does its own raw summary
/// fetch the same way [`poll_candles`] fetches candles. The summary->[`Account`]
/// mapping mirrors `OandaAdapter::get_account`. Shared with the adapter's REST
/// fill path: both paths deliver fills (deduped by transaction id), and
/// whichever publishes a fill also emits its snapshot.
pub(crate) struct AccountFetcher<'a> {
    pub(crate) client: &'a Client,
    pub(crate) rest_url: &'a str,
    pub(crate) api_token: &'a str,
    pub(crate) account_id: &'a str,
    pub(crate) platform: TradingPlatform,
    /// Bound on the summary fetch ([`ACCOUNT_FETCH_TIMEOUT`] in production).
    pub(crate) timeout: Duration,
}

impl AccountFetcher<'_> {
    /// Fetch the account summary and map it to a core [`Account`].
    ///
    /// Returns `None` on any error (network, non-2xx, decode). A missing snapshot is
    /// preferable to one carrying bogus zeros for margin/equity: the canary attributes
    /// each snapshot to the fill that precedes it on the wire, so a dropped snapshot
    /// leaves a gap rather than mispairing later fills.
    async fn fetch(&self) -> Option<Account> {
        let url = format!("{}/v3/accounts/{}/summary", self.rest_url, self.account_id);
        let response = match self
            .client
            .get(&url)
            .bearer_auth(self.api_token)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                warn!(
                    platform = %self.platform,
                    error = %e,
                    "Account summary fetch failed -- skipping account snapshot"
                );
                return None;
            }
        };

        let status = response.status();
        if !status.is_success() {
            warn!(
                platform = %self.platform,
                %status,
                "Account summary fetch returned non-success -- skipping account snapshot"
            );
            return None;
        }

        let parsed: OandaAccountResponse = match response.json().await {
            Ok(parsed) => parsed,
            Err(e) => {
                warn!(
                    platform = %self.platform,
                    error = %e,
                    "Account summary decode failed -- skipping account snapshot"
                );
                return None;
            }
        };

        let acct = parsed.account;
        // Unlike `adapter::get_account` -- which must return an `Account` for the REST
        // route and so defaults unparseable fields to zero -- the streaming path can skip
        // the snapshot. A non-numeric field would otherwise emit a bogus zero for
        // equity/margin, so any parse failure is treated as a missing snapshot.
        let (Ok(balance), Ok(equity), Ok(margin_used), Ok(margin_available), Ok(unrealized_pnl)) = (
            Decimal::from_str(&acct.balance),
            Decimal::from_str(&acct.nav),
            Decimal::from_str(&acct.margin_used),
            Decimal::from_str(&acct.margin_available),
            Decimal::from_str(&acct.unrealized_pl),
        ) else {
            warn!(
                platform = %self.platform,
                "Account summary had an unparseable numeric field -- skipping account snapshot"
            );
            return None;
        };

        Some(Account {
            balance,
            equity,
            margin_used,
            margin_available,
            unrealized_pnl,
            currency: acct.currency,
        })
    }

    /// Build the per-fill account snapshot: the fetched summary with `balance`
    /// overridden by `fill_balance` (the fill's `accountBalance`). `None` if the
    /// fetch failed or did not complete within `self.timeout` -- the bound keeps a
    /// hung summary endpoint from stalling the transaction stream's
    /// heartbeat/cancellation arms (and the REST order path's response).
    pub(crate) async fn account_snapshot_for_fill(
        &self,
        fill_balance: Option<&str>,
    ) -> Option<Account> {
        let account = match tokio::time::timeout(self.timeout, self.fetch()).await {
            Ok(account) => account?,
            Err(_elapsed) => {
                warn!(
                    platform = %self.platform,
                    timeout_ms = self.timeout.as_millis(),
                    "Account summary fetch timed out -- skipping account snapshot"
                );
                return None;
            }
        };
        Some(merge_fill_account(account, fill_balance))
    }
}

/// Merge a fetched account summary with the balance carried on the fill.
///
/// `balance` is taken from the fill's `accountBalance` when present -- it is the
/// point-in-time balance booked by this fill and ordered against it, whereas the
/// fetched summary can race ahead of a subsequent fill. `equity` (OANDA NAV) and the
/// margin/unrealized fields come from the summary: there is no per-fill source for
/// them, and OANDA's NAV is the authoritative equity. After the override `equity` may
/// not exactly equal `balance + unrealized_pnl`; the canary scores each dimension
/// independently, so this two-source merge is acceptable.
fn merge_fill_account(mut account: Account, fill_balance: Option<&str>) -> Account {
    if let Some(balance) = fill_balance.and_then(|b| Decimal::from_str(b).ok()) {
        account.balance = balance;
    }
    account
}

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

    // No last-trade price on forex; `last` reports mid instead.
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
        // STOP_LOSS_ORDER / TAKE_PROFIT_ORDER are the dependent legs OANDA
        // creates server-side on a native bracket's entry fill. They reference a
        // trade and carry no instrument/units, so the emitted OrderCreated is
        // wire-only (id, type, trigger price); without it the SL leg -- the OCO
        // loser, which never fills -- would never surface to consumers.
        "LIMIT_ORDER"
        | "STOP_ORDER"
        | "MARKET_IF_TOUCHED_ORDER"
        | "STOP_LOSS_ORDER"
        | "TAKE_PROFIT_ORDER" => transaction_new_order_to_messages(tx, platform),
        "MARKET_ORDER_REJECT" => transaction_reject_to_messages(tx, platform),
        "DAILY_FINANCING" => transaction_financing_to_messages(tx, platform),
        // Ignored transaction types: TRAILING_STOP_LOSS_ORDER (carries a distance,
        // not a price), MARKET_ORDER (its fill is what matters), etc.
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

/// Derive the filled order's type and time-in-force from the `ORDER_FILL`
/// `reason` -- the only order context the wire line carries.
///
/// `MARKET_IF_TOUCHED_ORDER` maps to `StopLimit` and `TRAILING_STOP_LOSS_ORDER`
/// to `TrailingStop`, matching the adapter's `from_oanda_order_type`. Market
/// fills get `Fok`: OANDA market orders are FOK or IOC only (never GTC), and
/// the adapter always submits FOK. Resting-order fills default to `Gtc` (see
/// the event contract below). A missing or unrecognised reason keeps the
/// pre-enrichment `(Market, Gtc)` -- an "unknown" sentinel, not a wire-valid
/// combination.
fn fill_reason_to_order_context(reason: Option<&str>) -> (OrderType, TimeInForce) {
    match reason {
        Some("LIMIT_ORDER" | "TAKE_PROFIT_ORDER") => (OrderType::Limit, TimeInForce::Gtc),
        Some("STOP_ORDER" | "STOP_LOSS_ORDER" | "GUARANTEED_STOP_LOSS_ORDER") => {
            (OrderType::Stop, TimeInForce::Gtc)
        }
        Some("TRAILING_STOP_LOSS_ORDER") => (OrderType::TrailingStop, TimeInForce::Gtc),
        Some("MARKET_IF_TOUCHED_ORDER") => (OrderType::StopLimit, TimeInForce::Gtc),
        Some(r) if r.starts_with("MARKET_ORDER") => (OrderType::Market, TimeInForce::Fok),
        _ => (OrderType::Market, TimeInForce::Gtc),
    }
}

/// ORDER_FILL -> Filled order event, plus a Trade carrying commission and an
/// optional Financing for carry settled on the fill.
///
/// Emits, in order:
/// 1. `Order { OrderFilled }` -- the order state change (unchanged).
/// 2. `Trade { TradeFilled }` -- symmetric with the engine leg, carrying the
///    per-fill `commission` so the canary can pair the same shape live-vs-backtest.
/// 3. `Financing` -- only when the fill books non-zero `financing` (closing an
///    overnight position settles accrued carry on the fill).
///
/// `commission_currency` is left empty: OANDA does not carry the account home
/// currency on the transaction line (amounts are denominated in it).
///
/// # Event contract: stream-won vs REST-won fills
///
/// A fill belonging to a synchronous REST `submit_order` is published by
/// whichever path claims its transaction id first (see `dedupe`). The two
/// shapes differ -- which one strategies receive depends on the race winner:
///
/// * **REST wins** (`OandaAdapter::emit_rest_fill`): full request context --
///   real `order_type`/`time_in_force`, `quantity` = requested, and a partial
///   IOC publishes `OrderPartiallyFilled(requested/filled/remaining)` followed
///   by `OrderCancelled`.
/// * **Stream wins** (this conversion): wire context only -- `order_type` and
///   `time_in_force` derived from the fill `reason`
///   ([`fill_reason_to_order_context`]), `quantity` = filled units,
///   `remaining` = 0, and a partial IOC publishes OANDA's wire shape:
///   `OrderFilled(filled)` followed by `OrderCancelled`. A stream-won
///   IOC/FOK limit fill reports `Gtc` (the wire does not carry TIF), and
///   non-market types carry `limit_price`/`stop_price` = `None` (the wire
///   does not carry the trigger price either).
///
/// Either way the fill+cancel pair is claimed atomically and stays internally
/// coherent, and the `Trade` frame is identical in both shapes -- consumers
/// pairing executions should key on trades. Resting-order fills (entered via
/// the create response, filled later) have no REST competitor and always take
/// the stream shape.
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

    let timestamp = parse_transaction_time(tx.time.as_deref());
    let (order_type, time_in_force) = fill_reason_to_order_context(tx.reason.as_deref());

    let order = Order {
        id: order_id.to_string(),
        client_order_id: tx.client_order_id.clone(),
        symbol: symbol.clone(),
        side,
        order_type,
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
        time_in_force,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let commission = tx
        .commission
        .as_deref()
        .and_then(|c| Decimal::from_str(c).ok())
        .unwrap_or(Decimal::ZERO);

    let trade = Trade {
        // The fill transaction id identifies the execution; fall back to the order id.
        id: tx
            .id
            .as_deref()
            .or(tx.order_id.as_deref())
            .unwrap_or("unknown")
            .to_string(),
        order_id: order_id.to_string(),
        symbol: symbol.clone(),
        side,
        quantity,
        price: fill_price.unwrap_or(Decimal::ZERO),
        commission,
        // Home currency is not carried on the transaction line (see fn docs).
        commission_currency: String::new(),
        is_maker: None,
        timestamp,
    };

    let mut messages = vec![
        WsMessage::Order {
            event: OrderEventType::OrderFilled,
            order,
            parent_order_id: None,
            timestamp: Utc::now(),
        },
        WsMessage::trade(trade),
    ];

    if let Some(amount) = tx
        .financing
        .as_deref()
        .and_then(|f| Decimal::from_str(f).ok())
        && !amount.is_zero()
    {
        messages.push(WsMessage::Financing {
            symbol,
            amount,
            timestamp,
        });
    }

    messages
}

/// DAILY_FINANCING -> one Financing message per instrument.
///
/// OANDA books overnight carry once daily (21:00 UTC) as a single transaction with
/// a per-instrument `positionFinancings[]` breakdown. Each entry becomes a
/// `WsMessage::Financing` stamped with the transaction time. Entries that do not
/// parse are skipped. Position units are intentionally not forwarded -- OANDA does
/// not carry them on this transaction.
fn transaction_financing_to_messages(
    tx: &OandaTransactionStreamLine,
    _platform: TradingPlatform,
) -> Vec<WsMessage> {
    let timestamp = parse_transaction_time(tx.time.as_deref());

    tx.position_financings
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|pf| {
            let amount = Decimal::from_str(&pf.financing).ok()?;
            Some(WsMessage::Financing {
                symbol: pf.instrument.clone(),
                amount,
                timestamp,
            })
        })
        .collect()
}

/// Parse an OANDA RFC-3339 transaction time, falling back to now if absent/unparseable.
pub(crate) fn parse_transaction_time(time: Option<&str>) -> DateTime<Utc> {
    time.and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map_or_else(Utc::now, |dt| dt.with_timezone(&Utc))
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
        client_order_id: tx.client_order_id.clone(),
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
        "LIMIT_ORDER" | "TAKE_PROFIT_ORDER" => OrderType::Limit,
        "STOP_ORDER" | "STOP_LOSS_ORDER" => OrderType::Stop,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OandaPriceBucket;
    use tektii_gateway_core::websocket::messages::TradeEventType;
    use tektii_gateway_test_support::wiremock_helpers::{
        mount_json, mount_json_with_delay, start_mock_server,
    };

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
            client_order_id: None,
            reason: None,
            reject_reason: None,
            commission: None,
            financing: None,
            account_balance: None,
            position_financings: None,
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
        // ORDER_FILL now emits Order + Trade (commission/financing both 0 here).
        assert_eq!(msgs.len(), 2);

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
        assert_eq!(msgs.len(), 2);

        match &msgs[0] {
            WsMessage::Order { order, .. } => {
                assert_eq!(order.side, Side::Sell);
                assert_eq!(order.quantity, Decimal::from(5000));
                // No orderID => use transaction id.
                assert_eq!(order.id, "6361");
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_fill_carries_client_order_id() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("-10000".to_string()),
            price: Some("1.09500".to_string()),
            order_id: Some("6400".to_string()),
            client_order_id: Some("strat-42-tp".to_string()),
            ..test_tx("6401", "ORDER_FILL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);

        match &msgs[0] {
            WsMessage::Order { order, .. } => {
                assert_eq!(order.client_order_id, Some("strat-42-tp".to_string()));
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_fill_without_client_order_id_stays_none() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("10000".to_string()),
            price: Some("1.08525".to_string()),
            order_id: Some("6357".to_string()),
            ..test_tx("6360", "ORDER_FILL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);

        match &msgs[0] {
            WsMessage::Order { order, .. } => {
                assert_eq!(order.client_order_id, None);
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    /// Extract `(order_type, time_in_force)` from a fill's `Order` frame.
    fn fill_order_context(reason: Option<&str>) -> (OrderType, TimeInForce) {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("10000".to_string()),
            price: Some("1.08525".to_string()),
            order_id: Some("6357".to_string()),
            reason: reason.map(String::from),
            ..test_tx("6360", "ORDER_FILL")
        };
        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        match &msgs[0] {
            WsMessage::Order { order, .. } => (order.order_type, order.time_in_force),
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_fill_market_reasons_map_to_market_fok() {
        for reason in [
            "MARKET_ORDER",
            "MARKET_ORDER_TRADE_CLOSE",
            "MARKET_ORDER_POSITION_CLOSEOUT",
            "MARKET_ORDER_MARGIN_CLOSEOUT",
        ] {
            assert_eq!(
                fill_order_context(Some(reason)),
                (OrderType::Market, TimeInForce::Fok),
                "reason {reason}"
            );
        }
    }

    #[test]
    fn transaction_fill_limit_reasons_map_to_limit() {
        for reason in ["LIMIT_ORDER", "TAKE_PROFIT_ORDER"] {
            assert_eq!(
                fill_order_context(Some(reason)),
                (OrderType::Limit, TimeInForce::Gtc),
                "reason {reason}"
            );
        }
    }

    #[test]
    fn transaction_fill_stop_reasons_map_to_stop() {
        for reason in [
            "STOP_ORDER",
            "STOP_LOSS_ORDER",
            "GUARANTEED_STOP_LOSS_ORDER",
        ] {
            assert_eq!(
                fill_order_context(Some(reason)),
                (OrderType::Stop, TimeInForce::Gtc),
                "reason {reason}"
            );
        }
    }

    #[test]
    fn transaction_fill_trailing_stop_reason_maps_to_trailing_stop() {
        assert_eq!(
            fill_order_context(Some("TRAILING_STOP_LOSS_ORDER")),
            (OrderType::TrailingStop, TimeInForce::Gtc)
        );
    }

    #[test]
    fn transaction_fill_market_if_touched_reason_maps_to_stop_limit() {
        assert_eq!(
            fill_order_context(Some("MARKET_IF_TOUCHED_ORDER")),
            (OrderType::StopLimit, TimeInForce::Gtc)
        );
    }

    #[test]
    fn transaction_fill_missing_or_unknown_reason_defaults_market_gtc() {
        assert_eq!(
            fill_order_context(None),
            (OrderType::Market, TimeInForce::Gtc)
        );
        assert_eq!(
            fill_order_context(Some("FIXED_PRICE_ORDER")),
            (OrderType::Market, TimeInForce::Gtc)
        );
    }

    #[test]
    fn transaction_stream_line_parses_client_order_id() {
        // OANDA spells the wire field `clientOrderID` (capital ID).
        let line = r#"{"type":"ORDER_FILL","id":"6401","orderID":"6400","clientOrderID":"strat-42-tp","instrument":"EUR_USD","units":"-10000","price":"1.09500"}"#;
        let tx: OandaTransactionStreamLine = serde_json::from_str(line).unwrap();
        assert_eq!(tx.client_order_id.as_deref(), Some("strat-42-tp"));
    }

    #[test]
    fn transaction_cancel_carries_client_order_id() {
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            order_id: Some("6402".to_string()),
            client_order_id: Some("strat-42-sl".to_string()),
            reason: Some("LINKED_TRADE_CLOSED".to_string()),
            ..test_tx("6403", "ORDER_CANCEL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);

        match &msgs[0] {
            WsMessage::Order { order, .. } => {
                assert_eq!(order.client_order_id, Some("strat-42-sl".to_string()));
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
    fn transaction_stop_loss_order_emits_created() {
        let tx = OandaTransactionStreamLine {
            price: Some("1.14362".to_string()),
            ..test_tx("6211", "STOP_LOSS_ORDER")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { event, order, .. } => {
                assert_eq!(*event, OrderEventType::OrderCreated);
                assert_eq!(order.id, "6211");
                assert_eq!(order.order_type, OrderType::Stop);
                // Wire-only: the SL transaction carries no instrument/units.
                assert!(order.symbol.is_empty());
                assert_eq!(order.quantity, Decimal::ZERO);
                assert_eq!(
                    order.stop_price,
                    Some(Decimal::from_str("1.14362").unwrap())
                );
                assert_eq!(order.status, OrderStatus::Open);
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_take_profit_order_emits_created() {
        let tx = OandaTransactionStreamLine {
            price: Some("1.14398".to_string()),
            ..test_tx("6212", "TAKE_PROFIT_ORDER")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);

        match &msgs[0] {
            WsMessage::Order { event, order, .. } => {
                assert_eq!(*event, OrderEventType::OrderCreated);
                assert_eq!(order.id, "6212");
                assert_eq!(order.order_type, OrderType::Limit);
                assert_eq!(
                    order.limit_price,
                    Some(Decimal::from_str("1.14398").unwrap())
                );
                assert_eq!(order.status, OrderStatus::Open);
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_ignored_types_produce_no_messages() {
        // DAILY_FINANCING, STOP_LOSS_ORDER, and TAKE_PROFIT_ORDER are no longer
        // ignored -- they emit Financing / OrderCreated messages respectively.
        // TRAILING_STOP_LOSS_ORDER stays ignored (the transaction carries a
        // distance, not a price, which the wire-only converter cannot model).
        for tx_type in &["TRAILING_STOP_LOSS_ORDER", "MARKET_ORDER"] {
            let tx = test_tx("9999", tx_type);

            let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
            assert!(
                msgs.is_empty(),
                "Transaction type '{tx_type}' should produce no messages"
            );
        }
    }

    #[test]
    fn transaction_fill_emits_order_and_trade_with_commission() {
        // A core-pricing fill with a non-zero commission. The OANDA leg must emit a
        // Trade carrying the commission, symmetric with the engine leg.
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("10000".to_string()),
            price: Some("1.08525".to_string()),
            time: Some("2024-01-15T10:30:00Z".to_string()),
            order_id: Some("6357".to_string()),
            commission: Some("2.5000".to_string()),
            ..test_tx("6360", "ORDER_FILL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        // Order + Trade (financing is 0 -> no Financing message).
        assert_eq!(msgs.len(), 2);
        assert!(matches!(
            &msgs[0],
            WsMessage::Order {
                event: OrderEventType::OrderFilled,
                ..
            }
        ));
        match &msgs[1] {
            WsMessage::Trade { event, trade, .. } => {
                assert_eq!(*event, TradeEventType::TradeFilled);
                assert_eq!(trade.symbol, "EUR_USD");
                assert_eq!(trade.side, Side::Buy);
                assert_eq!(trade.quantity, Decimal::from(10000));
                assert_eq!(trade.price, Decimal::from_str("1.08525").unwrap());
                assert_eq!(trade.commission, Decimal::from_str("2.5000").unwrap());
                // Home currency is not carried on the transaction line.
                assert_eq!(trade.commission_currency, "");
                assert_eq!(trade.order_id, "6357");
            }
            other => panic!("Expected Trade event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_fill_zero_commission_still_emits_trade() {
        // OANDA spread-bet/practice fills report commission 0. The Trade must still be
        // emitted (commission 0) so the canary's commission dimension is scoreable
        // live-vs-backtest instead of structurally absent.
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("1".to_string()),
            price: Some("1.16163".to_string()),
            commission: Some("0.0000".to_string()),
            financing: Some("0.0000".to_string()),
            ..test_tx("1468470", "ORDER_FILL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 2, "zero financing must not add a Financing msg");
        match &msgs[1] {
            WsMessage::Trade { trade, .. } => {
                assert_eq!(trade.commission, Decimal::ZERO);
            }
            other => panic!("Expected Trade event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_fill_with_financing_emits_financing() {
        // Closing an overnight position settles accrued carry on the fill; forward it
        // as a Financing message alongside the Order and Trade.
        let tx = OandaTransactionStreamLine {
            instrument: Some("EUR_USD".to_string()),
            units: Some("-10000".to_string()),
            price: Some("1.08525".to_string()),
            commission: Some("0.0000".to_string()),
            financing: Some("-0.7500".to_string()),
            ..test_tx("6360", "ORDER_FILL")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 3, "Order + Trade + Financing");
        match &msgs[2] {
            WsMessage::Financing { symbol, amount, .. } => {
                assert_eq!(symbol, "EUR_USD");
                assert_eq!(*amount, Decimal::from_str("-0.7500").unwrap());
            }
            other => panic!("Expected Financing event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_daily_financing_emits_per_position() {
        let tx = OandaTransactionStreamLine {
            time: Some("2026-06-01T21:00:00.000000000Z".to_string()),
            financing: Some("-3.3873".to_string()),
            position_financings: Some(vec![
                crate::types::OandaPositionFinancing {
                    instrument: "EUR_USD".to_string(),
                    financing: "-4.5873".to_string(),
                },
                crate::types::OandaPositionFinancing {
                    instrument: "GBP_USD".to_string(),
                    financing: "1.2000".to_string(),
                },
            ]),
            ..test_tx("1467691", "DAILY_FINANCING")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 2, "one Financing message per position");
        let expected_ts = DateTime::parse_from_rfc3339("2026-06-01T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        match &msgs[0] {
            WsMessage::Financing {
                symbol,
                amount,
                timestamp,
            } => {
                assert_eq!(symbol, "EUR_USD");
                assert_eq!(*amount, Decimal::from_str("-4.5873").unwrap());
                assert_eq!(*timestamp, expected_ts, "uses the transaction time");
            }
            other => panic!("Expected Financing event, got {other:?}"),
        }
        match &msgs[1] {
            WsMessage::Financing { symbol, amount, .. } => {
                assert_eq!(symbol, "GBP_USD");
                assert_eq!(*amount, Decimal::from_str("1.2000").unwrap());
            }
            other => panic!("Expected Financing event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_daily_financing_skips_unparseable_entry() {
        // The documented contract: a positionFinancings entry whose amount does not
        // parse is skipped, the well-formed siblings still emit.
        let tx = OandaTransactionStreamLine {
            time: Some("2026-06-01T21:00:00Z".to_string()),
            position_financings: Some(vec![
                crate::types::OandaPositionFinancing {
                    instrument: "EUR_USD".to_string(),
                    financing: "-4.5873".to_string(),
                },
                crate::types::OandaPositionFinancing {
                    instrument: "GBP_USD".to_string(),
                    financing: "not-a-number".to_string(),
                },
            ]),
            ..test_tx("1467691", "DAILY_FINANCING")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1, "the unparseable entry is skipped");
        match &msgs[0] {
            WsMessage::Financing { symbol, .. } => assert_eq!(symbol, "EUR_USD"),
            other => panic!("Expected Financing event, got {other:?}"),
        }
    }

    #[test]
    fn transaction_financing_falls_back_to_now_without_time() {
        // No transaction time on the line => the message is stamped with ~now, not the
        // Unix epoch (the documented fallback in parse_transaction_time).
        let before = Utc::now();
        let tx = OandaTransactionStreamLine {
            // test_tx leaves `time: None`.
            position_financings: Some(vec![crate::types::OandaPositionFinancing {
                instrument: "EUR_USD".to_string(),
                financing: "-1.0".to_string(),
            }]),
            ..test_tx("1", "DAILY_FINANCING")
        };

        let msgs = transaction_to_messages(&tx, TradingPlatform::OandaPractice);
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            WsMessage::Financing { timestamp, .. } => {
                assert!(
                    *timestamp >= before,
                    "should fall back to ~now: {timestamp}"
                );
            }
            other => panic!("Expected Financing event, got {other:?}"),
        }
    }

    /// Run the buffer through a strategy sink backed by `dedupe` and return
    /// everything it sent.
    async fn run_buffer_through_strategy_sink(
        buffer: &mut BytesMut,
        dedupe: &PublishedTransactions,
        account_fetcher: Option<&AccountFetcher<'_>>,
    ) -> Vec<WsMessage> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(RwLock::new(Some(tx)));
        let cancel = CancellationToken::new();
        process_transaction_buffer(
            buffer,
            TradingPlatform::OandaPractice,
            &TransactionSink::Strategy {
                slot: &slot,
                dedupe,
                account_fetcher,
            },
            &cancel,
            &cancel,
        )
        .await;

        let mut messages = Vec::new();
        while let Ok(event) = rx.try_recv() {
            messages.push(event.msg);
        }
        messages
    }

    fn fresh_dedupe() -> PublishedTransactions {
        PublishedTransactions::with_capacity(8)
    }

    #[tokio::test]
    async fn ndjson_buffer_processing() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"HEARTBEAT","time":"2024-01-15T10:30:05.000000000Z","lastTransactionID":"6360"}
{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
"#,
        );

        // No fetcher (None): heartbeat produces no messages; ORDER_FILL produces
        // Order + Trade, with no account snapshot appended.
        let messages = run_buffer_through_strategy_sink(&mut buffer, &fresh_dedupe(), None).await;

        assert_eq!(messages.len(), 2);
        assert!(buffer.is_empty(), "Buffer should be fully consumed");
    }

    #[tokio::test]
    async fn ndjson_stop_loss_order_surfaces_through_strategy_sink() {
        // A native bracket's server-side SL leg arrives as a STOP_LOSS_ORDER line.
        // It is not a REST-published type, so the dedupe never suppresses it; it
        // must reach the strategy sink as an OrderCreated.
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"STOP_LOSS_ORDER","id":"6211","price":"1.14362","reason":"ON_FILL"}
"#,
        );

        let messages = run_buffer_through_strategy_sink(&mut buffer, &fresh_dedupe(), None).await;

        assert_eq!(messages.len(), 1);
        match &messages[0] {
            WsMessage::Order { event, order, .. } => {
                assert_eq!(*event, OrderEventType::OrderCreated);
                assert_eq!(order.id, "6211");
                assert_eq!(order.order_type, OrderType::Stop);
                assert_eq!(
                    order.stop_price,
                    Some(Decimal::from_str("1.14362").unwrap())
                );
            }
            other => panic!("Expected Order event, got {other:?}"),
        }
        assert!(buffer.is_empty(), "Buffer should be fully consumed");
    }

    #[tokio::test]
    async fn ndjson_fill_sent_inline_even_when_more_data_is_pending() {
        // Inline emission: a fill must reach the sink as soon as its line is
        // parsed, not be held back until the connection errors.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(RwLock::new(Some(tx)));
        let dedupe = fresh_dedupe();
        let cancel = CancellationToken::new();

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
{"type":"HEARTBEAT","time":"2024-01-"#,
        );

        process_transaction_buffer(
            &mut buffer,
            TradingPlatform::OandaPractice,
            &TransactionSink::Strategy {
                slot: &slot,
                dedupe: &dedupe,
                account_fetcher: None,
            },
            &cancel,
            &cancel,
        )
        .await;

        assert!(
            matches!(rx.try_recv(), Ok(event) if matches!(event.msg, WsMessage::Order { .. })),
            "fill must be on the channel immediately after its line is parsed"
        );
    }

    #[tokio::test]
    async fn ndjson_internal_sink_sends_inline_without_dedupe() {
        // Pins inline emission on the Internal sink. (Its independence from
        // strategy-side claims is structural -- the variant carries no dedupe
        // handle -- and is pinned end-to-end by the integration test
        // `internal_stream_delivers_fills_independent_of_strategy_dedupe`.)
        let (tx, mut rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
"#,
        );

        process_transaction_buffer(
            &mut buffer,
            TradingPlatform::OandaPractice,
            &TransactionSink::Internal(&tx, TradingPlatform::OandaPractice),
            &cancel,
            &cancel,
        )
        .await;

        assert!(
            matches!(rx.try_recv(), Ok(event) if matches!(event.message, WsMessage::Order { .. })),
            "fill must be broadcast immediately after its line is parsed"
        );
    }

    #[tokio::test]
    async fn ndjson_dedupe_suppresses_redelivered_fill() {
        let dedupe = fresh_dedupe();
        let line = br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
"#;

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(line);
        let first = run_buffer_through_strategy_sink(&mut buffer, &dedupe, None).await;
        assert_eq!(first.len(), 2, "first delivery emits Order + Trade");

        // The same transaction re-delivered (e.g. after a reconnect).
        buffer.extend_from_slice(line);
        let second = run_buffer_through_strategy_sink(&mut buffer, &dedupe, None).await;
        assert!(second.is_empty(), "re-delivered fill must be suppressed");
    }

    #[tokio::test]
    async fn ndjson_dedupe_only_applies_to_rest_published_types() {
        // LIMIT_ORDER creates are never published by the REST path, so they
        // bypass the claim even if an id collides.
        let dedupe = fresh_dedupe();
        assert!(dedupe.try_claim("7001"));

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"LIMIT_ORDER","id":"7001","instrument":"EUR_USD","units":"10000","price":"1.05000"}
"#,
        );

        let messages = run_buffer_through_strategy_sink(&mut buffer, &dedupe, None).await;
        assert_eq!(
            messages.len(),
            1,
            "order-create event must still be emitted"
        );
    }

    #[tokio::test]
    async fn ndjson_fill_without_id_bypasses_dedupe() {
        // An id-less ORDER_FILL cannot be claimed, so it is emitted rather
        // than dropped -- a possible duplicate is preferable to a lost fill.
        let dedupe = fresh_dedupe();
        assert!(dedupe.try_claim("6361"));

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
"#,
        );

        let messages = run_buffer_through_strategy_sink(&mut buffer, &dedupe, None).await;
        assert_eq!(messages.len(), 2, "id-less fill still emits Order + Trade");
    }

    #[tokio::test]
    async fn ndjson_disconnected_channel_does_not_claim() {
        // With no strategy sender, the stream must decline WITHOUT claiming,
        // so the REST path can still publish the fill later.
        let slot: Arc<RwLock<Option<mpsc::UnboundedSender<ProviderEvent>>>> =
            Arc::new(RwLock::new(None));
        let dedupe = fresh_dedupe();
        let cancel = CancellationToken::new();

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
"#,
        );

        process_transaction_buffer(
            &mut buffer,
            TradingPlatform::OandaPractice,
            &TransactionSink::Strategy {
                slot: &slot,
                dedupe: &dedupe,
                account_fetcher: None,
            },
            &cancel,
            &cancel,
        )
        .await;

        assert!(
            dedupe.try_claim("6361"),
            "a dead channel must not claim the fill away from the REST path"
        );
    }

    #[tokio::test]
    async fn ndjson_cancelled_generation_stops_emitting() {
        // Once the generation's token fires (its event_tx repointed to the
        // new generation), remaining buffered lines must not be emitted.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(RwLock::new(Some(tx)));
        let dedupe = fresh_dedupe();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
"#,
        );

        process_transaction_buffer(
            &mut buffer,
            TradingPlatform::OandaPractice,
            &TransactionSink::Strategy {
                slot: &slot,
                dedupe: &dedupe,
                account_fetcher: None,
            },
            &cancel,
            &CancellationToken::new(),
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "a cancelled generation must not emit"
        );
        assert!(
            dedupe.try_claim("6361"),
            "a cancelled generation must not claim either"
        );
    }

    #[tokio::test]
    async fn ndjson_disconnect_stops_emitting() {
        // The parent (disconnect) token must stop emission just like the
        // per-generation token.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(RwLock::new(Some(tx)));
        let dedupe = fresh_dedupe();
        let parent_cancel = CancellationToken::new();
        parent_cancel.cancel();

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357"}
"#,
        );

        process_transaction_buffer(
            &mut buffer,
            TradingPlatform::OandaPractice,
            &TransactionSink::Strategy {
                slot: &slot,
                dedupe: &dedupe,
                account_fetcher: None,
            },
            &CancellationToken::new(),
            &parent_cancel,
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "a disconnected provider must not emit"
        );
    }

    #[tokio::test]
    async fn ndjson_partial_line_stays_in_buffer() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(br#"{"type":"HEARTBEAT","time":"2024-01-"#);

        let messages = run_buffer_through_strategy_sink(&mut buffer, &fresh_dedupe(), None).await;

        assert!(messages.is_empty());
        assert!(!buffer.is_empty(), "Partial line should remain in buffer");
    }

    #[tokio::test]
    async fn ndjson_malformed_line_skipped() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(b"not valid json\n");

        let messages = run_buffer_through_strategy_sink(&mut buffer, &fresh_dedupe(), None).await;

        assert!(
            messages.is_empty(),
            "Malformed line should produce no messages"
        );
        assert!(
            buffer.is_empty(),
            "Malformed line should be consumed from buffer"
        );
    }

    /// Build an OANDA account-summary response body.
    fn account_summary_value(
        balance: &str,
        nav: &str,
        margin_used: &str,
        margin_available: &str,
        unrealized_pl: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "account": {
                "balance": balance,
                "NAV": nav,
                "marginUsed": margin_used,
                "marginAvailable": margin_available,
                "unrealizedPL": unrealized_pl,
                "hedgingEnabled": false,
                "currency": "USD"
            }
        })
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn merge_fill_account_overrides_balance_from_fill() {
        let fetched = Account {
            balance: dec("100000.0000"),
            equity: dec("100250.5000"),
            margin_used: dec("2500.0000"),
            margin_available: dec("97750.5000"),
            unrealized_pnl: dec("250.5000"),
            currency: "USD".to_string(),
        };

        let merged = merge_fill_account(fetched, Some("100123.4500"));

        // Balance comes from the fill; everything else stays from the summary.
        assert_eq!(merged.balance, dec("100123.4500"));
        assert_eq!(merged.equity, dec("100250.5000"));
        assert_eq!(merged.margin_used, dec("2500.0000"));
        assert_eq!(merged.margin_available, dec("97750.5000"));
        assert_eq!(merged.unrealized_pnl, dec("250.5000"));
        assert_eq!(merged.currency, "USD");
    }

    #[test]
    fn merge_fill_account_falls_back_to_summary_balance() {
        let fetched = Account {
            balance: dec("100000.0000"),
            equity: dec("100250.5000"),
            margin_used: dec("2500.0000"),
            margin_available: dec("97750.5000"),
            unrealized_pnl: dec("250.5000"),
            currency: "USD".to_string(),
        };

        // Fill omits accountBalance, or carries an unparseable value -> keep summary.
        assert_eq!(
            merge_fill_account(fetched.clone(), None).balance,
            dec("100000.0000")
        );
        assert_eq!(
            merge_fill_account(fetched, Some("not-a-number")).balance,
            dec("100000.0000")
        );
    }

    #[tokio::test]
    async fn account_fetcher_maps_summary() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            200,
            account_summary_value(
                "100000.0000",
                "100250.5000",
                "2500.0000",
                "97750.5000",
                "250.5000",
            ),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: ACCOUNT_FETCH_TIMEOUT,
        };

        let account = fetcher
            .fetch()
            .await
            .expect("summary should map to Account");
        assert_eq!(account.balance, dec("100000.0000"));
        assert_eq!(account.equity, dec("100250.5000"));
        assert_eq!(account.margin_used, dec("2500.0000"));
        assert_eq!(account.margin_available, dec("97750.5000"));
        assert_eq!(account.unrealized_pnl, dec("250.5000"));
        assert_eq!(account.currency, "USD");
    }

    #[tokio::test]
    async fn account_fetcher_returns_none_on_server_error() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            500,
            serde_json::json!({ "errorMessage": "boom" }),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: ACCOUNT_FETCH_TIMEOUT,
        };

        assert!(
            fetcher.fetch().await.is_none(),
            "a 500 must yield no snapshot, not a bogus zero-filled one"
        );
    }

    #[tokio::test]
    async fn account_fetcher_returns_none_on_decode_error() {
        // A 200 whose body does not match the expected schema (e.g. OANDA drift) must
        // yield no snapshot rather than a partially/zero-filled Account.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            200,
            serde_json::json!({ "unexpected": true }),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: ACCOUNT_FETCH_TIMEOUT,
        };

        assert!(
            fetcher.fetch().await.is_none(),
            "an unparseable 200 body must yield no snapshot"
        );
    }

    #[tokio::test]
    async fn account_fetcher_returns_none_on_non_numeric_field() {
        // A 200 that deserializes but carries a non-numeric monetary field must yield no
        // snapshot, rather than zeroing that field (which would feed a bogus equity).
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            200,
            account_summary_value(
                "100000.0000",
                "not-a-number",
                "2500.0000",
                "97750.5000",
                "250.5000",
            ),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: ACCOUNT_FETCH_TIMEOUT,
        };

        assert!(
            fetcher.fetch().await.is_none(),
            "a non-numeric NAV must yield no snapshot, not a zeroed equity"
        );
    }

    #[tokio::test]
    async fn account_snapshot_skipped_on_timeout() {
        // A summary response slower than the fetcher's bound must yield no
        // snapshot -- the bound keeps a hung endpoint from stalling the
        // transaction stream and the REST order path.
        let (server, base_url) = start_mock_server().await;
        mount_json_with_delay(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            200,
            account_summary_value(
                "100000.0000",
                "100250.5000",
                "2500.0000",
                "97750.5000",
                "250.5000",
            ),
            Duration::from_millis(500),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: Duration::from_millis(50),
        };

        assert!(
            fetcher
                .account_snapshot_for_fill(Some("100123.45"))
                .await
                .is_none(),
            "a timed-out summary fetch must drop the snapshot"
        );
    }

    #[tokio::test]
    async fn order_fill_emits_account_snapshot_ordered_after_fill() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            200,
            account_summary_value(
                "999999.9999",
                "100250.5000",
                "2500.0000",
                "97750.5000",
                "250.5000",
            ),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: ACCOUNT_FETCH_TIMEOUT,
        };

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357","accountBalance":"100123.4500","time":"2024-01-15T10:30:00Z"}
"#,
        );

        let messages =
            run_buffer_through_strategy_sink(&mut buffer, &fresh_dedupe(), Some(&fetcher)).await;

        // Order, Trade, then the account snapshot -- in that order.
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[0], WsMessage::Order { .. }));
        assert!(matches!(messages[1], WsMessage::Trade { .. }));
        match &messages[2] {
            WsMessage::Account {
                event,
                account,
                timestamp,
            } => {
                assert_eq!(*event, AccountEventType::BalanceUpdated);
                // Balance from the fill's accountBalance, NOT the summary's balance.
                assert_eq!(account.balance, dec("100123.4500"));
                // Equity/margin/unrealized from the summary.
                assert_eq!(account.equity, dec("100250.5000"));
                assert_eq!(account.margin_used, dec("2500.0000"));
                assert_eq!(account.margin_available, dec("97750.5000"));
                assert_eq!(account.unrealized_pnl, dec("250.5000"));
                // Stamped with the fill's transaction time, ordered against the fill.
                assert_eq!(
                    *timestamp,
                    parse_transaction_time(Some("2024-01-15T10:30:00Z"))
                );
            }
            other => panic!("Expected Account snapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_fills_in_one_chunk_each_get_their_own_snapshot_in_order() {
        // The canary attributes an account snapshot to the fill that precedes it on
        // the wire, so two fills in a single chunk must yield [F1, A1, F2, A2] -- each
        // Account ordered immediately after its own fill, not both appended at the end.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            200,
            account_summary_value("0.0", "100250.5000", "2500.0000", "97750.5000", "250.5000"),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: ACCOUNT_FETCH_TIMEOUT,
        };

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"1","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"11","accountBalance":"100100.0000"}
{"type":"ORDER_FILL","id":"2","instrument":"EUR_USD","units":"-5000","price":"1.08600","orderID":"12","accountBalance":"100200.0000"}
"#,
        );

        let messages =
            run_buffer_through_strategy_sink(&mut buffer, &fresh_dedupe(), Some(&fetcher)).await;

        // [Order, Trade, Account] x2, interleaved per fill.
        assert_eq!(messages.len(), 6);
        assert!(matches!(messages[0], WsMessage::Order { .. }));
        assert!(matches!(messages[1], WsMessage::Trade { .. }));
        assert!(matches!(messages[3], WsMessage::Order { .. }));
        assert!(matches!(messages[4], WsMessage::Trade { .. }));

        // First snapshot carries the first fill's balance; second carries the second's.
        match &messages[2] {
            WsMessage::Account { account, .. } => assert_eq!(account.balance, dec("100100.0000")),
            other => panic!("Expected first Account snapshot at index 2, got {other:?}"),
        }
        match &messages[5] {
            WsMessage::Account { account, .. } => assert_eq!(account.balance, dec("100200.0000")),
            other => panic!("Expected second Account snapshot at index 5, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn order_fill_emits_no_account_snapshot_when_fetch_fails() {
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/accounts/test-acct/summary",
            500,
            serde_json::json!({ "errorMessage": "boom" }),
        )
        .await;

        let client = Client::new();
        let fetcher = AccountFetcher {
            client: &client,
            rest_url: &base_url,
            api_token: "test-token",
            account_id: "test-acct",
            platform: TradingPlatform::OandaPractice,
            timeout: ACCOUNT_FETCH_TIMEOUT,
        };

        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            br#"{"type":"ORDER_FILL","id":"6361","instrument":"EUR_USD","units":"10000","price":"1.08525","orderID":"6357","accountBalance":"100123.4500"}
"#,
        );

        let messages =
            run_buffer_through_strategy_sink(&mut buffer, &fresh_dedupe(), Some(&fetcher)).await;

        // Fill frames still emitted; no account snapshot when the summary fetch fails.
        assert_eq!(messages.len(), 2);
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, WsMessage::Account { .. })),
            "no account snapshot should be emitted on fetch failure"
        );
    }

    #[test]
    fn subscribe_updates_instruments() {
        let creds = OandaCredentials::new("test", "test-account");
        let provider = OandaWebSocketProvider::new(&creds, TradingPlatform::OandaPractice);

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
    async fn poll_and_emit_reports_auth_failure_as_unhealthy() {
        // A 403/401 reports an unhealthy (retryable) cycle rather than signalling a permanent
        // stop -- the caller backs off and retries. No candle is emitted on the failed cycle.
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

        let healthy = poll_and_emit(
            &Client::new(),
            &base_url,
            "test-token",
            &["EUR_USD".to_string()],
            TradingPlatform::OandaPractice,
            &event_tx,
            &mut last_emitted,
        )
        .await;

        assert!(!healthy, "auth failure should mark the cycle unhealthy");
        assert!(rx.try_recv().is_err(), "no candle on auth failure");
    }

    #[tokio::test]
    async fn poll_and_emit_reports_transient_failure_as_unhealthy() {
        // A 5xx marks the cycle unhealthy so the caller backs off -- previously a transient
        // error was swallowed and the cycle reported healthy, retrying only at the 60s cadence.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            500,
            serde_json::json!({ "errorMessage": "boom" }),
        )
        .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let mut last_emitted = HashMap::new();

        let healthy = poll_and_emit(
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
            !healthy,
            "transient failure should mark the cycle unhealthy"
        );
        assert!(rx.try_recv().is_err(), "no candle on transient failure");
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

    #[tokio::test(start_paused = true)]
    async fn run_candle_poll_resumes_after_transient_auth_failure() {
        // A momentary 401 must not latch the candle feed off: the poll backs off and
        // recovers on the next attempt. First request 401, every later request 200+candle.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (server, base_url) = start_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/v3/instruments/EUR_USD/candles"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({ "errorMessage": "transient blip" })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            200,
            serde_json::json!({ "candles": [candle_value("2024-01-15T10:00:00.000000000Z", true)] }),
        )
        .await;

        let instruments = Arc::new(RwLock::new(vec!["EUR_USD".to_string()]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let poll_cancel = CancellationToken::new();
        let parent_cancel = CancellationToken::new();
        let client = Client::new();

        let poll = run_candle_poll(
            &client,
            &base_url,
            "test-token",
            instruments,
            TradingPlatform::OandaPractice,
            event_tx,
            poll_cancel.clone(),
            parent_cancel,
        );
        tokio::pin!(poll);

        let event = tokio::select! {
            () = &mut poll => panic!("candle poll exited on the transient 401 instead of recovering"),
            ev = rx.recv() => ev.expect("poll should emit the recovered candle"),
        };
        match event.msg {
            WsMessage::Candle { bar, .. } => assert_eq!(bar.symbol, "EUR_USD"),
            other => panic!("expected Candle, got {other:?}"),
        }

        // The poll is still alive after recovery -- tear it down cleanly.
        poll_cancel.cancel();
        let _ = poll.await;
    }

    #[tokio::test(start_paused = true)]
    async fn run_candle_poll_gives_up_after_sustained_failure() {
        // A sustained outage (every poll 401s) must eventually give up loudly and return,
        // but only after retrying -- not on the first failure as the old code did.
        let (server, base_url) = start_mock_server().await;
        mount_json(
            &server,
            "GET",
            "/v3/instruments/EUR_USD/candles",
            401,
            serde_json::json!({ "errorMessage": "still down" }),
        )
        .await;

        let instruments = Arc::new(RwLock::new(vec!["EUR_USD".to_string()]));
        let (tx, _rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(RwLock::new(Some(tx)));
        let client = Client::new();

        // Awaited directly under a paused clock: auto-advance fast-forwards the boundary
        // and backoff sleeps, so the poll exhausts its retry budget and returns in real
        // milliseconds. The old code returned after a single 401 (one request); the fix
        // retries before giving up.
        run_candle_poll(
            &client,
            &base_url,
            "test-token",
            instruments,
            TradingPlatform::OandaPractice,
            event_tx,
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await;

        let requests = server
            .received_requests()
            .await
            .expect("mock server should record requests");
        assert_eq!(
            requests.len(),
            MAX_CONSECUTIVE_CANDLE_POLL_FAILURES as usize,
            "candle poll should give up only after exactly the sustained-failure budget"
        );
    }

    #[tokio::test]
    async fn reconnect_cancels_previous_transaction_stream_token() {
        // Regression: on the old code, `connect()` spawned the transaction
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
            "reconnect must cancel the previous transaction-stream token"
        );

        let gen1_token = provider.transaction_stream_cancel.read().await.clone();
        assert!(
            !gen1_token.is_cancelled(),
            "gen-1 transaction-stream token should be live after reconnect"
        );

        provider.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn disconnect_cancels_all_per_task_tokens() {
        // Regression: `disconnect()` previously cancelled only the
        // top-level `cancel_token`. Stream tasks happened to exit anyway because
        // each `select!` block also listened on the parent token, but that
        // invariant was implicit -- a future spawn site wired only to its
        // per-task token would leak past `disconnect()`. The fix cancels each
        // per-task token explicitly. This test pins that contract by snapshotting
        // the three per-task tokens and asserting all of them are cancelled
        // after `disconnect()`.
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

        let _rx = provider
            .connect(config)
            .await
            .expect("connect should succeed");

        let price_token = provider.price_stream_cancel.read().await.clone();
        let candle_token = provider.candle_poll_cancel.read().await.clone();
        let tx_token = provider.transaction_stream_cancel.read().await.clone();
        assert!(
            !price_token.is_cancelled(),
            "price token live before disconnect"
        );
        assert!(
            !candle_token.is_cancelled(),
            "candle token live before disconnect"
        );
        assert!(
            !tx_token.is_cancelled(),
            "transaction token live before disconnect"
        );

        provider.disconnect().await.expect("disconnect");

        assert!(
            price_token.is_cancelled(),
            "disconnect must cancel the price-stream per-task token"
        );
        assert!(
            candle_token.is_cancelled(),
            "disconnect must cancel the candle-poll per-task token"
        );
        assert!(
            tx_token.is_cancelled(),
            "disconnect must cancel the transaction-stream per-task token"
        );
    }
}
