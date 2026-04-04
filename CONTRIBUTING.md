# Contributing to Trading Gateway

Thanks for considering a contribution! Whether it's a bug report, feature suggestion, or pull request — all help is welcome.

Please open an [issue](https://github.com/tektii/trading-gateway/issues) for bugs or feature requests before starting large changes.

## Prerequisites

- **Rust 1.91.0** — handled automatically by [`rust-toolchain.toml`](rust-toolchain.toml)
- **cargo-nextest** — `cargo install cargo-nextest` (used for running tests)
- **cargo-deny** — `cargo install cargo-deny` (used for license and advisory audits)
- **Docker** — optional, for running the gateway locally

## Getting Started

```bash
git clone https://github.com/tektii/trading-gateway.git
cd tektii-gateway
cargo check --workspace
```

If this compiles, your environment is ready.

## Development Workflow

1. Fork the repository and create a branch from `main`
2. Make your changes
3. Run all checks before opening a PR:

   ```bash
   cargo fmt --check
   cargo clippy --workspace --all-features -- -D warnings
   cargo nextest run --workspace --all-features
   cargo deny check
   ```

4. Open a pull request against `main`

## Code Standards

- **Formatting:** `rustfmt` with `max_width = 100` (see [`rustfmt.toml`](rustfmt.toml))
- **Lints:** clippy with `pedantic` + `nursery` groups enabled, warnings treated as errors in CI
- **No unsafe code:** `unsafe_code = "deny"` is set workspace-wide
- **Error handling:** Use `Result<T, E>` with [`thiserror`](https://docs.rs/thiserror) for custom error types — never `unwrap()` or `expect()` in production code
- **Logging:** Use [`tracing`](https://docs.rs/tracing) macros (`tracing::info!`, `tracing::error!`, etc.) — not `println!`

## API Spec

The OpenAPI spec (`openapi.json`) is source-controlled. If you change API routes, update it before opening your PR:

```bash
cargo run --bin openapi-export > openapi.json
```

CI will fail if the committed spec is stale.

## Feature Flags

The gateway uses Cargo feature flags for broker adapters:

| Feature | Default | Description |
|---------|---------|-------------|
| `alpaca` | Yes | Alpaca adapter |
| `binance` | Yes | Binance adapter |
| `oanda` | Yes | Oanda adapter |
| `saxo` | Yes | Saxo adapter |
| `tektii` | No | Tektii backtesting adapter |

To check compilation with a specific adapter:

```bash
cargo check --no-default-features --features alpaca
```

To check with all adapters:

```bash
cargo check --all-features
```

## CI

All of the following must pass before a PR can merge:

| Job | Command |
|-----|---------|
| Format | `cargo fmt --check` |
| Clippy | `cargo clippy --workspace --all-features -- -D warnings` |
| Test | `cargo nextest run --workspace --all-features` |
| Deny | `cargo deny check` |
| Features | `cargo check` with each feature flag individually and `--all-features` |

## License

By submitting a pull request, you agree that your contribution is licensed under the [Elastic License 2.0](LICENSE).
