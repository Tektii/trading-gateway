# Smoke Tests

Adapter-agnostic smoke tests that validate the gateway's REST API against a real broker sandbox. The same tests run against any provider.

## Prerequisites

- [k6](https://grafana.com/docs/k6/latest/set-up/install-k6/) installed
- A running gateway instance (local binary, Docker container, or remote)

## Running locally

Point at any running gateway:

```bash
k6 run smoke-tests/smoke.js \
  --env GATEWAY_URL=http://localhost:8080 \
  --env SMOKE_TEST_SYMBOL=AAPL
```

Enable the order lifecycle test (submits a far-from-market limit order, fetches it, then cancels):

```bash
k6 run smoke-tests/smoke.js \
  --env GATEWAY_URL=http://localhost:8080 \
  --env SMOKE_TEST_SYMBOL=AAPL \
  --env SMOKE_TEST_ORDER=1 \
  --env SMOKE_ORDER_QTY=1 \
  --env SMOKE_ORDER_PRICE=1.00
```

## Running against a Docker container

```bash
# Build the image
docker build -f docker/Dockerfile -t tektii/gateway:smoke .

# Start with your adapter's credentials (defaults to paper trading)
docker run -d --name gw \
  -e GATEWAY_PROVIDER=alpaca \
  -e ALPACA_API_KEY=pk-xxx \
  -e ALPACA_API_SECRET=sk-xxx \
  -e GATEWAY_HOST=0.0.0.0 \
  -p 8080:8080 \
  tektii/gateway:smoke

# Wait for ready
until curl -sf http://localhost:8080/readyz; do sleep 1; done

# Run smoke tests
k6 run smoke-tests/smoke.js \
  --env GATEWAY_URL=http://localhost:8080 \
  --env SMOKE_TEST_SYMBOL=AAPL \
  --env SMOKE_TEST_ORDER=1

# Cleanup
docker rm -f gw
```

## Gateway environment variables

The gateway requires `GATEWAY_PROVIDER` plus provider-specific credentials. `GATEWAY_MODE` defaults to `paper`.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `GATEWAY_PROVIDER` | Yes | - | Provider to use (see table below) |
| `GATEWAY_MODE` | No | `paper` | Trading mode: `paper` or `live` |
| `GATEWAY_HOST` | No | `127.0.0.1` | Bind address (`0.0.0.0` for Docker) |

### Provider credentials

| `GATEWAY_PROVIDER` | Required credentials |
|---------------------|----------------------|
| `alpaca` | `ALPACA_API_KEY`, `ALPACA_API_SECRET` |
| `binance_spot` | `BINANCE_API_KEY`, `BINANCE_API_SECRET` |
| `binance_futures` | `BINANCE_API_KEY`, `BINANCE_API_SECRET` |
| `binance_margin` | `BINANCE_API_KEY`, `BINANCE_API_SECRET` |
| `binance_coin_futures` | `BINANCE_API_KEY`, `BINANCE_API_SECRET` |
| `oanda` | `OANDA_API_KEY`, `OANDA_ACCOUNT_ID` |
| `saxo` | `SAXO_APP_KEY`, `SAXO_APP_SECRET`, `SAXO_ACCOUNT_KEY` |

## k6 test variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `GATEWAY_URL` | No | `http://localhost:8080` | Base URL of the gateway |
| `SMOKE_TEST_SYMBOL` | No | `AAPL` | Symbol to use for quote/bar/order tests |
| `SMOKE_TEST_ORDER` | No | `0` | Set to `1` to enable order submit+cancel test |
| `SMOKE_ORDER_QTY` | No | `1` | Quantity for the test order |
| `SMOKE_ORDER_PRICE` | No | `1.00` | Limit price (should be far from market) |

## Adapter-specific symbols

| `GATEWAY_PROVIDER` | Symbol | Order price |
|---------------------|--------|-------------|
| `alpaca` | `AAPL` | `1.00` |
| `binance_spot` | `BTCUSDT` | `1000.00` |
| `oanda` | `EUR_USD` | `0.50000` |
| `saxo` | `EURUSD:FXSPOT` | `0.50000` |

## What's tested

| Group | Endpoints | Notes |
|-------|-----------|-------|
| Health | `GET /livez`, `/readyz`, `/health` | Liveness, readiness, provider status |
| Account | `GET /v1/account` | Auth + balance deserialization |
| Market Data | `GET /v1/quotes/{symbol}`, `GET /v1/bars/{symbol}` | Quote and OHLCV bar data |
| System | `GET /v1/capabilities`, `GET /v1/status` | Provider features and connection |
| Orders | `GET /v1/orders`, `GET /v1/orders/history` | Read-only list endpoints |
| Positions | `GET /v1/positions` | Read-only list |
| Trades | `GET /v1/trades` | Read-only list |
| Order Lifecycle | `POST /v1/orders` + `GET /v1/orders/{id}` + `DELETE /v1/orders/{id}` | Submit, fetch, cancel (opt-in) |

## CI

Smoke tests gate every release. On tag push (`v*.*.*`), the release workflow builds the image then runs these tests against all four adapter sandboxes. All must pass before images are published and the GitHub Release is created. See `.github/workflows/release.yml`.
