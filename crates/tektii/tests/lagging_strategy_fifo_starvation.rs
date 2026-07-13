//! System-level engine-contract tests over the FULL production wiring: fake
//! engine WS server → real `TektiiWebSocketProvider` (WS read loop, pending
//! registration, engine ACK writer) → real `ProviderRegistry` broadcast loop
//! → real axum WS server → scripted strategy client. The other ACK tests
//! inject provider events directly and bypass the tektii provider; this file
//! is the only coverage of that chain end to end.
//!
//! The fake engine mirrors the real engine's `AckManager` semantics — exact
//! `event_id` matching, a per-run consecutive-timeout counter that resets
//! only on a matching ACK, a larger cold-start grace before the first ACK,
//! warn-and-continue below threshold — at scaled-down timings. This couples
//! the gateway's ACK relay to the behaviour the engine actually enforces:
//! an SDK-faithful echoing strategy completes the run, and a legacy client
//! that ACKs engine-paced events with empty payloads (the deprecated
//! auto-correlation contract) starves the engine to its halt threshold —
//! the regression record of the halt this crate's strict-ID release fixed.

mod helpers;

use std::time::Duration;

use rust_decimal_macros::dec;
use tektii_gateway_core::websocket::provider::WebSocketProvider;
use tektii_gateway_test_support::harness::{StrategyClient, spawn_test_gateway_without_provider};
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;
use tektii_gateway_test_support::websocket::MockWsServer;
use tektii_protocol::websocket::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;

use tektii_gateway_core::models::TradingPlatform;

/// Engine per-candle ACK window (real engine: 5s).
const ACK_WINDOW: Duration = Duration::from_millis(250);
/// Consecutive-timeout budget before the strategy's first ACK (real engine: 24).
const COLD_START_GRACE: u32 = 6;
/// Consecutive-timeout budget after the first ACK (real engine: 5).
const STEADY_THRESHOLD: u32 = 3;

/// How the scripted strategy acknowledges the frames it receives.
#[derive(Clone, Copy, PartialEq)]
enum AckMode {
    /// SDK-faithful: echo the frame's `event_id` when present, empty otherwise.
    EchoEventId,
    /// Deprecated auto-correlation contract: every ACK has an empty payload.
    AlwaysEmpty,
}

struct EngineOutcome {
    /// Consecutive-timeout count at halt, `None` if the run completed.
    halted_at: Option<u32>,
    /// Candles whose exact `event_id` came back before their window expired.
    candles_acked: usize,
    /// Total `EventAck` frames the engine received (any payload).
    acks_received: usize,
}

/// Drive the fake engine: send one candle per iteration, wait for its ACK
/// within the window, mirroring `AckManager::wait_for_ack` semantics.
async fn run_fake_engine(
    engine_tx: &mpsc::UnboundedSender<String>,
    engine_rx: &mut mpsc::UnboundedReceiver<String>,
    total_candles: usize,
) -> EngineOutcome {
    let mut consecutive_timeouts: u32 = 0;
    let mut ever_acked = false;
    let mut candles_acked = 0;
    let mut acks_received = 0;

    for i in 0..total_candles {
        let event_id = format!("evt-{i}");
        let candle = ServerMessage::candle(
            &event_id,
            "C:BTCUSD",
            "1m",
            1_704_067_200_000 + (i as u64) * 60_000,
            dec!(100),
            dec!(101),
            dec!(99),
            dec!(100.5),
            1000.0,
        );
        engine_tx
            .send(serde_json::to_string(&candle).expect("serialize candle"))
            .expect("engine WS open");

        let deadline = tokio::time::Instant::now() + ACK_WINDOW;
        let mut matched = false;
        loop {
            tokio::select! {
                msg = engine_rx.recv() => {
                    let Some(raw) = msg else { break };
                    let Ok(ClientMessage::EventAck { events_processed, .. }) =
                        serde_json::from_str::<ClientMessage>(&raw)
                    else {
                        continue;
                    };
                    acks_received += 1;
                    if events_processed.iter().any(|id| id == &event_id) {
                        matched = true;
                        break;
                    }
                    // Stray/late ACK for another event: drained and ignored,
                    // exactly like the real AckManager.
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }

        if matched {
            consecutive_timeouts = 0;
            ever_acked = true;
            candles_acked += 1;
        } else {
            consecutive_timeouts += 1;
            let threshold = if ever_acked {
                STEADY_THRESHOLD
            } else {
                COLD_START_GRACE
            };
            if consecutive_timeouts >= threshold {
                return EngineOutcome {
                    halted_at: Some(consecutive_timeouts),
                    candles_acked,
                    acks_received,
                };
            }
            // Below threshold: warn-and-continue to the next candle, like the
            // real simulation loop.
        }
    }

    EngineOutcome {
        halted_at: None,
        candles_acked,
        acks_received,
    }
}

/// Run the scripted strategy over the raw WS stream: read frames, ACK per
/// `mode` after each frame, exactly where the SDK ACKs (after the user body).
fn spawn_strategy(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mode: AckMode,
) -> tokio::task::JoinHandle<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    tokio::spawn(async move {
        let (mut sink, mut source) = stream.split();
        let mut acks_sent = 0usize;
        while let Some(Ok(msg)) = source.next().await {
            let Message::Text(text) = msg else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if value.get("type").and_then(serde_json::Value::as_str) == Some("ping") {
                continue;
            }
            let event_id = value
                .get("event_id")
                .and_then(serde_json::Value::as_str)
                .map(String::from);

            let events_processed: Vec<String> = match mode {
                AckMode::EchoEventId => event_id.into_iter().collect(),
                AckMode::AlwaysEmpty => vec![],
            };
            let ack = serde_json::json!({
                "type": "event_ack",
                "correlation_id": format!("corr-{acks_sent}"),
                "events_processed": events_processed,
                "timestamp": 0,
            });
            let frame = serde_json::to_string(&ack).expect("serialize ack");
            if sink.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
            acks_sent += 1;
        }
    })
}

/// Assemble the full production wiring and run one backtest scenario.
async fn run_scenario(mode: AckMode, total_candles: usize) -> EngineOutcome {
    let (engine_ws, engine_tx, mut engine_rx) = MockWsServer::start().await;

    let adapter = std::sync::Arc::new(MockTradingAdapter::new(TradingPlatform::Tektii));
    let gw = spawn_test_gateway_without_provider(adapter).await;

    // Production order: the strategy attaches first (`wait_for_strategy`),
    // then the gateway connects to the engine and the backtest begins.
    let client = StrategyClient::connect(&gw).await;
    let strategy_task = spawn_strategy(client.into_inner(), mode);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (provider, _broadcast_rx) = helpers::create_ws_provider(&engine_ws.url());
    let stream = provider
        .connect(helpers::minimal_provider_config())
        .await
        .expect("provider connect");
    let bridge = provider.ack_bridge().await.expect("bridge set by connect");
    gw.state
        .provider_registry()
        .set_tektii_ack_bridge(bridge)
        .await;
    gw.state
        .provider_registry()
        .register_provider(
            TradingPlatform::Tektii,
            Box::new(provider),
            stream,
            vec![],
            vec![],
        )
        .await
        .expect("register provider");

    let outcome = run_fake_engine(&engine_tx, &mut engine_rx, total_candles).await;

    strategy_task.abort();
    engine_ws.shutdown().await;
    outcome
}

#[tokio::test]
async fn baseline_echoing_strategy_completes_the_backtest() {
    let outcome = run_scenario(AckMode::EchoEventId, 8).await;
    assert_eq!(
        outcome.halted_at, None,
        "an SDK-faithful strategy must not trip the ACK threshold"
    );
    assert_eq!(outcome.candles_acked, 8, "every candle ACK must round-trip");
}

/// The deprecated auto-correlation contract, exercised end to end: ACKs
/// arrive with an empty `events_processed`, release nothing, nothing paces
/// the engine, and it halts at the cold-start threshold having never seen a
/// single ACK. Documents — deliberately — that empty-acking clients cannot
/// pace a backtest under strict-ID release.
#[tokio::test]
async fn empty_acks_for_paced_candles_starve_the_engine_and_halt_the_run() {
    let outcome = run_scenario(AckMode::AlwaysEmpty, 32).await;
    assert_eq!(
        outcome.halted_at,
        Some(COLD_START_GRACE),
        "empty ACKs must release nothing, so the engine must halt"
    );
    assert_eq!(outcome.candles_acked, 0);
    assert_eq!(
        outcome.acks_received, 0,
        "no ACK may reach the engine when every payload is empty"
    );
}
