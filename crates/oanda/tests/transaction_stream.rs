//! Transaction-stream delivery semantics: inline emission and exactly-once
//! fills/cancels across the stream and REST paths.
//!
//! wiremock serves the stream body and then closes the connection; the
//! provider's reconnect loop re-fetches the same body after backoff, so each
//! test window of a few seconds exercises delivery across one or more
//! reconnect cycles — the duplicate-burst regression shape.
//!
//! Stream bodies carry a trailing `LIMIT_ORDER` create line as a sentinel:
//! creates are never REST-published so they bypass the dedupe and arrive on
//! every serve cycle. Asserting on the sentinel count proves the stream
//! actually delivered (and how many cycles ran) inside a timing window, so
//! the zero-duplicate assertions cannot pass vacuously on a slow machine.

mod helpers;

use std::time::Duration;

use helpers::{oanda_account_json, oanda_ioc_cancel_json, oanda_market_fill_json, test_adapter};
use rust_decimal_macros::dec;
use serde_json::json;
use tektii_gateway_core::adapter::TradingAdapter;
use tektii_gateway_core::models::{
    Order, OrderRequest, OrderStatus, OrderType, TimeInForce, TradingPlatform,
};
use tektii_gateway_core::websocket::messages::{OrderEventType, WsMessage};
use tektii_gateway_core::websocket::provider::{
    EventStream, ProviderConfig, ProviderEvent, WebSocketProvider,
};
use tektii_gateway_oanda::{OandaCredentials, OandaWebSocketProvider};
use tektii_gateway_test_support::models::test_order_request;
use tektii_gateway_test_support::wiremock_helpers::{mount_json, mount_text, start_mock_server};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

const STREAM_PATH: &str = "/v3/accounts/test-account-123/transactions/stream";
const ORDERS_PATH: &str = "/v3/accounts/test-account-123/orders";
const SUMMARY_PATH: &str = "/v3/accounts/test-account-123/summary";

/// One reconnect cycle (1s initial backoff) plus slack.
const ONE_RECONNECT: Duration = Duration::from_millis(1600);
/// Two reconnect cycles (1s + 2s backoff) plus slack.
const TWO_RECONNECTS: Duration = Duration::from_millis(3400);

fn test_provider(base_url: &str) -> OandaWebSocketProvider {
    let credentials = OandaCredentials::new("test-token", "test-account-123")
        .with_rest_url(base_url)
        .with_stream_url(base_url);
    OandaWebSocketProvider::new(&credentials, TradingPlatform::OandaPractice)
}

fn provider_config() -> ProviderConfig {
    ProviderConfig {
        platform: TradingPlatform::OandaPractice,
        symbols: vec![],
        event_types: vec!["order".into()],
        credentials: None,
        tektii_params: None,
    }
}

fn order_fill_line(id: &str, order_id: &str, units: &str) -> String {
    json!({
        "type": "ORDER_FILL",
        "id": id,
        "orderID": order_id,
        "instrument": "EUR_USD",
        "units": units,
        "price": "1.10000",
        "time": "2024-01-15T10:30:00.000000000Z"
    })
    .to_string()
}

fn order_cancel_line(id: &str, order_id: &str) -> String {
    json!({
        "type": "ORDER_CANCEL",
        "id": id,
        "orderID": order_id,
        "reason": "TIME_IN_FORCE_EXPIRED",
        "time": "2024-01-15T10:30:00.000000000Z"
    })
    .to_string()
}

/// A `LIMIT_ORDER` create — never REST-published, so never deduped. Emitted
/// as `OrderCreated` on every serve cycle (see module docs).
fn sentinel_line(id: &str) -> String {
    json!({
        "type": "LIMIT_ORDER",
        "id": id,
        "instrument": "EUR_USD",
        "units": "1000",
        "price": "1.00000",
        "time": "2024-01-15T10:30:00.000000000Z"
    })
    .to_string()
}

/// Drain everything the stream delivers within `window`.
async fn collect_events(rx: &mut EventStream, window: Duration) -> Vec<WsMessage> {
    let deadline = tokio::time::Instant::now() + window;
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        events.push(event.msg);
    }
    events
}

/// Drain everything already queued on a channel.
fn drain_events(rx: &mut mpsc::UnboundedReceiver<ProviderEvent>) -> Vec<WsMessage> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.msg);
    }
    events
}

fn count_order_events(events: &[WsMessage], wanted: OrderEventType) -> usize {
    events
        .iter()
        .filter(|m| matches!(m, WsMessage::Order { event, .. } if *event == wanted))
        .count()
}

fn find_order_event<'a>(
    events: &'a [WsMessage],
    order_id: &str,
    wanted: OrderEventType,
) -> Option<&'a Order> {
    events.iter().find_map(|m| match m {
        WsMessage::Order { event, order, .. } if *event == wanted && order.id == order_id => {
            Some(order)
        }
        _ => None,
    })
}

fn count_trades(events: &[WsMessage]) -> usize {
    events
        .iter()
        .filter(|m| matches!(m, WsMessage::Trade { .. }))
        .count()
}

fn count_accounts(events: &[WsMessage]) -> usize {
    events
        .iter()
        .filter(|m| matches!(m, WsMessage::Account { .. }))
        .count()
}

#[tokio::test]
async fn stream_fill_delivered_exactly_once_across_reconnects() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n",
            order_fill_line("9001", "9000", "10000"),
            sentinel_line("9002")
        ),
    )
    .await;

    let provider = test_provider(&base_url);
    let mut rx = provider.connect(provider_config()).await.unwrap();

    // Window spans two reconnect cycles, each re-serving the same body.
    let events = collect_events(&mut rx, TWO_RECONNECTS).await;

    let cycles = count_order_events(&events, OrderEventType::OrderCreated);
    assert!(
        cycles >= 2,
        "expected at least two serve cycles in the window, saw {cycles}"
    );
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderFilled),
        1,
        "stream-only fill must be delivered exactly once across reconnects"
    );
    assert_eq!(
        count_trades(&events),
        1,
        "the fill's trade frame must be delivered exactly once across reconnects"
    );
    assert_eq!(
        count_accounts(&events),
        1,
        "the fill's account snapshot must be delivered exactly once across reconnects"
    );
}

#[tokio::test]
async fn rest_fill_suppresses_stream_duplicate() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;
    mount_json(
        &server,
        "POST",
        ORDERS_PATH,
        201,
        oanda_market_fill_json(&json!({"id": "9101", "orderID": "9100"})),
    )
    .await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n",
            order_fill_line("9101", "9100", "10000"),
            sentinel_line("9102")
        ),
    )
    .await;

    // REST fill publishes first, on the adapter's own channel.
    let adapter = test_adapter(&base_url);
    let (tx, mut rest_rx) = mpsc::unbounded_channel::<ProviderEvent>();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Filled);

    let rest_events = drain_events(&mut rest_rx);
    assert_eq!(
        count_order_events(&rest_events, OrderEventType::OrderFilled),
        1,
        "REST publishes the fill"
    );
    assert_eq!(
        count_trades(&rest_events),
        1,
        "REST publishes the fill's trade frame (parity with the stream shape)"
    );
    assert_eq!(
        count_accounts(&rest_events),
        1,
        "REST publishes the fill's account snapshot"
    );

    // The stream then delivers the same fill transaction — it must be
    // suppressed wholesale: no order frame, no account snapshot.
    let provider = test_provider(&base_url);
    let mut stream_rx = provider.connect(provider_config()).await.unwrap();
    let events = collect_events(&mut stream_rx, ONE_RECONNECT).await;

    assert!(
        count_order_events(&events, OrderEventType::OrderCreated) >= 1,
        "sentinel must prove the stream delivered within the window"
    );
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderFilled),
        0,
        "stream copy of a REST-published fill must be suppressed"
    );
    assert_eq!(
        count_trades(&events),
        0,
        "a suppressed fill must not emit a trade frame"
    );
    assert_eq!(
        count_accounts(&events),
        0,
        "a suppressed fill must not emit an account snapshot"
    );
}

#[tokio::test]
async fn stream_fill_suppresses_rest_duplicate() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;
    mount_json(
        &server,
        "POST",
        ORDERS_PATH,
        201,
        oanda_market_fill_json(&json!({"id": "9201", "orderID": "9200"})),
    )
    .await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n",
            order_fill_line("9201", "9200", "10000"),
            sentinel_line("9202")
        ),
    )
    .await;

    // Adapter and provider share one outbound channel, as wired in production.
    let adapter = test_adapter(&base_url);
    let provider = test_provider(&base_url).with_event_tx(adapter.provider_event_tx_handle());
    let mut rx = provider.connect(provider_config()).await.unwrap();

    // Wait until the stream has delivered the fill (skipping the sentinel).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("stream delivers the fill in real time")
            .expect("stream channel open");
        if matches!(
            event.msg,
            WsMessage::Order {
                event: OrderEventType::OrderFilled,
                ..
            }
        ) {
            break;
        }
    }

    // The REST response for the same transaction must not publish a second
    // fill, but the order handle itself is unaffected.
    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "9200");
    assert_eq!(handle.status, OrderStatus::Filled);

    let events = collect_events(&mut rx, ONE_RECONNECT).await;
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderFilled),
        0,
        "REST copy of a stream-published fill must be suppressed"
    );
    // The stream's own snapshot follows its fill; the suppressed REST copy
    // must not add another (reconnect re-serves are deduped too).
    assert_eq!(
        count_accounts(&events),
        1,
        "exactly one account snapshot: the stream's own, none from REST"
    );
}

#[tokio::test]
async fn rest_cancel_suppresses_stream_duplicate() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "POST", ORDERS_PATH, 201, oanda_ioc_cancel_json()).await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n",
            order_cancel_line("1982", "1981"),
            sentinel_line("1983")
        ),
    )
    .await;

    // REST publishes the synchronous IOC cancel first.
    let adapter = test_adapter(&base_url);
    let (tx, mut rest_rx) = mpsc::unbounded_channel::<ProviderEvent>();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        order_type: OrderType::Limit,
        limit_price: Some(dec!(1.05)),
        time_in_force: TimeInForce::Ioc,
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Cancelled);

    let rest_event = rest_rx.recv().await.expect("REST cancel published");
    assert!(matches!(
        rest_event.msg,
        WsMessage::Order {
            event: OrderEventType::OrderCancelled,
            ..
        }
    ));

    // The stream's copy of the same ORDER_CANCEL transaction must be suppressed.
    let provider = test_provider(&base_url);
    let mut stream_rx = provider.connect(provider_config()).await.unwrap();
    let events = collect_events(&mut stream_rx, ONE_RECONNECT).await;

    assert!(
        count_order_events(&events, OrderEventType::OrderCreated) >= 1,
        "sentinel must prove the stream delivered within the window"
    );
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderCancelled),
        0,
        "stream copy of a REST-published cancel must be suppressed"
    );
}

#[tokio::test]
async fn stream_cancel_suppresses_rest_duplicate() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "POST", ORDERS_PATH, 201, oanda_ioc_cancel_json()).await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n",
            order_cancel_line("1982", "1981"),
            sentinel_line("1984")
        ),
    )
    .await;

    // Adapter and provider share one outbound channel; the stream delivers
    // the ORDER_CANCEL before the REST response is processed.
    let adapter = test_adapter(&base_url);
    let provider = test_provider(&base_url).with_event_tx(adapter.provider_event_tx_handle());
    let mut rx = provider.connect(provider_config()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("stream delivers the cancel in real time")
            .expect("stream channel open");
        if matches!(
            event.msg,
            WsMessage::Order {
                event: OrderEventType::OrderCancelled,
                ..
            }
        ) {
            break;
        }
    }

    // The REST response carries the same cancel transaction — its copy must
    // be suppressed while the order handle is unaffected.
    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        order_type: OrderType::Limit,
        limit_price: Some(dec!(1.05)),
        time_in_force: TimeInForce::Ioc,
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Cancelled);

    let events = collect_events(&mut rx, ONE_RECONNECT).await;
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderCancelled),
        0,
        "REST copy of a stream-published cancel must be suppressed"
    );
}

#[tokio::test]
async fn partial_ioc_fill_and_cancel_published_exactly_once_as_a_pair() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;

    // REST response: partial fill (6000 of 10000) plus the remainder-cancel.
    let mut body = oanda_market_fill_json(&json!({
        "id": "9401",
        "orderID": "9400",
        "units": "6000"
    }));
    body["orderCancelTransaction"] = json!({
        "id": "9402",
        "type": "ORDER_CANCEL",
        "orderID": "9400",
        "reason": "TIME_IN_FORCE_EXPIRED",
        "time": "2024-01-15T10:30:00.000000000Z"
    });
    mount_json(&server, "POST", ORDERS_PATH, 201, body).await;

    // The stream re-delivers both transactions of the pair.
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n{}\n",
            order_fill_line("9401", "9400", "6000"),
            order_cancel_line("9402", "9400"),
            sentinel_line("9403")
        ),
    )
    .await;

    // REST publishes the pair first, on the adapter's own channel.
    let adapter = test_adapter(&base_url);
    let (tx, mut rest_rx) = mpsc::unbounded_channel::<ProviderEvent>();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        order_type: OrderType::Limit,
        limit_price: Some(dec!(1.10)),
        time_in_force: TimeInForce::Ioc,
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::PartiallyFilled);

    let rest_events = drain_events(&mut rest_rx);
    assert_eq!(
        count_order_events(&rest_events, OrderEventType::OrderPartiallyFilled),
        1
    );
    assert_eq!(count_trades(&rest_events), 1);
    assert_eq!(
        count_order_events(&rest_events, OrderEventType::OrderCancelled),
        1
    );
    assert_eq!(count_accounts(&rest_events), 1);

    // Both stream copies of the pair must be suppressed.
    let provider = test_provider(&base_url);
    let mut stream_rx = provider.connect(provider_config()).await.unwrap();
    let events = collect_events(&mut stream_rx, ONE_RECONNECT).await;

    assert!(
        count_order_events(&events, OrderEventType::OrderCreated) >= 1,
        "sentinel must prove the stream delivered within the window"
    );
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderFilled)
            + count_order_events(&events, OrderEventType::OrderPartiallyFilled),
        0,
        "stream copy of the pair's fill must be suppressed"
    );
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderCancelled),
        0,
        "stream copy of the pair's cancel must be suppressed"
    );
    assert_eq!(count_accounts(&events), 0);
}

#[tokio::test]
async fn rest_full_fill_claims_accompanying_cancel() {
    // OANDA can return a fill plus a cancel transaction in one response; the
    // fill wins and no cancel event reaches strategies. The cancel's id must
    // still be claimed, so the stream's copy cannot deliver a contradictory
    // OrderCancelled after the OrderFilled.
    let (server, base_url) = start_mock_server().await;

    let mut body = oanda_market_fill_json(&json!({"id": "9501", "orderID": "9500"}));
    body["orderCancelTransaction"] = json!({
        "id": "9502",
        "type": "ORDER_CANCEL",
        "orderID": "9500",
        "reason": "TIME_IN_FORCE_EXPIRED",
        "time": "2024-01-15T10:30:00.000000000Z"
    });
    mount_json(&server, "POST", ORDERS_PATH, 201, body).await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n",
            order_cancel_line("9502", "9500"),
            sentinel_line("9503")
        ),
    )
    .await;

    let adapter = test_adapter(&base_url);
    let (tx, mut rest_rx) = mpsc::unbounded_channel::<ProviderEvent>();
    *adapter.provider_event_tx_handle().write().await = Some(tx);

    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Filled);

    let rest_events = drain_events(&mut rest_rx);
    assert_eq!(
        count_order_events(&rest_events, OrderEventType::OrderFilled),
        1
    );
    assert_eq!(
        count_order_events(&rest_events, OrderEventType::OrderCancelled),
        0,
        "the fill wins; REST publishes no cancel"
    );

    let provider = test_provider(&base_url);
    let mut stream_rx = provider.connect(provider_config()).await.unwrap();
    let events = collect_events(&mut stream_rx, ONE_RECONNECT).await;

    assert!(
        count_order_events(&events, OrderEventType::OrderCreated) >= 1,
        "sentinel must prove the stream delivered within the window"
    );
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderCancelled),
        0,
        "the stream's copy of the swallowed cancel must be suppressed"
    );
}

#[tokio::test]
async fn unconnected_adapter_does_not_claim_fill_away_from_stream() {
    // A REST fill processed before any strategy channel exists must not be
    // claimed — once a channel connects, the stream's copy is the only
    // delivery left and must go through.
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;
    mount_json(
        &server,
        "POST",
        ORDERS_PATH,
        201,
        oanda_market_fill_json(&json!({"id": "9601", "orderID": "9600"})),
    )
    .await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!("{}\n", order_fill_line("9601", "9600", "10000")),
    )
    .await;

    // No provider event channel wired: the REST publish is skipped.
    let adapter = test_adapter(&base_url);
    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.status, OrderStatus::Filled);

    // The stream copy must then be delivered (exactly once across re-serves).
    let provider = test_provider(&base_url).with_event_tx(adapter.provider_event_tx_handle());
    let mut rx = provider.connect(provider_config()).await.unwrap();
    let events = collect_events(&mut rx, ONE_RECONNECT).await;

    assert_eq!(
        count_order_events(&events, OrderEventType::OrderFilled),
        1,
        "the stream must deliver the fill the unconnected REST path skipped"
    );
}

#[tokio::test]
async fn internal_stream_delivers_fills_independent_of_strategy_dedupe() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;
    mount_json(
        &server,
        "POST",
        ORDERS_PATH,
        201,
        oanda_market_fill_json(&json!({"id": "9301", "orderID": "9300"})),
    )
    .await;
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!("{}\n", order_fill_line("9301", "9300", "10000")),
    )
    .await;

    // The REST path publishes the fill to strategy clients first.
    let adapter = test_adapter(&base_url);
    let (tx, _rest_rx) = mpsc::unbounded_channel::<ProviderEvent>();
    *adapter.provider_event_tx_handle().write().await = Some(tx);
    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        ..test_order_request()
    };
    adapter.submit_order(&request).await.unwrap();

    // The internal EventRouter stream feeds state management, not strategy
    // clients — it must still receive the fill.
    let provider = test_provider(&base_url);
    let (internal_tx, mut internal_rx) = broadcast::channel(16);
    provider
        .connect_internal_trading_stream(
            internal_tx,
            CancellationToken::new(),
            TradingPlatform::OandaPractice,
        )
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), internal_rx.recv())
        .await
        .expect("internal stream delivers the fill in real time")
        .expect("internal channel open");
    assert!(matches!(
        event.message,
        WsMessage::Order {
            event: OrderEventType::OrderFilled,
            ..
        }
    ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn stream_won_partial_ioc_delivers_wire_shape_pair_and_suppresses_rest() {
    // When the stream wins the race for a partial IOC, strategies receive the
    // wire shape — OrderFilled carrying the filled units, then the terminal
    // OrderCancelled — in that order, with the order context derived from the
    // fill's `reason`. The REST response's copies of both transactions must
    // then be suppressed wholesale.
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;

    // REST response: partial fill (6000 of 10000) plus the remainder-cancel.
    let mut body = oanda_market_fill_json(&json!({
        "id": "9701",
        "orderID": "9700",
        "units": "6000"
    }));
    body["orderCancelTransaction"] = json!({
        "id": "9702",
        "type": "ORDER_CANCEL",
        "orderID": "9700",
        "reason": "TIME_IN_FORCE_EXPIRED",
        "time": "2024-01-15T10:30:00.000000000Z"
    });
    mount_json(&server, "POST", ORDERS_PATH, 201, body).await;

    // Stream copy of the same pair, with the fill reason an IOC limit carries.
    let fill_line = json!({
        "type": "ORDER_FILL",
        "id": "9701",
        "orderID": "9700",
        "instrument": "EUR_USD",
        "units": "6000",
        "price": "1.10000",
        "reason": "LIMIT_ORDER",
        "time": "2024-01-15T10:30:00.000000000Z"
    })
    .to_string();
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n{}\n",
            fill_line,
            order_cancel_line("9702", "9700"),
            sentinel_line("9703")
        ),
    )
    .await;

    // Adapter and provider share one outbound channel, as wired in production;
    // the stream delivers the pair before the REST response is processed.
    let adapter = test_adapter(&base_url);
    let provider = test_provider(&base_url).with_event_tx(adapter.provider_event_tx_handle());
    let mut rx = provider.connect(provider_config()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("stream delivers the pair in real time")
            .expect("stream channel open");
        let done = matches!(
            event.msg,
            WsMessage::Order {
                event: OrderEventType::OrderCancelled,
                ..
            }
        );
        events.push(event.msg);
        if done {
            break;
        }
    }

    let fill_pos = events
        .iter()
        .position(
            |m| matches!(m, WsMessage::Order { event, .. } if *event == OrderEventType::OrderFilled),
        )
        .expect("stream publishes the fill before the cancel");

    match &events[fill_pos] {
        WsMessage::Order { order, .. } => {
            assert_eq!(order.id, "9700");
            assert_eq!(order.order_type, OrderType::Limit, "derived from reason");
            assert_eq!(
                order.time_in_force,
                TimeInForce::Gtc,
                "wire carries no TIF; the contract's documented best-effort"
            );
            assert_eq!(order.filled_quantity, dec!(6000));
            assert_eq!(order.status, OrderStatus::Filled, "wire shape");
        }
        other => panic!("expected the fill's Order frame, got {other:?}"),
    }
    assert_eq!(count_trades(&events), 1, "the fill's trade frame");
    assert_eq!(count_accounts(&events), 1, "the fill's account snapshot");

    // The REST response carries the same pair — both copies must be
    // suppressed while the order handle still reports the partial fill.
    let request = OrderRequest {
        symbol: "EUR_USD".into(),
        quantity: dec!(10000),
        order_type: OrderType::Limit,
        limit_price: Some(dec!(1.10)),
        time_in_force: TimeInForce::Ioc,
        ..test_order_request()
    };
    let handle = adapter.submit_order(&request).await.unwrap();
    assert_eq!(handle.id, "9700");
    assert_eq!(handle.status, OrderStatus::PartiallyFilled);

    let later = collect_events(&mut rx, ONE_RECONNECT).await;
    assert!(
        count_order_events(&later, OrderEventType::OrderCreated) >= 1,
        "sentinel must prove the stream is still delivering within the window"
    );
    assert_eq!(
        count_order_events(&later, OrderEventType::OrderFilled)
            + count_order_events(&later, OrderEventType::OrderPartiallyFilled),
        0,
        "REST copy of the stream-published fill must be suppressed"
    );
    assert_eq!(
        count_order_events(&later, OrderEventType::OrderCancelled),
        0,
        "REST copy of the stream-published cancel must be suppressed"
    );
    assert_eq!(count_trades(&later), 0);
    assert_eq!(
        count_accounts(&later),
        0,
        "a suppressed REST fill must not add an account snapshot"
    );
}

/// A native-bracket dependent leg create (`STOP_LOSS_ORDER` / `TAKE_PROFIT_ORDER`).
/// OANDA creates these server-side on the entry fill; the wire line references a
/// trade and carries no instrument/units, only the trigger price.
fn dependent_leg_line(id: &str, leg_type: &str, price: &str) -> String {
    json!({
        "type": leg_type,
        "id": id,
        "price": price,
        "time": "2024-01-15T10:30:00.000000000Z"
    })
    .to_string()
}

/// The C09 native-bracket race, at the gateway boundary: the marketable
/// take-profit fills and closes the trade, the stop-loss loser surfaces only
/// afterwards, then OANDA's own OCO cancels it because its trade is gone. The
/// gateway owns no exit state for a native bracket (OANDA manages the OCO), so
/// its sole job is to surface the loser's full lifecycle — `OrderCreated` then
/// the terminal `OrderCancelled` — rather than leaving it resting `Open` and
/// orphaned. This regression-guards the leg-surfacing + cancel-forwarding path
/// for the exact sequence that orphaned the stop-loss in canary C09.
#[tokio::test]
async fn native_bracket_stop_loss_loser_surfaces_and_then_terminates() {
    let (server, base_url) = start_mock_server().await;
    mount_json(&server, "GET", SUMMARY_PATH, 200, oanda_account_json()).await;

    // The take-profit fills and closes the long trade; the stop-loss leg lands
    // on the stream only after the close (the race); OANDA then cancels it as a
    // linked-trade-closed loser.
    let take_profit_fill = json!({
        "type": "ORDER_FILL",
        "id": "6207",
        "orderID": "6206",
        "instrument": "EUR_USD",
        "units": "-10000",
        "price": "1.10100",
        "reason": "TAKE_PROFIT_ORDER",
        "time": "2024-01-15T10:30:00.000000000Z"
    })
    .to_string();
    let stop_loss_cancel = json!({
        "type": "ORDER_CANCEL",
        "id": "6209",
        "orderID": "6208",
        "reason": "LINKED_TRADE_CLOSED",
        "time": "2024-01-15T10:30:00.000000000Z"
    })
    .to_string();
    mount_text(
        &server,
        "GET",
        STREAM_PATH,
        200,
        &format!(
            "{}\n{}\n{}\n{}\n{}\n",
            order_fill_line("6204", "6203", "10000"),
            dependent_leg_line("6206", "TAKE_PROFIT_ORDER", "1.10100"),
            take_profit_fill,
            dependent_leg_line("6208", "STOP_LOSS_ORDER", "1.09900"),
            stop_loss_cancel,
        ),
    )
    .await;

    let provider = test_provider(&base_url);
    let mut rx = provider.connect(provider_config()).await.unwrap();

    let events = collect_events(&mut rx, TWO_RECONNECTS).await;

    // The stop-loss loser surfaced as a resting Stop.
    let sl_create = find_order_event(&events, "6208", OrderEventType::OrderCreated)
        .expect("the stop-loss loser must surface as an OrderCreated");
    assert_eq!(sl_create.order_type, OrderType::Stop);
    assert_eq!(sl_create.status, OrderStatus::Open);

    // ...and then reached a terminal Cancelled state exactly once (deduped
    // across reconnects), so nothing rests orphaned after the position is flat.
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderCancelled),
        1,
        "the stop-loss loser must terminate (OrderCancelled), exactly once"
    );

    // The entry fill that opened the position and the take-profit fill that
    // closed it are each delivered exactly once across the reconnect window.
    assert_eq!(
        count_order_events(&events, OrderEventType::OrderFilled),
        2,
        "entry fill + take-profit fill, each delivered exactly once"
    );
}
