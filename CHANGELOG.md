# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.0](https://github.com/Tektii/trading-gateway/compare/v0.2.0...v0.3.0) (2026-06-13)


### Features

* **examples:** size ma_crossover template by equity fraction ([#77](https://github.com/Tektii/trading-gateway/issues/77)) ([896beab](https://github.com/Tektii/trading-gateway/commit/896beab07c4b870d1a1b00495001d399113c49de))
* **examples:** size rsi_momentum by equity fraction; fix README env vars ([#83](https://github.com/Tektii/trading-gateway/issues/83)) ([20cd49d](https://github.com/Tektii/trading-gateway/commit/20cd49d9864934dfb3ab507cfcac238f7141a2e4))
* **exit-management:** seed synthesized-exit client_order_id from parent ([#79](https://github.com/Tektii/trading-gateway/issues/79)) ([f106bb5](https://github.com/Tektii/trading-gateway/commit/f106bb56ac69dacf44287bb3c901769d1574912e))
* **gateway:** propagate clean end-of-backtest terminal to strategies ([#67](https://github.com/Tektii/trading-gateway/issues/67)) ([69c802e](https://github.com/Tektii/trading-gateway/commit/69c802eda7f0373f1fc0c939b54802f0d78b7b4b))
* **oanda:** emit per-fill account snapshots on the live WS stream ([#80](https://github.com/Tektii/trading-gateway/issues/80)) ([134fbc0](https://github.com/Tektii/trading-gateway/commit/134fbc067db185ddd1809dfcdae3f891339302cd))
* **oanda:** forward per-fill commission and financing on the stream ([#74](https://github.com/Tektii/trading-gateway/issues/74)) ([fca88d7](https://github.com/Tektii/trading-gateway/commit/fca88d79d377a90b67e261ac055530f591ee8003))
* **tektii:** forward time-in-force to the engine and map it back ([#90](https://github.com/Tektii/trading-gateway/issues/90)) ([b7fce58](https://github.com/Tektii/trading-gateway/commit/b7fce58eb324885d0bc21a9a66243177183bdfba))


### Bug Fixes

* **core:** register attached SL/TP exits in the order-submit path ([#81](https://github.com/Tektii/trading-gateway/issues/81)) ([6e0d677](https://github.com/Tektii/trading-gateway/commit/6e0d67789f3051c162ef1b84b96cb39186e3f2c5))
* **examples:** make ma_crossover and rsi_momentum templates symbol-agnostic ([#75](https://github.com/Tektii/trading-gateway/issues/75)) ([97e5763](https://github.com/Tektii/trading-gateway/commit/97e57638980d18344cf32963e2a1f2728e4502ef))
* **examples:** make ma_crossover template trade in backtests ([#72](https://github.com/Tektii/trading-gateway/issues/72)) ([e139e27](https://github.com/Tektii/trading-gateway/commit/e139e27a1e181e86f17ed4f39a35b02257b89e59))
* **gateway:** relay engine's end-of-backtest terminal and flush-ack ([#68](https://github.com/Tektii/trading-gateway/issues/68)) ([9be83f7](https://github.com/Tektii/trading-gateway/commit/9be83f730955dfa99fc5388a5f4142abf99f1437))
* **oanda:** derive stream-fill order context from the fill reason ([#92](https://github.com/Tektii/trading-gateway/issues/92)) ([e63f798](https://github.com/Tektii/trading-gateway/commit/e63f7989f91fa969d07f2a247c60f5993d1a4f0d))
* **oanda:** deserialize orderID/tradeID wire spelling on transactions ([#87](https://github.com/Tektii/trading-gateway/issues/87)) ([eb12373](https://github.com/Tektii/trading-gateway/commit/eb12373f74fad73767e2484b4e15d4ed6ff2c6a0))
* **oanda:** emit per-fill account snapshots from the REST fill path ([#86](https://github.com/Tektii/trading-gateway/issues/86)) ([036fcdc](https://github.com/Tektii/trading-gateway/commit/036fcdc7d0ab3fbfe506e3124673094077757476))
* **oanda:** emit transaction-stream events inline and dedupe against REST fills ([#91](https://github.com/Tektii/trading-gateway/issues/91)) ([3dc4f0e](https://github.com/Tektii/trading-gateway/commit/3dc4f0ee661a2db9c0f970c25c819eae4791adcb))
* **oanda:** propagate client ids onto native TP/SL legs ([#82](https://github.com/Tektii/trading-gateway/issues/82)) ([3f8bbd1](https://github.com/Tektii/trading-gateway/commit/3f8bbd1067f5a6798ed18617c5a7ac77d1ddd834))
* **oanda:** report real quantities for partially filled IOC orders ([#88](https://github.com/Tektii/trading-gateway/issues/88)) ([e918283](https://github.com/Tektii/trading-gateway/commit/e9182832b3f6121ef42cf0c5b721535f06e7b7a9))
* **oanda:** report synchronous order cancels from the REST create response ([#85](https://github.com/Tektii/trading-gateway/issues/85)) ([b6d55c8](https://github.com/Tektii/trading-gateway/commit/b6d55c84ea9ef22b7f9f0f66ca71c9fc763a0f0a))
* **oanda:** retry the candle poll on transient HTTP failures ([#93](https://github.com/Tektii/trading-gateway/issues/93)) ([5553fbf](https://github.com/Tektii/trading-gateway/commit/5553fbf17d140014a6b30f0755f76e73ccfdf950))
* **tektii:** release engine events FIFO one-per-ack instead of drain-all ([#89](https://github.com/Tektii/trading-gateway/issues/89)) ([a854882](https://github.com/Tektii/trading-gateway/commit/a8548828a404768bfdf0ef99a6c01364a283de62))

## [0.2.0](https://github.com/Tektii/trading-gateway/compare/v0.1.0...v0.2.0) (2026-05-26)


### Features

* Add issue templates, changelog, and reorganise README ([6fc091b](https://github.com/Tektii/trading-gateway/commit/6fc091b65e13bb1e7567f095302117dcf028b440))
* **cleanup:** add comprehensive cleanup script for build artifacts and Docker resources ([58c4409](https://github.com/Tektii/trading-gateway/commit/58c44098759e68d4efb9802599eca6a4538adbe0))
* Enhance AlpacaAdapter with data feed support and improve error handling ([5635b42](https://github.com/Tektii/trading-gateway/commit/5635b42848784bd7aa076b28ae30205440b0a978))
* enhance WebSocket provider to support upstream event filtering ([c06f1c0](https://github.com/Tektii/trading-gateway/commit/c06f1c0b1bcbc694ad17e2cabc933154daf09154))
* **examples:** warm indicator state from history on live start ([767a586](https://github.com/Tektii/trading-gateway/commit/767a58688d84752d1fef6b5aa779daa4f6f96fdb))
* **examples:** warm indicator state from history on live start ([8f62e6f](https://github.com/Tektii/trading-gateway/commit/8f62e6fc9e1a77fde0e0c191595e29536380a882))
* **health:** expose build git SHA and version at runtime (TEK-580) ([#52](https://github.com/Tektii/trading-gateway/issues/52)) ([9561b44](https://github.com/Tektii/trading-gateway/commit/9561b44af3a0ee2b9a6d30bf7fab52c714cb94d2))
* **mock:** default to candle event streaming ([bec872f](https://github.com/Tektii/trading-gateway/commit/bec872f9c1d8cf06f3b62e336b8708c3e87be5e7))
* **oanda:** emit live M1 candle stream via REST poll (TEK-599) ([#53](https://github.com/Tektii/trading-gateway/issues/53)) ([7946896](https://github.com/Tektii/trading-gateway/commit/79468967d7800a89dc3b0736a4fe622a2a7495dc))
* **templates:** SDK-based Python strategy templates ([040ebcb](https://github.com/Tektii/trading-gateway/commit/040ebcb2b5a82851ba892702fde13112df88ca8b))


### Bug Fixes

* **adapter:** map HTTP 403 errors to OrderRejected for better clarity on order-level issues ([68fd254](https://github.com/Tektii/trading-gateway/commit/68fd25457150ae88a4da07dd79b3cbe23af2519c))
* **alpaca:** allow data_url to be configured independently of base_url ([f4d4f09](https://github.com/Tektii/trading-gateway/commit/f4d4f09a8825af55b96c601e8bb524f86e5d0ca4))
* **alpaca:** handle binary WebSocket frames on trading stream (TEK-573) ([#48](https://github.com/Tektii/trading-gateway/issues/48)) ([a14444f](https://github.com/Tektii/trading-gateway/commit/a14444f048795d4f04496a4e82408ed70310c056))
* **api:** allow OptionalValidatedJson with no Content-Type ([882c896](https://github.com/Tektii/trading-gateway/commit/882c896b298a1adb0cce43637dfc5c7924621f1f))
* **ci:** use simple release-type for cargo workspace ([#27](https://github.com/Tektii/trading-gateway/issues/27)) ([6774c7b](https://github.com/Tektii/trading-gateway/commit/6774c7bbe8ab45e88db93ea9c671aef4c9f07631))
* Correct provider connection task logging in main function ([6e7a822](https://github.com/Tektii/trading-gateway/commit/6e7a822417a8f60dac8dfc7ad99411c0cf1cb3c5))
* **dependencies:** update rand version to 0.9.4 for improved functionality ([6352d98](https://github.com/Tektii/trading-gateway/commit/6352d98e49e7138719cdc283679c78cf8e5ea0d5))
* **dependencies:** update tektii version constraint to &gt;=1.5.1,&lt;2 in pyproject.toml ([55d76bf](https://github.com/Tektii/trading-gateway/commit/55d76bf69179c9d9f0ac6cf4a1072f6ea2439764))
* Improve logging for subscriptions and provider connections in main function ([125f839](https://github.com/Tektii/trading-gateway/commit/125f8398945f0a58362ce1b0dc83c7647f135f19))
* **oanda:** broadcast REST market-order fills to WS clients (TEK-602) ([#54](https://github.com/Tektii/trading-gateway/issues/54)) ([0a94134](https://github.com/Tektii/trading-gateway/commit/0a94134c2e05546fdb55310699e1a7eb8d8ec5f3))
* **oanda:** cancel per-task tokens in disconnect (TEK-658) ([#65](https://github.com/Tektii/trading-gateway/issues/65)) ([009e451](https://github.com/Tektii/trading-gateway/commit/009e451bfe108567fce8ac72aac706b8d5c3cba4))
* **oanda:** cancel-and-replace transaction stream task on reconnect (TEK-601) ([#63](https://github.com/Tektii/trading-gateway/issues/63)) ([651bfbf](https://github.com/Tektii/trading-gateway/commit/651bfbf4b0281db341e3422a8b76539c7cc55a26))
* **protocol:** default Trade.liquidation when omitted by engine (TEK-286) ([#41](https://github.com/Tektii/trading-gateway/issues/41)) ([16472dc](https://github.com/Tektii/trading-gateway/commit/16472dc1d7187aa65bc369332f966aecb513a722))
* Reorder imports for consistency in main.rs ([26b2bda](https://github.com/Tektii/trading-gateway/commit/26b2bda5ed4622b691ebf6edfe8f3c0a6911d07b))
* Resolve breaking changes from reqwest 0.13 and hmac 0.13 upgrades ([3e2e019](https://github.com/Tektii/trading-gateway/commit/3e2e019401bec8f95118c4e73f24addb092b5ba1))
* **symbols:** gateway is symbol pass-through, align templates to engine-native form (TEK-269) ([#49](https://github.com/Tektii/trading-gateway/issues/49)) ([fadd605](https://github.com/Tektii/trading-gateway/commit/fadd6055caa55d211d3bf99d0f6907c3f00f2f2d))
* **tektii:** propagate engine average_fill_price to clients (TEK-603) ([c968f9d](https://github.com/Tektii/trading-gateway/commit/c968f9dc857e558dbe6fb5cc5d16539b1774909c))
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
