//! Gateway application state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use secrecy::SecretBox;

use crate::adapter::AdapterRegistry;
use crate::correlation::CorrelationStore;
use crate::exit_management::ExitHandlerRegistry;
use crate::websocket::connection::WsConnectionManager;
use crate::websocket::registry::ProviderRegistry;

/// Application state for the gateway.
///
/// Contains all shared state needed by REST handlers and WebSocket server.
/// All fields are Arc-wrapped for cheap cloning.
#[derive(Clone)]
pub struct GatewayState {
    /// Registry of trading adapters.
    adapter_registry: AdapterRegistry,

    /// WebSocket connection manager.
    ws_connection_manager: Arc<WsConnectionManager>,

    /// Provider registry for WebSocket event streaming.
    provider_registry: Arc<ProviderRegistry>,

    /// Registry of per-adapter exit handlers.
    exit_handler_registry: Arc<ExitHandlerRegistry>,

    /// Correlation store mapping provider order IDs to gateway-assigned correlation IDs.
    correlation_store: Arc<CorrelationStore>,

    /// Optional API key for authenticating incoming requests.
    api_key: Option<Arc<SecretBox<String>>>,

    /// Whether the gateway is shutting down.
    shutting_down: Arc<AtomicBool>,
}

impl GatewayState {
    /// Create a new gateway state.
    #[must_use]
    pub fn new(
        adapter_registry: AdapterRegistry,
        ws_connection_manager: Arc<WsConnectionManager>,
        provider_registry: Arc<ProviderRegistry>,
        exit_handler_registry: Arc<ExitHandlerRegistry>,
        correlation_store: Arc<CorrelationStore>,
        api_key: Option<Arc<SecretBox<String>>>,
    ) -> Self {
        Self {
            adapter_registry,
            ws_connection_manager,
            provider_registry,
            exit_handler_registry,
            correlation_store,
            api_key,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get a reference to the adapter registry.
    #[must_use]
    pub const fn adapter_registry(&self) -> &AdapterRegistry {
        &self.adapter_registry
    }

    /// Get the single trading adapter.
    #[must_use]
    pub fn adapter(&self) -> &Arc<dyn crate::adapter::TradingAdapter> {
        self.adapter_registry.adapter()
    }

    /// Get the configured trading platform.
    #[must_use]
    pub fn platform(&self) -> crate::models::TradingPlatform {
        self.adapter_registry.platform()
    }

    /// Get a reference to the WebSocket connection manager.
    #[must_use]
    pub const fn ws_connection_manager(&self) -> &Arc<WsConnectionManager> {
        &self.ws_connection_manager
    }

    /// Get a reference to the provider registry.
    #[must_use]
    pub const fn provider_registry(&self) -> &Arc<ProviderRegistry> {
        &self.provider_registry
    }

    /// Get a reference to the exit handler registry.
    #[must_use]
    pub const fn exit_handler_registry(&self) -> &Arc<ExitHandlerRegistry> {
        &self.exit_handler_registry
    }

    /// Get a reference to the correlation store.
    #[must_use]
    pub const fn correlation_store(&self) -> &Arc<CorrelationStore> {
        &self.correlation_store
    }

    /// Get a reference to the configured API key (if any).
    #[must_use]
    pub fn api_key(&self) -> Option<&SecretBox<String>> {
        self.api_key.as_deref()
    }

    /// Check if the gateway is shutting down.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Signal that the gateway is shutting down.
    pub fn set_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }
}

impl std::fmt::Debug for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayState")
            .field("adapter_registry", &self.adapter_registry)
            .field("ws_connection_manager", &self.ws_connection_manager)
            .field("provider_registry", &"<ProviderRegistry>")
            .field("exit_handler_registry", &self.exit_handler_registry)
            .field("correlation_store", &self.correlation_store)
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "<redacted>"
                } else {
                    "None"
                },
            )
            .field("shutting_down", &self.is_shutting_down())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::test_helpers::test_gateway_state;

    #[test]
    fn shutdown_flag_defaults_to_false() {
        let state = test_gateway_state();
        assert!(!state.is_shutting_down());
    }

    #[test]
    fn set_shutting_down_sets_flag() {
        let state = test_gateway_state();
        state.set_shutting_down();
        assert!(state.is_shutting_down());
    }
}
