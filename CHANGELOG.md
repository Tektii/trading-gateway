# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.2.0](https://github.com/Tektii/trading-gateway/compare/v0.1.0...v0.2.0) (2026-04-29)


### Features

* Add issue templates, changelog, and reorganise README ([6fc091b](https://github.com/Tektii/trading-gateway/commit/6fc091b65e13bb1e7567f095302117dcf028b440))
* Enhance AlpacaAdapter with data feed support and improve error handling ([5635b42](https://github.com/Tektii/trading-gateway/commit/5635b42848784bd7aa076b28ae30205440b0a978))
* enhance WebSocket provider to support upstream event filtering ([c06f1c0](https://github.com/Tektii/trading-gateway/commit/c06f1c0b1bcbc694ad17e2cabc933154daf09154))
* **examples:** warm indicator state from history on live start ([767a586](https://github.com/Tektii/trading-gateway/commit/767a58688d84752d1fef6b5aa779daa4f6f96fdb))
* **examples:** warm indicator state from history on live start ([8f62e6f](https://github.com/Tektii/trading-gateway/commit/8f62e6fc9e1a77fde0e0c191595e29536380a882))
* **mock:** default to candle event streaming ([bec872f](https://github.com/Tektii/trading-gateway/commit/bec872f9c1d8cf06f3b62e336b8708c3e87be5e7))
* **templates:** SDK-based Python strategy templates ([040ebcb](https://github.com/Tektii/trading-gateway/commit/040ebcb2b5a82851ba892702fde13112df88ca8b))


### Bug Fixes

* **adapter:** map HTTP 403 errors to OrderRejected for better clarity on order-level issues ([68fd254](https://github.com/Tektii/trading-gateway/commit/68fd25457150ae88a4da07dd79b3cbe23af2519c))
* **alpaca:** allow data_url to be configured independently of base_url ([f4d4f09](https://github.com/Tektii/trading-gateway/commit/f4d4f09a8825af55b96c601e8bb524f86e5d0ca4))
* **api:** allow OptionalValidatedJson with no Content-Type ([882c896](https://github.com/Tektii/trading-gateway/commit/882c896b298a1adb0cce43637dfc5c7924621f1f))
* **ci:** use simple release-type for cargo workspace ([#27](https://github.com/Tektii/trading-gateway/issues/27)) ([6774c7b](https://github.com/Tektii/trading-gateway/commit/6774c7bbe8ab45e88db93ea9c671aef4c9f07631))
* Correct provider connection task logging in main function ([6e7a822](https://github.com/Tektii/trading-gateway/commit/6e7a822417a8f60dac8dfc7ad99411c0cf1cb3c5))
* **dependencies:** update rand version to 0.9.4 for improved functionality ([6352d98](https://github.com/Tektii/trading-gateway/commit/6352d98e49e7138719cdc283679c78cf8e5ea0d5))
* Improve logging for subscriptions and provider connections in main function ([125f839](https://github.com/Tektii/trading-gateway/commit/125f8398945f0a58362ce1b0dc83c7647f135f19))
* **protocol:** default Trade.liquidation when omitted by engine (TEK-286) ([#41](https://github.com/Tektii/trading-gateway/issues/41)) ([16472dc](https://github.com/Tektii/trading-gateway/commit/16472dc1d7187aa65bc369332f966aecb513a722))
* Reorder imports for consistency in main.rs ([26b2bda](https://github.com/Tektii/trading-gateway/commit/26b2bda5ed4622b691ebf6edfe8f3c0a6911d07b))
* Resolve breaking changes from reqwest 0.13 and hmac 0.13 upgrades ([3e2e019](https://github.com/Tektii/trading-gateway/commit/3e2e019401bec8f95118c4e73f24addb092b5ba1))
* **tektii:** tie pending-ID drain to actual send_to success ([#39](https://github.com/Tektii/trading-gateway/issues/39)) ([660e561](https://github.com/Tektii/trading-gateway/commit/660e5614db7ffcd31056efe31e3abea1b33e8d56))
* Update account information description to include buying power ([39ae4a7](https://github.com/Tektii/trading-gateway/commit/39ae4a7e18df2b7656ba61a4832a0f4f227cda91))
* Update dependencies to use 'tektii' instead of 'tektii-gateway' in examples ([1f3b81a](https://github.com/Tektii/trading-gateway/commit/1f3b81a8eb42ad53feef44d9c83a14311f2b8513))
* Update deploy-pages action version in GitHub Actions workflow ([db8f97a](https://github.com/Tektii/trading-gateway/commit/db8f97a38a86f37782d39df87d3234b2959cee72))

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
