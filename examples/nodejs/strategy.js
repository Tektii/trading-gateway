// Example strategy for the Tektii Trading Gateway.
//
// Connects via WebSocket, receives candle events from the mock provider,
// and places market orders when the price crosses simple thresholds.
// Demonstrates the ACK pattern required for backtest compatibility
// with the Tektii engine.
//
// DISCLAIMER: This is an educational example, not investment advice.
// This strategy has no risk management — no stop losses, no position
// sizing, no exposure limits. Never trade real money without proper
// risk controls.
//
// Usage:
//   # Terminal 1 — start the gateway with mock provider
//   docker run -e GATEWAY_PROVIDER=mock -p 8080:8080 ghcr.io/tektii/gateway:latest
//
//   # Terminal 2 — run this strategy
//   cd examples/nodejs && npm install && npm start

const WebSocket = require("ws");
const crypto = require("node:crypto");

// ---------------------------------------------------------------------------
// Configuration (override via environment variables)
// ---------------------------------------------------------------------------
const GATEWAY_URL = process.env.GATEWAY_URL || "http://localhost:8080";
const WS_URL = process.env.WS_URL || "ws://localhost:8080/v1/ws";
const SYMBOL = process.env.SYMBOL || "AAPL";

// Mock provider starts AAPL at ~$150 with ±0.1% random walk (~$0.15/step).
// Tight thresholds ensure trades trigger within a few seconds of starting.
const BUY_THRESHOLD = parseFloat(process.env.BUY_THRESHOLD || "150.05");
const SELL_THRESHOLD = parseFloat(process.env.SELL_THRESHOLD || "150.15");

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let hasPosition = false;

// Stays true from order submission until ORDER_FILLED is received,
// preventing duplicate orders during the ~500ms mock fill delay.
let pendingOrder = false;

// ---------------------------------------------------------------------------
// REST helpers (native fetch, Node 18+)
// ---------------------------------------------------------------------------

async function getAccount() {
  const res = await fetch(`${GATEWAY_URL}/v1/account`);
  if (!res.ok) throw new Error(`GET /v1/account failed: ${res.status}`);
  return res.json();
}

async function submitOrder(symbol, side, quantity) {
  // The REST API accepts lowercase ("buy"/"sell"/"market"/"gtc") via serde
  // aliases. WebSocket events return SCREAMING_SNAKE_CASE ("BUY"/"SELL").
  // Quantity is a string to avoid floating-point precision issues with
  // fractional quantities (e.g., "0.001" for crypto).
  const body = {
    symbol,
    side,
    quantity,
    order_type: "market",
    time_in_force: "gtc",
  };

  console.log(`=> Submitting ${side} ${quantity} ${symbol}`);

  const res = await fetch(`${GATEWAY_URL}/v1/orders`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

  if (!res.ok) {
    const text = await res.text();
    console.error(`   Order rejected (${res.status}): ${text}`);
    return null;
  }

  const order = await res.json();
  console.log(`   Order accepted: ${order.id}`);
  return order;
}

// ---------------------------------------------------------------------------
// ACK helper — required for Tektii engine backtesting, no-op for live/mock
// ---------------------------------------------------------------------------

function sendAck(ws, eventIds) {
  if (eventIds.length === 0) return;

  const ack = {
    type: "event_ack",
    correlation_id: crypto.randomUUID(),
    events_processed: eventIds,
    timestamp: Date.now(),
  };
  ws.send(JSON.stringify(ack));
}

// ---------------------------------------------------------------------------
// Candle handler — the core trading logic
// ---------------------------------------------------------------------------

async function handleCandle(ws, bar) {
  // Decimal values arrive as strings from the gateway (rust_decimal serialisation).
  const close = parseFloat(bar.close);
  const open = parseFloat(bar.open);
  const high = parseFloat(bar.high);
  const low = parseFloat(bar.low);

  console.log(
    `[${bar.symbol}] O=${open.toFixed(2)} H=${high.toFixed(2)} ` +
      `L=${low.toFixed(2)} C=${close.toFixed(2)}`
  );

  if (bar.symbol !== SYMBOL) return;
  if (pendingOrder) return;

  if (!hasPosition && close < BUY_THRESHOLD) {
    console.log(
      `   Close $${close.toFixed(2)} < $${BUY_THRESHOLD} — opening long`
    );
    pendingOrder = true;
    try {
      await submitOrder(SYMBOL, "buy", "1");
    } catch (err) {
      console.error(`   Order failed: ${err.message}`);
      pendingOrder = false;
    }
  } else if (hasPosition && close > SELL_THRESHOLD) {
    console.log(
      `   Close $${close.toFixed(2)} > $${SELL_THRESHOLD} — closing position`
    );
    pendingOrder = true;
    try {
      await submitOrder(SYMBOL, "sell", "1");
    } catch (err) {
      console.error(`   Order failed: ${err.message}`);
      pendingOrder = false;
    }
  }
}

// ---------------------------------------------------------------------------
// WebSocket connection
// ---------------------------------------------------------------------------

function connect() {
  console.log(`Connecting to ${WS_URL} ...`);
  const ws = new WebSocket(WS_URL);

  ws.on("open", () => {
    console.log("Connected to gateway WebSocket\n");
  });

  ws.on("message", async (raw) => {
    let msg;
    try {
      msg = JSON.parse(raw);
    } catch {
      return; // ignore non-JSON frames
    }

    // --- ACK pattern (engine compatibility) ---
    // The Tektii backtesting engine includes an event_id on every message.
    // Collecting and acknowledging these IDs controls time progression in
    // the engine. Against the mock/live gateway, event_id is absent and
    // this code is a harmless no-op — but it means this same strategy can
    // run against the engine with zero changes.
    const eventIds = [];
    if (msg.event_id) {
      eventIds.push(msg.event_id);
    }

    switch (msg.type) {
      case "ping":
        ws.send(JSON.stringify({ type: "pong" }));
        break;

      case "candle":
        await handleCandle(ws, msg.bar);
        break;

      case "order":
        console.log(
          `<= Order ${msg.event}: ${msg.order.symbol} ` +
            `qty=${msg.order.quantity} status=${msg.order.status}`
        );
        // Track position state from order fills. This is the most reliable
        // method across all providers (mock, live, and engine). Note: this
        // simplified boolean model assumes fixed 1-share trades. For variable
        // quantities, track position size numerically instead.
        if (
          msg.event === "ORDER_FILLED" &&
          msg.order.symbol === SYMBOL
        ) {
          if (msg.order.side === "BUY") hasPosition = true;
          if (msg.order.side === "SELL") hasPosition = false;
          pendingOrder = false;
        }
        break;

      case "position":
        console.log(
          `<= Position ${msg.event}: ${msg.position.symbol} ` +
            `qty=${msg.position.quantity} side=${msg.position.side}`
        );
        break;

      case "connection":
        console.log(`<= Connection: ${msg.event}`);
        break;

      case "error":
        console.error(`<= Error ${msg.code}: ${msg.message}`);
        break;

      default:
        break;
    }

    // Send ACK for any collected event IDs.
    sendAck(ws, eventIds);
  });

  ws.on("close", () => {
    // In production, re-query positions via GET /v1/positions on reconnect
    // to resynchronise state in case fills arrived while disconnected.
    console.log("\nWebSocket closed — reconnecting in 3s ...");
    setTimeout(connect, 3000);
  });

  ws.on("error", (err) => {
    console.error("WebSocket error:", err.message);
  });
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

async function main() {
  console.log("=== Tektii Gateway Example Strategy ===\n");
  console.log(`Symbol:         ${SYMBOL}`);
  console.log(`Buy threshold:  $${BUY_THRESHOLD}`);
  console.log(`Sell threshold: $${SELL_THRESHOLD}\n`);

  try {
    const account = await getAccount();
    console.log(
      `Account: ${account.balance} ${account.currency} ` +
        `(equity: ${account.equity})\n`
    );
  } catch (err) {
    console.error(
      `Could not reach gateway at ${GATEWAY_URL} — is it running?\n` +
        `  ${err.message}\n`
    );
    process.exit(1);
  }

  connect();
}

main();
