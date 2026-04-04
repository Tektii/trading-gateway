---
globs:
  - "tests/**"
  - "**/tests/**"
  - "crates/test-support/**"
---

# Testing Conventions

Use `tektii_gateway_test_support` for all mocks and fixtures — never create ad-hoc mocks.

Available infrastructure:
- `MockTradingAdapter` — builder pattern: `.with_order()`, `.with_position()`, etc. Assertions: `.submitted_orders()`, `.cancelled_orders()`
- `TestGateway` + `StrategyClient` — full e2e harness. `spawn_test_gateway(adapter)`, inject via `gw.inject_event(WsMessage::...)`, connect via `StrategyClient::connect(&gw)`
- `MockWebSocketProvider`, `MockExitHandler`, `MockPriceSource`, `MockStopOrderExecutor`
- Model factories: `test_order()`, `test_position()`, `test_account()`, `test_quote(symbol)`, `test_bar(symbol)`, `test_order_request()`, `test_trade()`

Integration tests at workspace root (`tests/`) use `TestGateway`. Per-adapter tests (`crates/{adapter}/tests/`) use `wiremock` via `start_mock_server()` + `mount_json()`.
