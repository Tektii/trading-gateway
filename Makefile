.PHONY: check fmt fmt-fix clippy test deny features openapi

check: fmt clippy test deny ## Full quality gate (matches CI)

fmt: ## Format check
	cargo fmt --check

fmt-fix: ## Format fix
	cargo fmt

clippy: ## Lint
	cargo clippy --workspace --all-features -- -D warnings

test: ## Run all tests
	cargo nextest run --workspace --all-features

deny: ## Security audit
	cargo deny check

features: ## Feature matrix check (each provider solo + all)
	cargo check --workspace --no-default-features
	cargo check --workspace --no-default-features --features alpaca
	cargo check --workspace --no-default-features --features binance
	cargo check --workspace --no-default-features --features oanda
	cargo check --workspace --no-default-features --features saxo
	cargo check --workspace --no-default-features --features tektii
	cargo check --workspace --all-features

openapi: ## Export OpenAPI spec
	cargo run --bin openapi-export > openapi.json
