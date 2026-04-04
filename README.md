# Trading Gateway

A source-available trading API gateway that normalises multiple broker APIs into a single REST + WebSocket interface — the same protocol used by the [Tektii](https://tektii.com) backtesting engine, so strategies go from backtest to live with zero code changes. Built in Rust. Language-agnostic.

[![License: Elastic License 2.0](https://img.shields.io/badge/License-ELv2-blue.svg)](LICENSE)
[![CI](https://img.shields.io/github/actions/workflow/status/tektii/trading-gateway/ci.yml?branch=main&label=CI)](https://github.com/tektii/trading-gateway/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/tektii/trading-gateway?label=release)](https://github.com/tektii/trading-gateway/releases)

Trade through any [supported broker](#supported-brokers-and-exchanges) with a single API. No adapter code. No vendor lock-in. The gateway handles authentication, normalisation, WebSocket streaming, reconnection, and exit management — your strategy code never changes when you switch brokers or move from backtesting to live.

> **Disclaimer:** This software is for informational and educational purposes only. It is not financial advice. Trading financial instruments carries significant risk of loss. Use at your own risk — see [full disclaimer](#disclaimer) below.

> **Note:** The [Tektii backtesting platform](https://tektii.com) is launching soon. The gateway is fully functional as a standalone trading proxy today — the backtesting integration will be available shortly.

## Table of Contents

- [Why Use a Trading Gateway?](#why-use-a-trading-gateway)
- [Architecture](#architecture)
- [Supported Brokers and Exchanges](#supported-brokers-and-exchanges)
- [Quick Start](#quick-start)
- [Place Your First Trade — API Walkthrough](#place-your-first-trade--api-walkthrough)
- [Examples](#examples)
- [Configuration (Environment Variables)](#configuration-environment-variables)
- [API Documentation](#api-documentation)
- [REST API Reference](#rest-api-reference)
- [Exit Management — Stop-Loss, Take-Profit, Trailing Stops](#exit-management--stop-loss-take-profit-trailing-stops)
- [WebSocket API — Real-Time Market Data and Order Events](#websocket-api--real-time-market-data-and-order-events)
- [Security and Authentication](#security-and-authentication)
- [Monitoring](#monitoring)
- [FAQ](#frequently-asked-questions)
- [Disclaimer](#disclaimer)
- [Troubleshooting](#troubleshooting)
- [License](#license)

## Why Use a Trading Gateway?

Every broker speaks a different language. Different REST endpoints, different WebSocket protocols, different auth flows, different order models, different failure modes. You write adapter code for Alpaca. Then you want to try Binance and discover nothing transfers. Your reconnection logic works on one broker and silently drops events on another. Your bracket order implementation is broker-specific spaghetti.

Trading Gateway sits between your strategy and the broker. You code against one REST + WebSocket protocol. The gateway handles the rest.

- **One protocol, any broker** — A single set of REST endpoints and WebSocket events for all supported brokers and exchanges. Language-agnostic: anything that speaks HTTP and WebSocket can connect — Python, Rust, JavaScript, or curl.
- **Broker-agnostic exit management** — Stop-loss, take-profit, and trailing stops that work identically on every broker, whether or not the broker natively supports them. State persists across restarts. Automatic reconnection with exponential backoff. Your strategy places the order and moves on.
- **Backtest-to-live with zero code changes** — The [Tektii](https://tektii.com) backtesting engine implements the same protocol. Write your strategy once, backtest it against the engine, then point it at the gateway for live or paper trading. No adapter code, no if-else branches.

**This is for you if:**
- You run algo trading strategies in any language and want a single multi-broker API
- You want bracket orders and exit management without broker-specific code
- You need reliable reconnection and state persistence for unattended trading

**This is probably not for you if:**
- You only trade on one broker and are happy with their native API
- You need a Python library you can `import` (look at [ccxt](https://github.com/ccxt/ccxt) instead)
- You want a full trading platform with strategy authoring (look at [QuantConnect](https://www.quantconnect.com/))

## Architecture

```
                     Your Strategy
                          |
               REST (HTTP) + WebSocket
                          |
                   +--------------+
                   |   Trading    |
                   |   Gateway    |
                   |              |
                   | - Normalise  |
                   | - Reconnect  |
                   | - Exit Mgmt  |
                   +--------------+
                          |
           +---------+---------+---------+
           |         |         |         |
        Alpaca   Binance    Oanda     Saxo
```

Each gateway instance connects to a single broker, selected at startup via `GATEWAY_PROVIDER`. Your strategy talks to the gateway — never directly to the broker.

## Supported Brokers and Exchanges

| `GATEWAY_PROVIDER` | Feature Flag | Required Credentials |
|---------------------|-------------|----------------------|
| `alpaca` | `alpaca` (default) | `ALPACA_API_KEY`, `ALPACA_API_SECRET` |
| `binance_spot` | `binance` (default) | `BINANCE_API_KEY`, `BINANCE_API_SECRET` |
| `oanda` | `oanda` (default) | `OANDA_API_KEY`, `OANDA_ACCOUNT_ID` |
| `saxo` | `saxo` (default) | `SAXO_APP_KEY`, `SAXO_APP_SECRET`, `SAXO_ACCOUNT_KEY` |
| `tektii` | `tektii` (opt-in) | `TEKTII_ENGINE_URL`, `TEKTII_ENGINE_WS_URL` |

**Paper vs live:** Set `GATEWAY_MODE=paper` (default) or `GATEWAY_MODE=live`. No URL configuration needed.

**Tektii adapter:** Opt-in only. Connects to the [Tektii](https://tektii.com) backtesting platform. Build with `--features tektii` to include it.

**Custom feature builds:** To include only specific adapters:

```bash
cargo build --release --no-default-features --features alpaca,oanda
```

## Quick Start

1. **Configure credentials**

   ```bash
   curl -O https://raw.githubusercontent.com/tektii/trading-gateway/main/.env.example
   cp .env.example .env
   ```

   Open `.env` and fill in your broker credentials. For a quick test with Alpaca paper trading:

   ```
   GATEWAY_PROVIDER=alpaca
   ALPACA_API_KEY=your-key
   ALPACA_API_SECRET=your-secret
   ```

   This defaults to paper trading mode. Set `GATEWAY_MODE=live` when you're ready for real trading.

2. **Start the gateway**

   ```bash
   docker run --env-file .env -p 8080:8080 \
     --read-only --cap-drop=ALL --security-opt=no-new-privileges \
     ghcr.io/tektii/gateway:latest
   ```

   Or with Docker Compose:

   ```bash
   git clone https://github.com/tektii/trading-gateway.git
   cd trading-gateway
   cp .env.example .env      # then fill in your broker credentials
   docker compose up
   ```

3. **Verify it's running**

   ```bash
   curl http://localhost:8080/health
   ```

   You should see:

   ```json
   {"status":"connected","providers":[{"platform":"alpaca-paper","connected":true,"stale_instruments":[]}]}
   ```

4. **Explore the API** — open [http://localhost:8080/swagger-ui](http://localhost:8080/swagger-ui) for interactive documentation.

### Build from Source

```bash
git clone https://github.com/tektii/trading-gateway.git
cd trading-gateway
cp .env.example .env    # edit with your provider + credentials
cargo build --release
cargo run --release      # reads .env automatically
```

## Place Your First Trade — API Walkthrough

With the gateway running against Alpaca Paper (see Quick Start), walk through a full order lifecycle:

**1. Check your account**

```bash
curl http://localhost:8080/v1/account
```

```json
{"balance": "100000.00", "equity": "100000.00", "margin_used": "0", "margin_available": "100000.00", "unrealized_pnl": "0", "currency": "USD"}
```

**2. Get a quote**

```bash
curl http://localhost:8080/v1/quotes/AAPL
```

```json
{"symbol": "AAPL", "bid": "185.50", "ask": "185.55", "last": "185.52", "volume": "52341000", "timestamp": "..."}
```

**3. Buy 10 shares**

```bash
curl -X POST http://localhost:8080/v1/orders \
  -H "Content-Type: application/json" \
  -d '{"symbol": "AAPL", "side": "BUY", "quantity": "10", "order_type": "MARKET"}'
```

```json
{"id": "order_abc123", "status": "PENDING"}
```

**4. Check your orders**

```bash
curl http://localhost:8080/v1/orders
```

The order should show `"status": "FILLED"` within seconds on a market order.

**5. View your position**

```bash
curl http://localhost:8080/v1/positions
```

```json
[{"id": "pos_xyz", "symbol": "AAPL", "side": "LONG", "quantity": "10", "average_entry_price": "185.53", "unrealized_pnl": "-0.10", ...}]
```

**6. Close the position**

```bash
curl -X DELETE http://localhost:8080/v1/positions/pos_xyz
```

This sends a market sell for the full position. You can also close partially by passing `{"quantity": "5"}` in the request body.

For the full API reference, see the [Swagger UI](http://localhost:8080/swagger-ui) or the REST API section below.

## Examples

Both examples do the same thing: connect via WebSocket, receive synthetic candle data for AAPL, and place market orders when the close price crosses configurable thresholds. They demonstrate the ACK pattern required for backtest compatibility with the [Tektii](https://tektii.com) engine. The same code works unchanged against a live broker — just point it at the right gateway.

**First, start the gateway with the mock provider** (no credentials needed):

```bash
docker run -e GATEWAY_PROVIDER=mock -p 8080:8080 ghcr.io/tektii/gateway:latest
```

### Python Strategy

**Prerequisites:** Python 3.10+ and [uv](https://docs.astral.sh/uv/)

```bash
cd examples/python
uv run strategy.py
```

`uv run` reads the inline dependency metadata ([PEP 723](https://peps.python.org/pep-0723/)) and installs `websockets` automatically — no virtual environment or `pip install` step needed.

Without uv: `pip install websockets && python strategy.py`

See [`examples/python/strategy.py`](examples/python/strategy.py) for the full source.

### Node.js Strategy

**Prerequisites:** Node.js 18+

```bash
cd examples/nodejs
npm install
npm start
```

See [`examples/nodejs/strategy.js`](examples/nodejs/strategy.js) for the full source.

### Expected Output

Both examples produce output like:

```
=== Tektii Gateway Example Strategy ===

Symbol:         AAPL
Buy threshold:  $150.05
Sell threshold: $150.15

Account: 100000 USD (equity: 100000)

Connecting to ws://localhost:8080/v1/ws ...
Connected to gateway WebSocket

[AAPL] O=150.13 H=150.58 L=149.63 C=150.08
[MSFT] O=399.93 H=401.77 L=398.73 C=400.57
...
[AAPL] O=149.81 H=150.46 L=149.36 C=150.01
   Close $150.01 < $150.05 — opening long
=> Submitting buy 1 AAPL
   Order accepted: 33de1d89-59a9-493f-a102-62d4bc4449c6
<= Order ORDER_CREATED: AAPL qty=1 status=OPEN
<= Order ORDER_FILLED: AAPL qty=1 status=FILLED
```

## Configuration (Environment Variables)

All configuration is via environment variables. See [`.env.example`](.env.example) for the complete template.

### Server

| Variable | Default | Description |
|----------|---------|-------------|
| `GATEWAY_HOST` | `127.0.0.1` | Bind address |
| `GATEWAY_PORT` | `8080` | Listen port (REST + WebSocket) |
| `GATEWAY_API_KEY` | *(unset)* | API key for authentication (see [Security](#security-and-authentication)) |
| `ENABLE_SWAGGER` | `false` | Enable Swagger UI at `/swagger-ui` |
| `RUST_LOG` | `info` | Log level filter ([`tracing-subscriber`](https://docs.rs/tracing-subscriber) syntax) |

Prometheus metrics are served at `/metrics` on the same port as the REST API.

### Subscriptions

| Variable | Default | Description |
|----------|---------|-------------|
| `SUBSCRIPTIONS` | `[]` | JSON array of market data subscriptions |

Each subscription specifies a platform, instrument, and event types:

```json
[
  {"platform": "alpaca-paper", "instrument": "AAPL", "events": ["quote"]},
  {"platform": "alpaca-paper", "instrument": "MSFT", "events": ["quote", "candle_1m"]}
]
```

Supported event patterns: `quote`, `trade`, `candle_1m`, `candle_5m`, `candle_1h`, `candle_*` (wildcard), `order_update`, `position_update`, `account_update`, `trade_update`, `option_greeks`.

### Reconnection

| Variable | Default | Description |
|----------|---------|-------------|
| `RECONNECT_INITIAL_BACKOFF_MS` | `1000` | Initial reconnection delay (ms) |
| `RECONNECT_MAX_BACKOFF_MS` | `60000` | Maximum backoff delay (ms) |
| `RECONNECT_MAX_DURATION_SECS` | `300` | Give up reconnecting after this duration (seconds) |

### Exit Management

| Variable | Default | Description |
|----------|---------|-------------|
| `SL_TP_TTL_HOURS` | `24` | TTL for stop-loss/take-profit exit orders (hours) |
| `EXIT_STATE_FILE` | `./gateway-exit-state.json` | Path for exit state snapshot (persists across restarts) |

### Provider Selection

| Variable | Default | Description |
|----------|---------|-------------|
| `GATEWAY_PROVIDER` | (required) | Provider to use — see [Supported Brokers](#supported-brokers-and-exchanges) |
| `GATEWAY_MODE` | `paper` | Trading mode: `paper` (sandbox/testnet) or `live` (real money) |

Set the required credential env vars for your chosen provider. See [`.env.example`](.env.example) for all available variables.

## API Documentation

Full API reference is available in two forms:

- **Hosted docs** — [tektii.github.io/trading-gateway](https://tektii.github.io/trading-gateway) — always up to date with `main`
- **Local Swagger UI** — run the gateway with `ENABLE_SWAGGER=true` and open [localhost:8080/swagger-ui](http://localhost:8080/swagger-ui)
- **Raw OpenAPI spec** — [openapi.json](https://tektii.github.io/trading-gateway/openapi.json) (for programmatic access)

## REST API Reference

The gateway exposes a normalised REST API at `/v1/...`. Each gateway instance serves a single trading provider, configured via environment variables at startup.

| Group | Endpoints |
|-------|-----------|
| Account | `GET /v1/account` |
| Orders | `POST /v1/orders`, `GET /v1/orders`, `GET /v1/orders/history`, `GET /v1/orders/{id}`, `PATCH /v1/orders/{id}`, `DELETE /v1/orders/{id}`, `DELETE /v1/orders` |
| Positions | `GET /v1/positions`, `GET /v1/positions/{id}`, `DELETE /v1/positions/{id}`, `DELETE /v1/positions` |
| Trades | `GET /v1/trades` |
| Market Data | `GET /v1/quotes/{symbol}`, `GET /v1/bars/{symbol}` |
| System | `GET /v1/capabilities`, `GET /v1/status` |
| Health | `GET /livez`, `GET /readyz`, `GET /health` |

Full request/response schemas are available at `/swagger-ui`.

## Exit Management — Stop-Loss, Take-Profit, Trailing Stops

The gateway provides broker-agnostic exit management — stop-loss, take-profit, and trailing stops that work identically across all supported brokers, regardless of each broker's native support.

**How it works:**

1. Place an order with `stop_loss` and/or `take_profit` fields — the gateway registers these as pending exit orders.
2. When the parent order fills, the gateway detects the fill and places the exit orders with the broker.
3. If the gateway restarts, exit state is restored from the snapshot file (`EXIT_STATE_FILE`) — no orders are lost.

**Bracket orders:**

```bash
curl -X POST http://localhost:8080/v1/orders \
  -H "Content-Type: application/json" \
  -d '{
    "symbol": "AAPL",
    "side": "BUY",
    "quantity": "100",
    "order_type": "LIMIT",
    "limit_price": "185.00",
    "stop_loss": "180.00",
    "take_profit": "195.00"
  }'
```

The gateway selects the best bracket strategy for each broker (native OCO where supported, synthetic management where not) and handles the full lifecycle: pending entry, fill detection, SL/TP placement, TTL-based cleanup, and circuit breakers for position protection.

**Trailing stops** are also supported — see `trailing_distance` and `trailing_type` in the order request fields.

Configuration: `SL_TP_TTL_HOURS` and `EXIT_STATE_FILE` — see [Configuration](#configuration-environment-variables).

## WebSocket API — Real-Time Market Data and Order Events

### Connection

Connect via WebSocket upgrade at:

```
ws://localhost:8080/v1/ws
```

The connection is immediately ready — no handshake or configuration message required. Events from all registered providers are streamed to all connected clients.

### Subscriptions

Market data subscriptions are **configured at startup** via the `SUBSCRIPTIONS` environment variable. There are no runtime subscribe/unsubscribe messages.

### Heartbeat

The server sends periodic `ping` messages. Respond with `pong` to keep the connection alive.

```json
{"type": "ping", "timestamp": "2025-01-15T10:30:00Z"}
```

```json
{"type": "pong"}
```

### Server → Client Messages

All messages are JSON with `"type"` as the discriminator field.

**Market data:**

```json
{"type": "quote", "quote": {"symbol": "AAPL", "bid": "150.00", "ask": "150.05", "last": "150.02", ...}, "timestamp": "..."}
```

```json
{"type": "candle", "bar": {"symbol": "AAPL", "open": "150.00", "high": "151.00", "low": "149.50", "close": "150.75", "volume": "1000", ...}, "timestamp": "..."}
```

```json
{"type": "trade", "event": "TRADE_FILLED", "trade": {...}, "timestamp": "..."}
```

**Trading events:**

```json
{"type": "order", "event": "ORDER_FILLED", "order": {...}, "timestamp": "..."}
```

Order event types: `ORDER_CREATED`, `ORDER_MODIFIED`, `ORDER_CANCELLED`, `ORDER_REJECTED`, `ORDER_FILLED`, `ORDER_PARTIALLY_FILLED`, `ORDER_EXPIRED`, `BRACKET_ORDER_CREATED`, `BRACKET_ORDER_MODIFIED`.

```json
{"type": "position", "event": "POSITION_OPENED", "position": {...}, "timestamp": "..."}
```

Position event types: `POSITION_OPENED`, `POSITION_MODIFIED`, `POSITION_CLOSED`.

```json
{"type": "account", "event": "BALANCE_UPDATED", "account": {...}, "timestamp": "..."}
```

Account event types: `BALANCE_UPDATED`, `MARGIN_WARNING`, `MARGIN_CALL`.

**Connection state:**

```json
{"type": "connection", "event": "BROKER_DISCONNECTED", "broker": "alpaca-paper", "error": "connection reset", "timestamp": "..."}
```

Connection event types: `CONNECTED`, `DISCONNECTING`, `RECONNECTING`, `BROKER_DISCONNECTED`, `BROKER_RECONNECTED`, `BROKER_CONNECTION_FAILED`. May include `broker`, `error`, and `gap_duration_ms` fields.

**Rate limiting:**

```json
{"type": "rate_limit", "event": "RATE_LIMIT_WARNING", "requests_remaining": 5, "reset_at": "...", "timestamp": "..."}
```

Rate limit event types: `RATE_LIMIT_WARNING`, `RATE_LIMIT_HIT`.

**Errors:**

```json
{"type": "error", "code": "INTERNAL_ERROR", "message": "...", "details": null, "timestamp": "..."}
```

Error codes: `INVALID_MESSAGE`, `INTERNAL_ERROR`, `POSITION_UNPROTECTED`, `LIQUIDATION`.

### Client → Server Messages

**Pong** (heartbeat response):

```json
{"type": "pong"}
```

**Event acknowledgment:**

```json
{
  "type": "event_ack",
  "correlation_id": "550e8400-e29b-41d4-a716-446655440000",
  "events_processed": ["evt_123", "evt_124"],
  "timestamp": 1700000000000
}
```

Event acknowledgment is **required** in simulation mode (it controls time progression in the Tektii backtesting engine) and **informational** in live trading.

## Security and Authentication

### API key authentication

Set `GATEWAY_API_KEY` to require authentication on all trading and data endpoints. When set, every request must include the key as either:

- `Authorization: Bearer <key>`
- `X-API-Key: <key>`

Health (`/livez`, `/readyz`, `/health`) and metrics (`/metrics`) endpoints are always unauthenticated so that orchestrators and monitoring can reach them without credentials.

When `GATEWAY_API_KEY` is **not** set, the gateway runs without authentication — any process that can reach it can place, modify, and cancel orders. A warning is logged at startup in this mode.

### Network-level isolation

Even with API key auth enabled, restrict network access as a defence-in-depth measure:

- **Localhost binding** — The default (`GATEWAY_HOST=127.0.0.1`) binds to localhost only. The gateway is unreachable from other machines.
- **Docker network isolation** — Run the gateway in an isolated Docker network. Only your strategy container shares the network.
- **Kubernetes ClusterIP** — Use a ClusterIP service with no NodePort or LoadBalancer. Only pods in the cluster can reach it.
- **Reverse proxy** — Place nginx, Caddy, or Envoy in front with mTLS for additional protection.

The Docker Compose configuration runs the container read-only with all capabilities dropped (`cap_drop: ALL`, `no-new-privileges`). Broker credentials are held in memory and scrubbed from log output.

## Monitoring

The gateway exposes Prometheus metrics at `GET /metrics` in text exposition format. Scrape this endpoint with Prometheus, Datadog, Grafana Agent, or any compatible collector.

```bash
curl http://localhost:8080/metrics
```

### Order Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `gateway_orders_submitted_total` | Counter | `platform` | Total orders submitted to the broker |
| `gateway_order_submit_duration_seconds` | Histogram | `platform` | Order submission round-trip latency (1ms–10s buckets) |
| `gateway_order_events_total` | Counter | `platform`, `event_type` | Order lifecycle events (filled, cancelled, rejected, expired, partially_filled) |
| `gateway_oco_double_exit_total` | Counter | `platform` | OCO race conditions where both legs filled simultaneously |

### WebSocket Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `gateway_ws_connections_active` | Gauge | — | Current number of connected strategy clients |
| `gateway_ws_slow_consumer_disconnects` | Counter | — | Clients disconnected for not consuming messages fast enough |
| `gateway_critical_notifications_dropped_total` | Counter | `platform` | Critical notifications (e.g. PositionUnprotected) dropped because no clients were connected |

### Broker Connection Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `gateway_broker_disconnections_total` | Counter | `platform` | Broker WebSocket disconnections |
| `gateway_broker_reconnect_attempts_total` | Counter | `platform` | Reconnection attempts to broker |
| `gateway_broker_reconnections_total` | Counter | `platform` | Successful reconnections |
| `gateway_broker_reconnect_failures_total` | Counter | `platform` | Failed reconnections (max retries exceeded) |
| `gateway_rate_limit_events_total` | Counter | `platform`, `event_type` | Rate limit events from broker |

### Other Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `gateway_circuit_breaker_resets_total` | Counter | — | Manual circuit breaker resets via `POST /v1/circuit-breaker/reset` |

## Frequently Asked Questions

**How is this different from ccxt?**

ccxt is a **library** you embed in Python or JavaScript. Trading Gateway is a **standalone service** with a language-agnostic REST + WebSocket protocol — connect from any language that speaks HTTP. A modern alternative to FIX protocol gateways, using REST and WebSocket instead of FIX sessions.

| | Trading Gateway | ccxt | FIX Protocol |
|---|---|---|---|
| Architecture | Standalone service | Embedded library | Session-based |
| Language support | Any (REST + WebSocket) | Python, JS, PHP | C++, Java, C# |
| Exit management | Built-in SL/TP/trailing | Manual | Manual |
| Reconnection | Automatic with backoff | Manual | Library-dependent |
| Backtest parity | Same protocol | No | No |

**Do I need the Tektii backtesting platform?**

No. The gateway is fully standalone. The Tektii adapter is opt-in (`--features tektii`) for users who want backtest-to-live parity with the [Tektii](https://tektii.com) engine.

**Is this production-ready?**

The gateway has 1000+ tests, hardened Docker images, and is used in production. That said, it's v0.1.0 — expect API evolution. Pin your version.

## Disclaimer

This software is provided for **informational and educational purposes only** and does not constitute financial, investment, or trading advice. The authors and contributors are not registered investment advisors, broker-dealers, or financial planners.

**Risk warning:** Trading financial instruments — including equities, futures, options, forex, and cryptocurrency — involves substantial risk of loss and is not suitable for every investor. Automated trading systems carry additional risks including but not limited to software bugs, network failures, unexpected market conditions, and incorrect order execution. Leveraged instruments can amplify losses beyond your initial deposit.

**No warranty:** This software is provided "as is" without warranty of any kind. The authors accept no responsibility for any financial losses, damages, or other consequences resulting from the use of this software. You are solely responsible for evaluating the risks and for any trading decisions you make.

**Before trading with real money:** independently verify all order execution, test thoroughly in paper trading mode, and consult a qualified financial advisor if you are unsure whether trading is appropriate for your situation.

By using this software, you acknowledge that you understand these risks and accept full responsibility for your trading activity.

## Troubleshooting

**`/health` shows `"connected": false`**
Credentials are wrong or the broker is unreachable. Check your `.env` values and that you can reach the broker's API from your network. The health endpoint shows which provider failed to connect.

**`GATEWAY_PROVIDER is required` error at startup**
You must set the `GATEWAY_PROVIDER` environment variable. See [Supported Brokers](#supported-brokers-and-exchanges) for valid values.

**WebSocket connects but no market data arrives**
Check the `SUBSCRIPTIONS` env var. It must be valid JSON — an array of objects with `platform`, `instrument`, and `events` fields. Example: `[{"platform": "alpaca-paper", "instrument": "AAPL", "events": ["quote"]}]`.

**Orders are rejected**
Broker-specific requirements vary. Common causes: Alpaca requires whole shares for some instruments, Binance enforces minimum notional and lot size rules, Oanda requires valid instrument IDs (e.g., `EUR_USD` not `EUR/USD`). Check the error message in the response body.

**Connection drops / frequent reconnections**
The gateway auto-reconnects with exponential backoff. If reconnections are frequent, check your network stability and broker API status pages. Tune backoff behaviour with `RECONNECT_INITIAL_BACKOFF_MS`, `RECONNECT_MAX_BACKOFF_MS`, and `RECONNECT_MAX_DURATION_SECS`.

## License

This project is licensed under the [Elastic License 2.0](LICENSE).

**Allowed:** Use, modify, self-host, embed in your own trading systems, contribute back.

**Not allowed:** Offer Trading Gateway as a hosted or managed service to third parties (i.e., you cannot resell it as SaaS).

See the [LICENSE](LICENSE) file for the full legal text.
