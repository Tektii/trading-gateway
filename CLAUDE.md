# CLAUDE.md

## What This Is

Trading Gateway is a unified trading API gateway. It normalises multiple broker APIs (Alpaca, Binance, Oanda, Saxo) into a single REST + WebSocket interface. A strategy connects to the gateway and trades through one API regardless of which broker is behind it.

**IMPORTANT — Single-provider model:** Each gateway instance serves exactly one trading provider, selected at startup by which credential env vars are set. The first provider with valid credentials wins; others are warned and ignored.

## Commands

```bash
# Build
cargo build --release
cargo build                         # debug

# Run (requires .env with broker credentials)
cargo run --release

# Test — ALWAYS use nextest, not cargo test
cargo nextest run --workspace --all-features
cargo nextest run -p tektii-gateway-core          # single crate
cargo nextest run -E 'test(test_name)'            # single test
cargo nextest run --no-capture                     # show stdout

# Lint — run before completing any task
cargo fmt --check                   # format check
cargo fmt                           # format fix
cargo clippy --workspace --all-features -- -D warnings

# Security audit
cargo deny check

# Feature-gated builds
cargo check --workspace --no-default-features --features alpaca
cargo check --workspace --all-features

# OpenAPI spec export
cargo run --bin openapi-export > openapi.json
```

CI runs: fmt, clippy, nextest, cargo-deny, and a feature matrix (each provider solo + all-features).

## Architecture

Rust workspace monorepo. Root binary (`src/main.rs`) wires everything together. Provider crates (`crates/{alpaca,binance,oanda,saxo}`) are feature-gated — default on, `tektii` is opt-in. `crates/core` holds all shared traits, models, routes, and WebSocket server. `crates/protocol` defines shared REST + WebSocket protocol types. `crates/test-support` provides `MockTradingAdapter`, `TestGateway` harness, and wiremock helpers.

### Key Abstractions

- **`TradingAdapter` trait** — the central abstraction every broker implements (account, orders, positions, market data). All handler code operates on `dyn TradingAdapter`.
- **`ProviderCapabilities` trait** — synchronous queries about what a provider supports (bracket orders, OCO, OTO). Drives `BracketStrategy` selection.
- **`WebSocketProvider` trait** — broker real-time stream with connect, reconnect with backoff, normalised `WsMessage` events.
- **`EventRouter`** — per-adapter event routing: normalises broker events, feeds exit management, broadcasts to strategy clients.
- **`ExitHandlerRegistry`** — SL/TP lifecycle: pending entries, fill detection, TTL cleanup, circuit breakers, state persistence across restarts.

### Port

All traffic on port `8080`: REST API, WebSocket (`/v1/ws`), and Prometheus metrics (`/metrics`).

## Conventions

- Rust 2024 edition, MSRV 1.91.0
- `unsafe_code = "deny"` — no unsafe, ever
- Clippy pedantic + nursery enabled (individual lints suppressed in `Cargo.toml`)
- `rustfmt` max width 100
- `rust_decimal` for all monetary values — NEVER `f64` for money
- Structured logging with `tracing` — NEVER `println!`
- `thiserror` for error types, `anyhow` only at the binary boundary
- Env-var-only configuration (no config files)
- `wiremock` for mocking broker HTTP APIs in tests
- `cargo-nextest` as test runner — NEVER `cargo test`
- Integration tests live in `tests/` at workspace root; per-adapter tests in `crates/{adapter}/tests/`
