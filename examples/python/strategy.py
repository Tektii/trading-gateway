# /// script
# requires-python = ">=3.10"
# dependencies = ["websockets>=12.0"]
# ///
#
# Example strategy for the Tektii Trading Gateway.
#
# Connects via WebSocket, receives candle events from the mock provider,
# and places market orders when the price crosses simple thresholds.
# Demonstrates the ACK pattern required for backtest compatibility
# with the Tektii engine.
#
# DISCLAIMER: This is an educational example, not investment advice.
# This strategy has no risk management — no stop losses, no position
# sizing, no exposure limits. Never trade real money without proper
# risk controls.
#
# Usage:
#   # Terminal 1 — start the gateway with mock provider
#   docker run -e GATEWAY_PROVIDER=mock -p 8080:8080 ghcr.io/tektii/gateway:latest
#
#   # Terminal 2 — run this strategy (uv handles dependencies automatically)
#   cd examples/python && uv run strategy.py
#
#   # Or without uv:
#   pip install websockets && python strategy.py

import asyncio
import json
import os
import sys
import urllib.request
import urllib.error
from dataclasses import dataclass
from decimal import Decimal
from uuid import uuid4

# ---------------------------------------------------------------------------
# Configuration (override via environment variables)
# ---------------------------------------------------------------------------
GATEWAY_URL = os.environ.get("GATEWAY_URL", "http://localhost:8080")
WS_URL = os.environ.get("WS_URL", "ws://localhost:8080/v1/ws")
SYMBOL = os.environ.get("SYMBOL", "AAPL")

# Mock provider starts AAPL at ~$150 with ±0.1% random walk (~$0.15/step).
# Tight thresholds ensure trades trigger within a few seconds of starting.
BUY_THRESHOLD = Decimal(os.environ.get("BUY_THRESHOLD", "150.05"))
SELL_THRESHOLD = Decimal(os.environ.get("SELL_THRESHOLD", "150.15"))

# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------


@dataclass
class State:
    has_position: bool = False
    # Stays True from order submission until ORDER_FILLED is received,
    # preventing duplicate orders during the ~500ms mock fill delay.
    pending_order: bool = False


state = State()

# ---------------------------------------------------------------------------
# REST helpers (stdlib urllib — no extra dependency needed)
# ---------------------------------------------------------------------------


def get_account():
    req = urllib.request.Request(f"{GATEWAY_URL}/v1/account")
    with urllib.request.urlopen(req) as resp:
        if resp.status != 200:
            raise RuntimeError(f"GET /v1/account failed: {resp.status}")
        return json.loads(resp.read())


def submit_order(symbol, side, quantity):
    # The REST API accepts lowercase ("buy"/"sell"/"market"/"gtc") via serde
    # aliases. WebSocket events return SCREAMING_SNAKE_CASE ("BUY"/"SELL").
    # Quantity is a string to avoid floating-point precision issues with
    # fractional quantities (e.g., "0.001" for crypto).
    body = json.dumps({
        "symbol": symbol,
        "side": side,
        "quantity": quantity,
        "order_type": "market",
        "time_in_force": "gtc",
    }).encode()

    print(f"=> Submitting {side} {quantity} {symbol}")

    req = urllib.request.Request(
        f"{GATEWAY_URL}/v1/orders",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    try:
        with urllib.request.urlopen(req) as resp:
            order = json.loads(resp.read())
            print(f"   Order accepted: {order['id']}")
            return order
    except urllib.error.HTTPError as e:
        text = e.read().decode()
        print(f"   Order rejected ({e.code}): {text}")
        return None


# ---------------------------------------------------------------------------
# ACK helper — required for Tektii engine backtesting, no-op for live/mock
# ---------------------------------------------------------------------------


def build_ack(event_ids):
    if not event_ids:
        return None
    return json.dumps({
        "type": "event_ack",
        "correlation_id": str(uuid4()),
        "events_processed": event_ids,
        "timestamp": int(asyncio.get_event_loop().time() * 1000),
    })


# ---------------------------------------------------------------------------
# Candle handler — the core trading logic
# ---------------------------------------------------------------------------


def handle_candle(bar):
    # Decimal values arrive as strings from the gateway (rust_decimal serialisation).
    close = Decimal(bar["close"])
    open_ = Decimal(bar["open"])
    high = Decimal(bar["high"])
    low = Decimal(bar["low"])

    print(f"[{bar['symbol']}] O={open_:.2f} H={high:.2f} L={low:.2f} C={close:.2f}")

    if bar["symbol"] != SYMBOL:
        return None
    if state.pending_order:
        return None

    if not state.has_position and close < BUY_THRESHOLD:
        print(f"   Close ${close:.2f} < ${BUY_THRESHOLD} — opening long")
        state.pending_order = True
        try:
            submit_order(SYMBOL, "buy", "1")
        except Exception as err:
            print(f"   Order failed: {err}")
            state.pending_order = False

    elif state.has_position and close > SELL_THRESHOLD:
        print(f"   Close ${close:.2f} > ${SELL_THRESHOLD} — closing position")
        state.pending_order = True
        try:
            submit_order(SYMBOL, "sell", "1")
        except Exception as err:
            print(f"   Order failed: {err}")
            state.pending_order = False


# ---------------------------------------------------------------------------
# WebSocket connection
# ---------------------------------------------------------------------------


async def connect():
    import websockets

    while True:
        print(f"Connecting to {WS_URL} ...")
        try:
            async with websockets.connect(WS_URL) as ws:
                print("Connected to gateway WebSocket\n")

                async for raw in ws:
                    msg = json.loads(raw)

                    # --- ACK pattern (engine compatibility) ---
                    # The Tektii backtesting engine includes an event_id on
                    # every message. Collecting and acknowledging these IDs
                    # controls time progression in the engine. Against the
                    # mock/live gateway, event_id is absent and this code is
                    # a harmless no-op — but it means this same strategy can
                    # run against the engine with zero changes.
                    event_ids = []
                    if "event_id" in msg:
                        event_ids.append(msg["event_id"])

                    match msg:
                        case {"type": "ping"}:
                            await ws.send(json.dumps({"type": "pong"}))

                        case {"type": "candle", "bar": bar}:
                            handle_candle(bar)

                        case {"type": "order", "event": event, "order": order}:
                            print(
                                f"<= Order {event}: {order['symbol']} "
                                f"qty={order['quantity']} status={order['status']}"
                            )
                            # Track position state from order fills. This is
                            # the most reliable method across all providers
                            # (mock, live, and engine). Note: this simplified
                            # boolean model assumes fixed 1-share trades. For
                            # variable quantities, track position size
                            # numerically instead.
                            if (
                                event == "ORDER_FILLED"
                                and order["symbol"] == SYMBOL
                            ):
                                if order["side"] == "BUY":
                                    state.has_position = True
                                if order["side"] == "SELL":
                                    state.has_position = False
                                state.pending_order = False

                        case {"type": "position", "event": event, "position": pos}:
                            print(
                                f"<= Position {event}: {pos['symbol']} "
                                f"qty={pos['quantity']} side={pos['side']}"
                            )

                        case {"type": "connection", "event": event}:
                            print(f"<= Connection: {event}")

                        case {"type": "error", "code": code, "message": message}:
                            print(f"<= Error {code}: {message}")

                    # Send ACK for any collected event IDs.
                    ack = build_ack(event_ids)
                    if ack:
                        await ws.send(ack)

        except (OSError, Exception) as err:
            # In production, re-query positions via GET /v1/positions on
            # reconnect to resynchronise state in case fills arrived while
            # disconnected.
            print(f"\nWebSocket closed ({err}) — reconnecting in 3s ...")
            await asyncio.sleep(3)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


async def main():
    print("=== Tektii Gateway Example Strategy ===\n")
    print(f"Symbol:         {SYMBOL}")
    print(f"Buy threshold:  ${BUY_THRESHOLD}")
    print(f"Sell threshold: ${SELL_THRESHOLD}\n")

    try:
        account = get_account()
        print(
            f"Account: {account['balance']} {account['currency']} "
            f"(equity: {account['equity']})\n"
        )
    except Exception as err:
        print(
            f"Could not reach gateway at {GATEWAY_URL} — is it running?\n"
            f"  {err}\n"
        )
        sys.exit(1)

    await connect()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\nShutting down.")
