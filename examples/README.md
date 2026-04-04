# Examples

Example strategies that connect to Trading Gateway via REST + WebSocket. Both do the same thing: receive synthetic candle data and place market orders when price crosses configurable thresholds.

| Example | Language | Prerequisites |
|---------|----------|---------------|
| [Python](python/strategy.py) | Python 3.10+ | [uv](https://docs.astral.sh/uv/) (or `pip install websockets`) |
| [Node.js](nodejs/strategy.js) | Node.js 18+ | `npm install` |

## Running

Start the gateway with the mock provider (no broker credentials needed):

```bash
docker run -e GATEWAY_PROVIDER=mock -p 8080:8080 ghcr.io/tektii/gateway:latest
```

Then run an example:

```bash
# Python
cd python && uv run strategy.py

# Node.js
cd nodejs && npm install && npm start
```

Both examples demonstrate the ACK pattern required for backtest compatibility with the [Tektii](https://tektii.com) engine. The same code works unchanged against a live broker.
