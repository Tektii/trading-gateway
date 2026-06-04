# Strategy Templates

Reference strategies for the Tektii platform. Each template is a runnable,
dockerised Python program built on the [Tektii Python SDK](https://pypi.org/project/tektii/).
They are the same images the platform provisions when you pick "start from
template" in the UI — fork one, tune it to your edge, and point it at the
gateway or the backtest engine.

| Template | Style | Defaults |
|---|---|---|
| [`python/ma_crossover`](python/ma_crossover/) | Trend-following (SMA 10 × SMA 20 crossover, optional bracket SL/TP) | `ORDER_QUANTITY=0.01` |
| [`python/rsi_momentum`](python/rsi_momentum/) | Mean-reversion (RSI 14, oversold/overbought zone entries, optional bracket SL/TP) | `ORDER_QUANTITY=0.01` |

Both templates are **symbol-agnostic** — they trade whatever instrument the
run is subscribed to, learned from the incoming stream. There is no symbol to
configure. Each keeps a single set of indicator state, so subscribe it to one
instrument at a time.

Each template lives in its own directory with `strategy.py`, `test_strategy.py`,
`pyproject.toml`, and a `Dockerfile`. The module docstring at the top of
`strategy.py` is the per-template README — it lists every environment
variable, the local-run command, and the docker command.

## Position sizing — `ORDER_QUANTITY` is not a fraction of capital

`ORDER_QUANTITY` (default `0.01`) is a **fixed instrument quantity** — 0.01
units of the symbol — *not* a fraction of capital. On the engine's default
~100k starting capital, a small fixed size like `0.01` is a near-zero position,
so the template defaults produce near-zero returns and a meaningless Sharpe
until you hand-size `ORDER_QUANTITY` to the instrument's price.

To size by notional or percentage of equity instead, reach for the SDK helper
(`tektii` >= 1.6.0), which returns a quantity at the current price:

```python
qty = await gw.quantity_for_notional(bar.symbol, notional="5000")
# or a share of equity:
qty = await gw.quantity_for_notional(bar.symbol, equity_fraction="0.10")
```

## Running a template locally

Start the gateway with the mock provider — no broker credentials needed:

```bash
docker run -e GATEWAY_PROVIDER=mock -p 8080:8080 ghcr.io/tektii/gateway:latest
```

Then run the template of your choice:

```bash
cd examples/python/ma_crossover
pip install -e .
python strategy.py
```

The SDK reads `TRADING_GATEWAY_URL` (default `http://localhost:8080`) and
`TRADING_GATEWAY_API_KEY` from the environment, so pointing at a remote gateway is
a one-line change.

## Running the tests

Each template ships a small `pytest` suite covering the indicator maths and
one end-to-end `on_candle → submit_order` path via `respx`:

```bash
cd examples/python/ma_crossover
pip install -e .[test]
pytest -q
```

## Docker

Same Dockerfile pattern for both templates. Build and run against a gateway
already listening on the host:

```bash
cd examples/python/ma_crossover
docker build -t tektii-template-ma-crossover:dev .
# Linux: --network=host works. macOS: --network=host doesn't reach the host,
# so point the SDK at the Docker host bridge instead.
docker run --rm \
  -e TRADING_GATEWAY_URL=http://host.docker.internal:8080 \
  tektii-template-ma-crossover:dev
```

The images run as an unprivileged user and exec `python` as PID 1 so
`SIGTERM` is delivered cleanly on `docker stop` — Cloud Run's 10-second
grace shutdown is honoured without extra work.

The `pip install` step pulls `tektii` from PyPI. Once the Python
SDK is published, these images build standalone. Until then, build against
a local SDK checkout by copying the built wheel into the template directory
and adjusting the install line.

## Backtest and live parity

The same strategy binary runs unchanged against a live broker and the Tektii
backtest engine. The SDK coordinates simulated-time progression with the
engine internally — there is no code path for the strategy author to toggle
between modes.

## What about Node.js?

The Node.js SDK is on the roadmap. When it ships, Node templates will live
alongside the Python ones under `examples/nodejs/`. Until then, all
reference strategies are Python.
