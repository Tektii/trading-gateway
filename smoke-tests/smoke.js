// smoke.js — k6 smoke tests for the Trading Gateway.
//
// Validates that a running gateway instance correctly handles REST API requests
// against a real broker sandbox. Adapter-agnostic: the same tests run against
// any provider (Alpaca Paper, Binance Testnet, Oanda Practice, etc).
//
// Usage:
//   k6 run smoke-tests/smoke.js \
//     --env GATEWAY_URL=http://localhost:8080 \
//     --env SMOKE_TEST_SYMBOL=AAPL
//
// Optional:
//   --env SMOKE_TEST_ORDER=1        Enable order submit+cancel lifecycle test
//   --env SMOKE_ORDER_QTY=1         Quantity for test order
//   --env SMOKE_ORDER_PRICE=1.00    Limit price far from market

import http from "k6/http";
import { check, group } from "k6";

const BASE = __ENV.GATEWAY_URL || "http://localhost:8080";
const SYMBOL = __ENV.SMOKE_TEST_SYMBOL || "AAPL";
const ORDER_TEST = __ENV.SMOKE_TEST_ORDER === "1";
const ORDER_QTY = __ENV.SMOKE_ORDER_QTY || "1";
const ORDER_PRICE = __ENV.SMOKE_ORDER_PRICE || "1.00";

const REQ_OPTS = { timeout: "10s" };
const JSON_OPTS = {
  timeout: "10s",
  headers: { "Content-Type": "application/json" },
};

// Single iteration, not a load test.
export const options = {
  iterations: 1,
  thresholds: {
    checks: ["rate==1.0"],
    http_req_duration: ["p(95)<5000"],
  },
};

// Safe JSON parser — returns null instead of throwing on non-JSON responses.
function jsonBody(r) {
  try {
    return r.json();
  } catch (_) {
    return null;
  }
}

export default function () {
  // -------------------------------------------------------------------
  // Health
  // -------------------------------------------------------------------
  group("Health", () => {
    const livez = http.get(`${BASE}/livez`, REQ_OPTS);
    const livezBody = jsonBody(livez);
    check(livez, {
      "GET /livez returns 200": (r) => r.status === 200,
      "GET /livez is JSON": () => livezBody !== null,
      "GET /livez status is ok": () => livezBody && livezBody.status === "ok",
    });

    const readyz = http.get(`${BASE}/readyz`, REQ_OPTS);
    const readyzBody = jsonBody(readyz);
    check(readyz, {
      "GET /readyz returns 200": (r) => r.status === 200,
      "GET /readyz is JSON": () => readyzBody !== null,
      "GET /readyz ready is true": () => readyzBody && readyzBody.ready === true,
    });

    const health = http.get(`${BASE}/health`, REQ_OPTS);
    const healthBody = jsonBody(health);
    check(health, {
      "GET /health returns 200": (r) => r.status === 200,
      "GET /health is JSON": () => healthBody !== null,
      "GET /health has valid status": () =>
        healthBody && ["connected", "degraded"].includes(healthBody.status),
    });
  });

  // -------------------------------------------------------------------
  // Account
  // -------------------------------------------------------------------
  group("Account", () => {
    const account = http.get(`${BASE}/v1/account`, REQ_OPTS);
    const body = jsonBody(account);
    check(account, {
      "GET /v1/account returns 200": (r) => r.status === 200,
      "GET /v1/account is JSON": () => body !== null,
      "account has balance": () => body && body.balance !== undefined,
      "account has equity": () => body && body.equity !== undefined,
      "account has currency": () => body && body.currency !== undefined,
    });
  });

  // -------------------------------------------------------------------
  // Market Data
  // -------------------------------------------------------------------
  group("Market Data", () => {
    const quote = http.get(`${BASE}/v1/quotes/${SYMBOL}`, REQ_OPTS);
    const quoteBody = jsonBody(quote);
    check(quote, {
      [`GET /v1/quotes/${SYMBOL} returns 200`]: (r) => r.status === 200,
      "quote is JSON": () => quoteBody !== null,
      "quote has symbol": () => quoteBody && quoteBody.symbol !== undefined,
      "quote has bid": () => quoteBody && quoteBody.bid !== undefined,
      "quote has ask": () => quoteBody && quoteBody.ask !== undefined,
    });

    const bars = http.get(
      `${BASE}/v1/bars/${SYMBOL}?timeframe=1h&start=2024-01-01T00:00:00Z&end=2024-01-02T00:00:00Z`,
      REQ_OPTS,
    );
    const barsBody = jsonBody(bars);
    check(bars, {
      [`GET /v1/bars/${SYMBOL} returns 200`]: (r) => r.status === 200,
      "bars is JSON": () => barsBody !== null,
      "bars is an array": () => barsBody && Array.isArray(barsBody),
    });
  });

  // -------------------------------------------------------------------
  // System
  // -------------------------------------------------------------------
  group("System", () => {
    const caps = http.get(`${BASE}/v1/capabilities`, REQ_OPTS);
    const capsBody = jsonBody(caps);
    check(caps, {
      "GET /v1/capabilities returns 200": (r) => r.status === 200,
      "capabilities is JSON": () => capsBody !== null,
      "capabilities has supported_order_types": () =>
        capsBody && Array.isArray(capsBody.supported_order_types),
      "capabilities has position_mode": () =>
        capsBody && capsBody.position_mode !== undefined,
    });

    const status = http.get(`${BASE}/v1/status`, REQ_OPTS);
    const statusBody = jsonBody(status);
    check(status, {
      "GET /v1/status returns 200": (r) => r.status === 200,
      "status is JSON": () => statusBody !== null,
      "status has connected field": () =>
        statusBody && statusBody.connected !== undefined,
    });
  });

  // -------------------------------------------------------------------
  // Orders (read-only)
  // -------------------------------------------------------------------
  group("Orders", () => {
    const orders = http.get(`${BASE}/v1/orders?symbol=${SYMBOL}`, REQ_OPTS);
    const ordersBody = jsonBody(orders);
    check(orders, {
      "GET /v1/orders returns 200": (r) => r.status === 200,
      "orders is an array": () => ordersBody && Array.isArray(ordersBody),
    });

    const history = http.get(`${BASE}/v1/orders/history?symbol=${SYMBOL}`, REQ_OPTS);
    const historyBody = jsonBody(history);
    check(history, {
      "GET /v1/orders/history returns 200": (r) => r.status === 200,
      "order history is an array": () =>
        historyBody && Array.isArray(historyBody),
    });
  });

  // -------------------------------------------------------------------
  // Positions (read-only)
  // -------------------------------------------------------------------
  group("Positions", () => {
    const positions = http.get(`${BASE}/v1/positions`, REQ_OPTS);
    const posBody = jsonBody(positions);
    check(positions, {
      "GET /v1/positions returns 200": (r) => r.status === 200,
      "positions is an array": () => posBody && Array.isArray(posBody),
    });
  });

  // -------------------------------------------------------------------
  // Trades (read-only)
  // -------------------------------------------------------------------
  group("Trades", () => {
    const trades = http.get(`${BASE}/v1/trades?symbol=${SYMBOL}`, REQ_OPTS);
    const tradesBody = jsonBody(trades);
    check(trades, {
      "GET /v1/trades returns 200": (r) => r.status === 200,
      "trades is an array": () => tradesBody && Array.isArray(tradesBody),
    });
  });

  // -------------------------------------------------------------------
  // Order Lifecycle (submit + get + cancel)
  // -------------------------------------------------------------------
  if (ORDER_TEST) {
    group("Order Lifecycle", () => {
      const payload = JSON.stringify({
        symbol: SYMBOL,
        side: "buy",
        quantity: ORDER_QTY,
        order_type: "limit",
        limit_price: ORDER_PRICE,
        time_in_force: "gtc",
      });

      const submit = http.post(`${BASE}/v1/orders`, payload, JSON_OPTS);
      const submitBody = jsonBody(submit);

      const submitted = check(submit, {
        "POST /v1/orders returns 201": (r) => r.status === 201,
        "order has id": () => submitBody && submitBody.id !== undefined,
      });

      if (submitted) {
        const orderId = submitBody.id;

        // Fetch the order by ID before cancelling
        const fetched = http.get(`${BASE}/v1/orders/${orderId}`, REQ_OPTS);
        const fetchedBody = jsonBody(fetched);
        check(fetched, {
          [`GET /v1/orders/${orderId} returns 200`]: (r) => r.status === 200,
          "fetched order has matching id": () =>
            fetchedBody && fetchedBody.id === orderId,
        });

        const cancel = http.del(`${BASE}/v1/orders/${orderId}`, null, REQ_OPTS);
        check(cancel, {
          [`DELETE /v1/orders/${orderId} returns 200`]: (r) => r.status === 200,
        });
      }
    });
  }
}

// Export structured JSON summary alongside the default text output.
export function handleSummary(data) {
  return {
    stdout: textSummary(data),
    "smoke-results.json": JSON.stringify(data, null, 2),
  };
}

// Minimal text summary (k6 built-in is not importable in all versions).
function textSummary(data) {
  const lines = ["\n=== Smoke Test Results ===\n"];

  const checks = data.root_group.checks || [];
  const groups = data.root_group.groups || [];

  function printChecks(checkList, indent) {
    for (const c of checkList) {
      const passed = c.passes > 0 && c.fails === 0;
      const icon = passed ? "+" : "x";
      lines.push(`${indent} ${icon} ${c.name}`);
    }
  }

  function printGroup(g, indent) {
    lines.push(`${indent}${g.name}`);
    printChecks(g.checks || [], indent);
    for (const sub of g.groups || []) {
      printGroup(sub, indent + "  ");
    }
  }

  printChecks(checks, "");
  for (const g of groups) {
    printGroup(g, "");
  }

  const metrics = data.metrics || {};
  const checkRate = metrics.checks ? metrics.checks.values.rate : 0;
  const totalChecks = metrics.checks
    ? metrics.checks.values.passes + metrics.checks.values.fails
    : 0;
  const passed = metrics.checks ? metrics.checks.values.passes : 0;
  const failed = metrics.checks ? metrics.checks.values.fails : 0;

  lines.push("");
  lines.push(`Results: ${passed}/${totalChecks} passed, ${failed} failed`);

  const p95 = metrics.http_req_duration
    ? metrics.http_req_duration.values["p(95)"]
    : null;
  if (p95 !== null) {
    lines.push(`HTTP p95 latency: ${Math.round(p95)}ms`);
  }

  lines.push("");
  return lines.join("\n");
}
