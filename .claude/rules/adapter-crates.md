---
globs:
  - "crates/alpaca/src/**"
  - "crates/binance/src/**"
  - "crates/oanda/src/**"
  - "crates/saxo/src/**"
  - "crates/tektii/src/**"
---

# Adapter Crate Conventions

Every adapter follows the same file structure:
- `adapter.rs` — implements `TradingAdapter` trait (the main file)
- `credentials.rs` — env-var config using `secrecy::SecretBox<String>` (NOT SecretString), with `Debug` impl that redacts secrets
- `capabilities.rs` — implements `ProviderCapabilities` trait
- `types.rs` — broker-specific API request/response types
- `websocket.rs` — implements `WebSocketProvider` trait (optional, some adapters use polling)

Constructor pattern (all adapters follow this):
1. Create `StateManager`
2. Create `ExitHandler::with_defaults(state_manager, platform)`
3. Create `EventRouter::new(state_manager, exit_handler, broadcaster, platform)`
4. Create `AdapterCircuitBreaker::new(3, Duration::from_secs(300), "provider_name")`

HTTP calls must use `execute_with_retry()` from `tektii_gateway_core::http` with an optional custom `ErrorMapper`. Use `retry_with_backoff()` for non-HTTP retry logic.

Record outages via `is_outage_error()` → `circuit_breaker.record_failure()`.

Never log raw credentials. Use `SecretBox::expose_secret()` only in auth headers.
