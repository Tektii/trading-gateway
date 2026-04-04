//! Tektii Gateway — unified trading API gateway.
//!
//! Startup sequence:
//!  1. Load config & init telemetry
//!  2. Bind port early (OS queues connections during setup)
//!  3. Create shared infrastructure
//!  4. Register adapters (feature-gated, credential-presence-driven)
//!  5. Connect provider WebSocket streams (from subscriptions)
//!  6. Spawn TTL cleanup tasks
//!  7. Build router and serve with graceful shutdown

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use utoipa_swagger_ui::SwaggerUi;

use tektii_gateway_core::adapter::{AdapterRegistry, TradingAdapter};
use tektii_gateway_core::api::middleware::auth_middleware;
use tektii_gateway_core::api::routes::create_gateway_router;
use tektii_gateway_core::api::state::GatewayState;
use tektii_gateway_core::config::{GatewayConfig, load_required_env_vars};
use tektii_gateway_core::events::router::EventRouter;
use tektii_gateway_core::exit_management::ExitHandlerRegistry;
use tektii_gateway_core::metrics::{MetricsHandle, metrics_handler};
use tektii_gateway_core::models::{
    GatewayMode, TradingPlatform, TradingPlatformKind, VALID_PROVIDERS,
};
use tektii_gateway_core::subscription::filter::SubscriptionFilter;
use tektii_gateway_core::telemetry::init_tracing;
use tektii_gateway_core::websocket::connection::WsConnectionManager;
use tektii_gateway_core::websocket::messages::WsMessage;
use tektii_gateway_core::websocket::provider::WebSocketProvider;
use tektii_gateway_core::websocket::registry::ProviderRegistry;
use tektii_gateway_core::websocket::server::ws_handler;

/// Collected adapter info for deferred EventRouter registration.
#[derive(Clone)]
struct AdapterInfo {
    platform: TradingPlatform,
    router: Arc<EventRouter>,
    adapter: Arc<dyn TradingAdapter>,
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    // 1. Load config
    let config = GatewayConfig::from_env()?;

    // 2. Init telemetry
    init_tracing("tektii-gateway", env!("CARGO_PKG_VERSION"));

    // 3. Bind port early
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("Listening on {}", addr);

    // 4. Init metrics
    let metrics_handle = Arc::new(MetricsHandle::try_init().ok_or_else(|| {
        anyhow::anyhow!("Failed to initialise metrics — is another recorder already installed?")
    })?);

    // 5. Create shared infrastructure
    let cancel_token = CancellationToken::new();
    let ws_connection_manager = Arc::new(WsConnectionManager::new());
    let subscription_filter = SubscriptionFilter::new(&config.subscriptions);
    let correlation_store =
        std::sync::Arc::new(tektii_gateway_core::correlation::CorrelationStore::new());
    let provider_registry = Arc::new(ProviderRegistry::new(
        ws_connection_manager.clone(),
        subscription_filter,
        cancel_token.clone(),
        config.reconnection.clone(),
        correlation_store.clone(),
    ));
    let exit_handler_registry = Arc::new(ExitHandlerRegistry::new());

    // 6. Read provider config
    let provider = read_provider()?;
    let mode = read_mode()?;
    let platform = provider.to_platform(mode);
    check_feature_enabled(provider)?;
    let credentials = load_required_env_vars(provider)?;

    if provider == TradingPlatformKind::Mock {
        info!(
            "Gateway starting in MOCK mode — no broker credentials required. All data is synthetic."
        );
        info!(
            "  REST API:     http://{}:{}/v1/account",
            config.host, config.port
        );
        info!("  WebSocket:    ws://{}:{}/v1/ws", config.host, config.port);
        info!("  Account:      $100,000 USD (simulated)");
        info!("  Symbols:      AAPL (~$150), BTCUSD (~$50k), EURUSD (~$1.08), or any string");
        info!("  Order fills:  ~500ms delay, market price fill");
        info!("  Quotes:       every 2s on subscribed symbols via WebSocket");
    } else {
        match mode {
            GatewayMode::Live => {
                warn!(
                    "Gateway starting in LIVE trading mode. Orders will execute with real funds."
                );
            }
            GatewayMode::Paper => {
                info!(
                    "Gateway starting in PAPER trading mode (default). Set GATEWAY_MODE=live for real trading."
                );
            }
        }
    }

    // 7. Register adapter
    let (adapter, event_routers_to_register) =
        register_adapter(provider, platform, &credentials, &exit_handler_registry).await?;
    let adapter_registry = AdapterRegistry::new(adapter, platform);
    info!(?platform, "Gateway configured for single provider");

    // 7-8. Wire EventRouters and trailing stop handlers
    wire_event_routers_and_trailing_stops(&provider_registry, &event_routers_to_register).await?;

    // 9. Spawn position synthesis task
    provider_registry.spawn_position_synthesis_task();

    // 10. Connect provider WebSocket streams in background (non-blocking)
    //     This allows the gateway to accept strategy connections before the
    //     upstream provider (e.g., engine) is available. Critical for backtest
    //     mode where startup order is: gateway → strategy → engine.
    {
        let config = config.clone();
        let provider_registry = provider_registry.clone();
        let event_routers = event_routers_to_register.clone();
        #[cfg(feature = "mock")]
        let mock_provider = provider == TradingPlatformKind::Mock;
        #[cfg(not(feature = "mock"))]
        let mock_provider = false;
        tokio::spawn(async move {
            // Mock mode: auto-connect with default symbols if no subscriptions set
            #[cfg(feature = "mock")]
            if mock_provider && config.platforms_used().is_empty() {
                let default_symbols = vec![
                    "AAPL".to_string(),
                    "MSFT".to_string(),
                    "BTCUSD".to_string(),
                    "EURUSD".to_string(),
                ];
                let default_events = vec!["quote".to_string()];
                info!(
                    symbols = ?default_symbols,
                    "Mock mode: auto-subscribing to default symbols (set SUBSCRIPTIONS to override)"
                );
                let result = connect_mock(
                    platform,
                    default_symbols,
                    default_events,
                    &provider_registry,
                    &event_routers,
                )
                .await;
                match result {
                    Ok(()) => info!("Mock provider WebSocket connected with default symbols"),
                    Err(e) => error!("Failed to connect mock provider: {e}"),
                }
            } else {
                connect_providers(&config, &provider_registry, &event_routers).await;
            }
            let _ = mock_provider; // suppress unused warning
        });
    }

    // 11. Spawn TTL cleanup tasks
    let ttl_duration = Duration::from_secs(config.sl_tp_ttl_hours * 3600);
    let _cleanup_handles = exit_handler_registry.spawn_cleanup_tasks(&cancel_token, ttl_duration);

    // 12. Check for previous exit state snapshot
    tektii_gateway_core::shutdown::read_exit_state_on_startup(&config.exit_state_file).await;

    // 13. Build router and serve
    let gateway_state = GatewayState::new(
        adapter_registry,
        ws_connection_manager.clone(),
        provider_registry.clone(),
        exit_handler_registry.clone(),
        correlation_store,
        config.api_key.clone(),
    );

    if gateway_state.api_key().is_some() {
        info!("API key authentication enabled");
    } else {
        warn!("╔══════════════════════════════════════════════════════════════╗");
        warn!("║  AUTHENTICATION DISABLED — all endpoints are open.         ║");
        warn!("║  Set GATEWAY_API_KEY to require Bearer token / X-API-Key.  ║");
        warn!("╚══════════════════════════════════════════════════════════════╝");
    }

    if config.host == "0.0.0.0" && gateway_state.api_key().is_none() {
        warn!(
            "Gateway is binding to 0.0.0.0 (all interfaces) with no API key — \
             all endpoints are unauthenticated and network-accessible"
        );
    }

    let shutdown_state = gateway_state.clone();

    let (router, api) = create_gateway_router().split_for_parts();
    let metrics_state = metrics_handle;
    let mut app = router
        .route(
            "/v1/ws",
            get(
                |ws: axum::extract::WebSocketUpgrade,
                 state: axum::extract::State<GatewayState>,
                 conn: axum::extract::ConnectInfo<SocketAddr>| async move {
                    ws_handler(ws, state, conn)
                },
            ),
        )
        .with_state(gateway_state.clone());

    if config.enable_swagger {
        info!("Swagger UI enabled at /swagger-ui");
        app = app.merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api));
    } else {
        info!("Swagger UI disabled (set ENABLE_SWAGGER=true to enable)");
    }

    // Auth middleware applied AFTER all routes are assembled so that merged
    // routes (e.g. Swagger UI) are also covered. Public paths (/health,
    // /metrics, /livez, /readyz) are exempted inside the middleware itself.
    let app = app
        .route_service("/metrics", get(metrics_handler).with_state(metrics_state))
        .layer(axum::middleware::from_fn_with_state(
            gateway_state,
            auth_middleware,
        ));

    info!("Gateway ready — serving on {}", addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // 13. Graceful shutdown sequence
    // (Axum graceful shutdown already drained in-flight requests above)
    tektii_gateway_core::shutdown::run_shutdown_sequence(
        &ws_connection_manager,
        &shutdown_state,
        &exit_handler_registry,
        &config.exit_state_file,
        &provider_registry,
        &cancel_token,
    )
    .await;

    Ok(())
}

/// Wire EventRouters into the ProviderRegistry and set up trailing stop handlers.
async fn wire_event_routers_and_trailing_stops(
    provider_registry: &Arc<ProviderRegistry>,
    event_routers: &[AdapterInfo],
) -> anyhow::Result<()> {
    let ws_price_source = Arc::new(tektii_gateway_core::trailing_stop::WebSocketPriceSource::new());
    provider_registry
        .set_price_source(ws_price_source.clone())
        .await;

    for info in event_routers {
        provider_registry
            .register_event_router(info.platform, info.router.clone(), info.adapter.clone())
            .await?;

        let stop_executor = Arc::new(
            tektii_gateway_core::trailing_stop::AdapterStopOrderExecutor::new(
                info.adapter.clone(),
                info.platform,
            ),
        );
        let trailing_handler =
            tektii_gateway_core::trailing_stop::TrailingStopHandler::new_arc_with_quote_subscriber(
                ws_price_source.clone(),
                stop_executor,
                info.platform,
                tektii_gateway_core::trailing_stop::TrailingStopConfig::default(),
                provider_registry.clone(),
            );
        if info
            .router
            .set_trailing_stop_handler(trailing_handler.clone())
            .is_err()
        {
            warn!(platform = ?info.platform, "Trailing stop handler already set on EventRouter");
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => info!("Received SIGINT"),
            _ = terminate.recv() => info!("Received SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("failed to listen for ctrl-c");
        info!("Received SIGINT");
    }
}

/// Read `GATEWAY_PROVIDER` from the environment.
fn read_provider() -> anyhow::Result<TradingPlatformKind> {
    let raw = env::var("GATEWAY_PROVIDER").map_err(|_| {
        anyhow::anyhow!("GATEWAY_PROVIDER is required. Set it to one of: {VALID_PROVIDERS}")
    })?;
    raw.parse::<TradingPlatformKind>().map_err(|_| {
        anyhow::anyhow!("invalid GATEWAY_PROVIDER='{raw}': valid values are {VALID_PROVIDERS}")
    })
}

/// Read `GATEWAY_MODE` from the environment (default: `paper`).
fn read_mode() -> anyhow::Result<GatewayMode> {
    match env::var("GATEWAY_MODE") {
        Ok(raw) => raw
            .parse::<GatewayMode>()
            .map_err(|e| anyhow::anyhow!("{e}")),
        Err(_) => Ok(GatewayMode::default()),
    }
}

/// Verify the selected provider's cargo feature is enabled in this build.
fn check_feature_enabled(provider: TradingPlatformKind) -> anyhow::Result<()> {
    let (enabled, feature) = match provider {
        TradingPlatformKind::Alpaca => (cfg!(feature = "alpaca"), "alpaca"),
        TradingPlatformKind::BinanceSpot
        | TradingPlatformKind::BinanceFutures
        | TradingPlatformKind::BinanceMargin
        | TradingPlatformKind::BinanceCoinFutures => (cfg!(feature = "binance"), "binance"),
        TradingPlatformKind::Oanda => (cfg!(feature = "oanda"), "oanda"),
        TradingPlatformKind::Saxo => (cfg!(feature = "saxo"), "saxo"),
        TradingPlatformKind::Tektii => (cfg!(feature = "tektii"), "tektii"),
        TradingPlatformKind::Mock => (cfg!(feature = "mock"), "mock"),
    };
    if !enabled {
        anyhow::bail!(
            "GATEWAY_PROVIDER='{provider}' requires the '{feature}' cargo feature, \
             which is not enabled in this build"
        );
    }
    Ok(())
}

/// Register the single adapter based on explicit provider selection.
#[allow(unused_variables, clippy::too_many_lines)]
async fn register_adapter(
    provider: TradingPlatformKind,
    platform: TradingPlatform,
    credentials: &[String],
    exit_handler_registry: &Arc<ExitHandlerRegistry>,
) -> anyhow::Result<(Arc<dyn TradingAdapter>, Vec<AdapterInfo>)> {
    match provider {
        // =====================================================================
        // Alpaca
        // =====================================================================
        #[cfg(feature = "alpaca")]
        TradingPlatformKind::Alpaca => {
            use tektii_gateway_alpaca::{AlpacaAdapter, AlpacaCredentials};

            let mut creds = AlpacaCredentials::new(&credentials[0], &credentials[1]);
            if let Ok(url) = env::var("ALPACA_BASE_URL") {
                creds = creds.with_base_url(url);
            }
            if let Ok(feed) = env::var("ALPACA_FEED") {
                creds = creds.with_feed(feed);
            }

            let (tx, _) = broadcast::channel::<WsMessage>(256);
            let adapter = Arc::new(AlpacaAdapter::new(&creds, tx, platform)?);

            exit_handler_registry.register(platform, adapter.exit_handler());
            let info = AdapterInfo {
                platform,
                router: adapter.event_router(),
                adapter: adapter.clone(),
            };
            info!(?platform, "Alpaca adapter registered");
            Ok((adapter, vec![info]))
        }

        // =====================================================================
        // Binance
        // =====================================================================
        #[cfg(feature = "binance")]
        TradingPlatformKind::BinanceSpot => {
            use tektii_gateway_binance::{BinanceCredentials, BinanceSpotAdapter};

            let mut creds = BinanceCredentials::new(&credentials[0], &credentials[1]);
            if let Ok(url) = env::var("BINANCE_BASE_URL") {
                creds = creds.with_base_url(url);
            }
            if let Ok(ws_url) = env::var("BINANCE_STREAM_URL") {
                creds = creds.with_ws_base_url(ws_url);
            }

            let (tx, _) = broadcast::channel::<WsMessage>(256);
            let adapter = Arc::new(BinanceSpotAdapter::new(&creds, tx, platform)?);
            exit_handler_registry.register(platform, adapter.exit_handler());
            let info = AdapterInfo {
                platform,
                router: adapter.event_router(),
                adapter: adapter.clone(),
            };

            info!(?platform, "Binance Spot adapter registered");
            Ok((adapter, vec![info]))
        }

        // UK-restricted: Binance derivatives/margin unavailable to UK retail (FCA)
        #[cfg(feature = "binance")]
        TradingPlatformKind::BinanceFutures
        | TradingPlatformKind::BinanceMargin
        | TradingPlatformKind::BinanceCoinFutures => {
            anyhow::bail!(
                "GATEWAY_PROVIDER='{provider}' is disabled — Binance derivatives and margin \
                 trading are restricted for UK retail customers by the FCA. \
                 Only binance_spot is available."
            );
        }

        // =====================================================================
        // Oanda
        // =====================================================================
        #[cfg(feature = "oanda")]
        TradingPlatformKind::Oanda => {
            use tektii_gateway_oanda::{OandaAdapter, OandaCredentials};

            let mut creds = OandaCredentials::new(&credentials[0], &credentials[1]);
            if let Ok(url) = env::var("OANDA_BASE_URL") {
                creds = creds.with_rest_url(url);
            }
            if let Ok(stream_url) = env::var("OANDA_STREAM_URL") {
                creds = creds.with_stream_url(stream_url);
            }

            let (tx, _) = broadcast::channel::<WsMessage>(256);
            let adapter = Arc::new(OandaAdapter::new(&creds, tx, platform)?);

            exit_handler_registry.register(platform, adapter.exit_handler());
            let info = AdapterInfo {
                platform,
                router: adapter.event_router(),
                adapter: adapter.clone(),
            };
            info!(?platform, "Oanda adapter registered");
            Ok((adapter, vec![info]))
        }

        // =====================================================================
        // Saxo
        // =====================================================================
        #[cfg(feature = "saxo")]
        TradingPlatformKind::Saxo => {
            use secrecy::SecretBox;
            use tektii_gateway_saxo::{SaxoAdapter, SaxoCredentials};

            let creds = SaxoCredentials {
                client_id: credentials[0].clone(),
                client_secret: SecretBox::new(Box::new(credentials[1].clone())),
                account_key: credentials[2].clone(),
                access_token: env::var("SAXO_ACCESS_TOKEN")
                    .ok()
                    .map(|t| SecretBox::new(Box::new(t))),
                refresh_token: env::var("SAXO_REFRESH_TOKEN")
                    .ok()
                    .map(|t| SecretBox::new(Box::new(t))),
                rest_url: env::var("SAXO_BASE_URL").ok(),
                auth_url: None,
                ws_url: env::var("SAXO_STREAM_URL").ok(),
                precheck_orders: None,
            };

            let (tx, _) = broadcast::channel::<WsMessage>(256);
            let adapter = Arc::new(SaxoAdapter::new(&creds, tx, platform).await?);

            exit_handler_registry.register(platform, adapter.exit_handler());
            let info = AdapterInfo {
                platform,
                router: adapter.event_router(),
                adapter: adapter.clone(),
            };
            info!(?platform, "Saxo adapter registered");
            Ok((adapter, vec![info]))
        }

        // =====================================================================
        // Tektii (backtesting engine adapter)
        // =====================================================================
        #[cfg(feature = "tektii")]
        TradingPlatformKind::Tektii => {
            use tektii_gateway_tektii::{TektiiAdapter, TektiiCredentials};

            let mut creds = TektiiCredentials::new(&credentials[0], &credentials[1]);
            if let Ok(api_key) = env::var("TEKTII_API_KEY") {
                creds = creds.with_api_key(api_key);
            }

            let (tx, _) = broadcast::channel::<WsMessage>(256);
            let adapter = Arc::new(TektiiAdapter::new(&creds, tx)?);

            exit_handler_registry.register(platform, adapter.exit_handler());
            let info = AdapterInfo {
                platform,
                router: adapter.event_router(),
                adapter: adapter.clone(),
            };
            info!(?platform, "Tektii adapter registered");
            Ok((adapter, vec![info]))
        }

        // =====================================================================
        // Mock (local development)
        // =====================================================================
        #[cfg(feature = "mock")]
        TradingPlatformKind::Mock => {
            use tektii_gateway_mock::MockProviderAdapter;

            let (tx, _) = broadcast::channel::<WsMessage>(256);
            let adapter = Arc::new(MockProviderAdapter::new(tx, platform));

            // Store the adapter so connect_mock can create a WS provider that shares
            // the same PriceGenerator and event sink
            let _ = MOCK_ADAPTER.set(Arc::clone(&adapter));

            exit_handler_registry.register(platform, adapter.exit_handler());
            let info = AdapterInfo {
                platform,
                router: adapter.event_router(),
                adapter: adapter.clone(),
            };
            info!(?platform, "Mock adapter registered");
            Ok((adapter, vec![info]))
        }

        // Feature not enabled — unreachable after check_feature_enabled
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("Provider '{provider}' is not available in this build"),
    }
}

/// Connect a provider and register it with the registry.
async fn connect_and_register_provider(
    provider: Box<dyn WebSocketProvider>,
    provider_config: tektii_gateway_core::websocket::provider::ProviderConfig,
    provider_registry: &Arc<ProviderRegistry>,
    platform: TradingPlatform,
    symbols: Vec<String>,
    event_types: Vec<String>,
) -> Result<(), String> {
    match provider.connect(provider_config).await {
        Ok(stream) => provider_registry
            .register_provider(platform, provider, stream, symbols, event_types)
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(feature = "alpaca")]
async fn connect_alpaca(
    platform: TradingPlatform,
    symbols: Vec<String>,
    event_types: Vec<String>,
    provider_registry: &Arc<ProviderRegistry>,
) -> Result<(), String> {
    use tektii_gateway_alpaca::AlpacaWebSocketProvider;
    use tektii_gateway_core::websocket::provider::{Credentials, ProviderConfig};

    let feed = env::var("ALPACA_FEED").unwrap_or_else(|_| "iex".to_string());
    let provider = Box::new(AlpacaWebSocketProvider::new(feed));
    let provider_config = ProviderConfig {
        platform,
        symbols: symbols.clone(),
        event_types: event_types.clone(),
        credentials: Some(Credentials::new(
            env::var("ALPACA_API_KEY").unwrap_or_default(),
            env::var("ALPACA_API_SECRET").unwrap_or_default(),
        )),
        tektii_params: None,
    };

    connect_and_register_provider(
        provider,
        provider_config,
        provider_registry,
        platform,
        symbols,
        event_types,
    )
    .await
}

#[cfg(feature = "binance")]
async fn connect_binance(
    platform: TradingPlatform,
    symbols: Vec<String>,
    event_types: Vec<String>,
    provider_registry: &Arc<ProviderRegistry>,
) -> Result<(), String> {
    use tektii_gateway_binance::BinanceWebSocketProvider;
    use tektii_gateway_core::arc_secret_from_string;
    use tektii_gateway_core::websocket::provider::{Credentials, ProviderConfig};

    let api_key = arc_secret_from_string(env::var("BINANCE_API_KEY").unwrap_or_default());
    let provider = Box::new(BinanceWebSocketProvider::new(platform, api_key));
    let provider_config = ProviderConfig {
        platform,
        symbols: symbols.clone(),
        event_types: event_types.clone(),
        credentials: Some(Credentials::new(
            env::var("BINANCE_API_KEY").unwrap_or_default(),
            env::var("BINANCE_API_SECRET").unwrap_or_default(),
        )),
        tektii_params: None,
    };

    connect_and_register_provider(
        provider,
        provider_config,
        provider_registry,
        platform,
        symbols,
        event_types,
    )
    .await
}

#[cfg(feature = "oanda")]
async fn connect_oanda(
    platform: TradingPlatform,
    symbols: Vec<String>,
    event_types: Vec<String>,
    provider_registry: &Arc<ProviderRegistry>,
) -> Result<(), String> {
    use tektii_gateway_core::websocket::provider::{Credentials, ProviderConfig};
    use tektii_gateway_oanda::{OandaCredentials, OandaWebSocketProvider};

    let api_key = env::var("OANDA_API_KEY").unwrap_or_default();
    let account_id = env::var("OANDA_ACCOUNT_ID").unwrap_or_default();
    let mut creds = OandaCredentials::new(&api_key, &account_id);
    if let Ok(url) = env::var("OANDA_STREAM_URL") {
        creds = creds.with_stream_url(url);
    }

    let provider = Box::new(OandaWebSocketProvider::new(&creds, platform));
    let provider_config = ProviderConfig {
        platform,
        symbols: symbols.clone(),
        event_types: event_types.clone(),
        credentials: Some(Credentials::new(api_key, account_id)),
        tektii_params: None,
    };

    connect_and_register_provider(
        provider,
        provider_config,
        provider_registry,
        platform,
        symbols,
        event_types,
    )
    .await
}

#[cfg(feature = "saxo")]
async fn connect_saxo(
    platform: TradingPlatform,
    symbols: Vec<String>,
    event_types: Vec<String>,
    provider_registry: &Arc<ProviderRegistry>,
) -> Result<(), String> {
    use secrecy::SecretBox;
    use tektii_gateway_core::websocket::provider::ProviderConfig;
    use tektii_gateway_saxo::{SaxoCredentials, SaxoWebSocketProvider};

    let creds = SaxoCredentials {
        client_id: env::var("SAXO_APP_KEY").unwrap_or_default(),
        client_secret: SecretBox::new(Box::new(env::var("SAXO_APP_SECRET").unwrap_or_default())),
        account_key: env::var("SAXO_ACCOUNT_KEY").unwrap_or_default(),
        access_token: env::var("SAXO_ACCESS_TOKEN")
            .ok()
            .map(|t| SecretBox::new(Box::new(t))),
        refresh_token: env::var("SAXO_REFRESH_TOKEN")
            .ok()
            .map(|t| SecretBox::new(Box::new(t))),
        rest_url: env::var("SAXO_BASE_URL").ok(),
        auth_url: None,
        ws_url: env::var("SAXO_STREAM_URL").ok(),
        precheck_orders: None,
    };

    match SaxoWebSocketProvider::new(&creds, platform) {
        Ok(provider) => {
            let provider = Box::new(provider);
            let provider_config = ProviderConfig {
                platform,
                symbols: symbols.clone(),
                event_types: event_types.clone(),
                credentials: None,
                tektii_params: None,
            };

            connect_and_register_provider(
                provider,
                provider_config,
                provider_registry,
                platform,
                symbols,
                event_types,
            )
            .await
        }
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(feature = "tektii")]
async fn connect_tektii(
    config: &GatewayConfig,
    platform: TradingPlatform,
    symbols: Vec<String>,
    event_types: Vec<String>,
    provider_registry: &Arc<ProviderRegistry>,
    event_routers: &[AdapterInfo],
) -> Result<(), String> {
    use tektii_gateway_core::websocket::provider::ProviderConfig;
    use tektii_gateway_tektii::TektiiWebSocketProvider;

    let ws_url = env::var("TEKTII_ENGINE_WS_URL").unwrap_or_default();

    let Some(event_router) = event_routers
        .iter()
        .find(|info| info.platform == platform)
        .map(|info| info.router.clone())
    else {
        return Err(format!("No event router registered for {platform:?}"));
    };

    let subscription_filter = Arc::new(SubscriptionFilter::new(
        &config
            .subscriptions_for_platform(platform)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>(),
    ));

    let provider = Box::new(TektiiWebSocketProvider::new(
        ws_url,
        event_router,
        platform,
        subscription_filter,
    ));

    let provider_config = ProviderConfig {
        platform,
        symbols: symbols.clone(),
        event_types: event_types.clone(),
        credentials: None,
        tektii_params: None,
    };

    // Connect provider and retrieve the ACK bridge before registering
    let stream = provider
        .connect(provider_config)
        .await
        .map_err(|e| e.to_string())?;

    // Register the ACK bridge with the provider registry so strategy ACKs
    // are forwarded to the engine (critical for simulation time sync)
    if let Some(bridge) = provider.ack_bridge().await {
        provider_registry.set_tektii_ack_bridge(bridge).await;
    }

    provider_registry
        .register_provider(platform, provider, stream, symbols, event_types)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(feature = "mock")]
/// Shared mock adapter — set during registration, used to create the WS provider
/// with the same PriceGenerator and event sink.
static MOCK_ADAPTER: std::sync::OnceLock<Arc<tektii_gateway_mock::MockProviderAdapter>> =
    std::sync::OnceLock::new();

#[cfg(feature = "mock")]
async fn connect_mock(
    platform: TradingPlatform,
    symbols: Vec<String>,
    event_types: Vec<String>,
    provider_registry: &Arc<ProviderRegistry>,
    event_routers: &[AdapterInfo],
) -> Result<(), String> {
    use tektii_gateway_core::websocket::provider::ProviderConfig;

    let _ = event_routers;

    let adapter = MOCK_ADAPTER.get().ok_or("Mock adapter not initialised")?;

    let provider = Box::new(adapter.create_ws_provider());
    let provider_config = ProviderConfig {
        platform,
        symbols: symbols.clone(),
        event_types: event_types.clone(),
        credentials: None,
        tektii_params: None,
    };

    connect_and_register_provider(
        provider,
        provider_config,
        provider_registry,
        platform,
        symbols,
        event_types,
    )
    .await
}

/// Connect WebSocket providers for subscribed platforms.
#[allow(unused_variables)]
async fn connect_providers(
    config: &GatewayConfig,
    provider_registry: &Arc<ProviderRegistry>,
    event_routers: &[AdapterInfo],
) {
    for platform in config.platforms_used() {
        let symbols = config.symbols_for_platform(platform);
        let event_types = config.events_for_platform(platform);

        info!(
            ?platform,
            ?symbols,
            ?event_types,
            "Connecting provider WebSocket"
        );

        let result: Result<(), String> = match platform.kind() {
            // -----------------------------------------------------------------
            // Alpaca
            // -----------------------------------------------------------------
            #[cfg(feature = "alpaca")]
            tektii_gateway_core::models::TradingPlatformKind::Alpaca => {
                connect_alpaca(platform, symbols, event_types, provider_registry).await
            }

            // -----------------------------------------------------------------
            // Binance variants
            // -----------------------------------------------------------------
            #[cfg(feature = "binance")]
            tektii_gateway_core::models::TradingPlatformKind::BinanceSpot
            | tektii_gateway_core::models::TradingPlatformKind::BinanceFutures
            | tektii_gateway_core::models::TradingPlatformKind::BinanceMargin
            | tektii_gateway_core::models::TradingPlatformKind::BinanceCoinFutures => {
                connect_binance(platform, symbols, event_types, provider_registry).await
            }

            // -----------------------------------------------------------------
            // Oanda
            // -----------------------------------------------------------------
            #[cfg(feature = "oanda")]
            tektii_gateway_core::models::TradingPlatformKind::Oanda => {
                connect_oanda(platform, symbols, event_types, provider_registry).await
            }

            // -----------------------------------------------------------------
            // Saxo
            // -----------------------------------------------------------------
            #[cfg(feature = "saxo")]
            tektii_gateway_core::models::TradingPlatformKind::Saxo => {
                connect_saxo(platform, symbols, event_types, provider_registry).await
            }

            // -----------------------------------------------------------------
            // Tektii (backtesting engine adapter)
            // -----------------------------------------------------------------
            #[cfg(feature = "tektii")]
            tektii_gateway_core::models::TradingPlatformKind::Tektii => {
                connect_tektii(
                    config,
                    platform,
                    symbols,
                    event_types,
                    provider_registry,
                    event_routers,
                )
                .await
            }

            // -----------------------------------------------------------------
            // Mock (local development)
            // -----------------------------------------------------------------
            #[cfg(feature = "mock")]
            tektii_gateway_core::models::TradingPlatformKind::Mock => {
                connect_mock(
                    platform,
                    symbols,
                    event_types,
                    provider_registry,
                    event_routers,
                )
                .await
            }

            // Catch-all for feature-disabled platforms
            #[allow(unreachable_patterns)]
            _ => {
                warn!(
                    ?platform,
                    "Provider feature not enabled, skipping WebSocket connection"
                );
                Ok(())
            }
        };

        match result {
            Ok(()) => info!(?platform, "Provider WebSocket connected"),
            Err(e) => error!(?platform, error = %e, "Failed to connect provider WebSocket"),
        }
    }
}
