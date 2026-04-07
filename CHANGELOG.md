# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Unified REST + WebSocket trading API with support for multiple brokers
- Broker-agnostic exit management (stop-loss, take-profit, trailing stops) with state persistence
- Automatic reconnection with exponential backoff
- Prometheus metrics at `/metrics`
- API key authentication with network isolation guidance
- Docker images (Debian and distroless) for amd64 and arm64
- OpenAPI specification with hosted API docs
- Python and Node.js example strategies
- Mock provider for zero-credential testing
