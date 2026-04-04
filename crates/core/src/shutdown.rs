//! Shutdown utilities for graceful gateway termination.
//!
//! Provides functions to log and persist exit handler state during shutdown,
//! ensuring operators know which positions may be unprotected.

use std::path::Path;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::api::state::GatewayState;
use crate::exit_management::ExitHandlerRegistry;
use crate::exit_management::types::{ExitEntryStatus, ExitLegType};
use crate::models::{Side, TradingPlatform};
use crate::websocket::connection::WsConnectionManager;
use crate::websocket::registry::ProviderRegistry;

// =============================================================================
// Shutdown Sequence
// =============================================================================

/// Execute the graceful shutdown sequence.
///
/// Steps (in order):
/// 1. Notify connected strategies (Disconnecting event + Close frame)
/// 2. Set shutdown flag (new orders rejected, /readyz returns 503)
/// 3. Log pending exit entries at error severity
/// 4. Write exit state snapshot to disk
/// 5. Disconnect broker WebSocket connections
/// 6. Cancel background tasks
pub async fn run_shutdown_sequence(
    ws_connection_manager: &WsConnectionManager,
    shutdown_state: &GatewayState,
    exit_handler_registry: &ExitHandlerRegistry,
    exit_state_path: &Path,
    provider_registry: &ProviderRegistry,
    cancel_token: &CancellationToken,
) {
    tracing::info!("Shutting down...");

    // Step 1: Notify connected strategies
    ws_connection_manager.broadcast_shutdown().await;

    // Step 2: Set shutdown flag — new orders rejected, /readyz returns 503
    shutdown_state.set_shutting_down();

    // Step 3: Log pending exit entries at error severity
    log_pending_exits(exit_handler_registry);

    // Step 4: Write exit state snapshot to disk
    if let Err(e) = write_exit_state_snapshot(exit_handler_registry, exit_state_path).await {
        tracing::error!(error = %e, "Failed to write exit state snapshot");
    }

    // Step 5: Disconnect broker WebSocket connections
    provider_registry.shutdown().await;

    // Step 6: Cancel background tasks
    cancel_token.cancel();

    tracing::info!("Gateway stopped");
}

// =============================================================================
// Logging
// =============================================================================

/// Log all pending and failed exit entries at error severity.
///
/// Emits a single `tracing::error!` if any non-terminal entries exist,
/// listing each entry's key details. If no entries are pending, nothing is logged.
pub fn log_pending_exits(registry: &ExitHandlerRegistry) {
    let handlers = registry.all();
    let mut pending_count = 0usize;
    let mut failed_count = 0usize;
    let mut details = Vec::new();

    for (platform, handler) in &handlers {
        for entry in handler.get_pending_entries() {
            pending_count += 1;
            details.push(format!(
                "  PENDING: platform={}, primary_order={}, type={:?}, created={}",
                platform, entry.primary_order_id, entry.order_type, entry.created_at,
            ));
        }

        for entry in handler.get_failed_entries_internal() {
            failed_count += 1;
            details.push(format!(
                "  FAILED: platform={}, primary_order={}, symbol={}, type={:?}, error={}",
                platform, entry.primary_order_id, entry.symbol, entry.order_type, entry.error,
            ));
        }
    }

    let total = pending_count + failed_count;
    if total > 0 {
        tracing::error!(
            pending_count,
            failed_count,
            total,
            "Pending exits will NOT be placed — manual intervention required\n{}",
            details.join("\n"),
        );
    }
}

// =============================================================================
// Exit State Snapshot
// =============================================================================

/// Snapshot of all non-terminal exit entries, written to disk on shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitStateSnapshot {
    /// When the snapshot was taken.
    pub timestamp: DateTime<Utc>,
    /// All non-terminal exit entries at time of shutdown.
    pub entries: Vec<ExitStateEntry>,
}

/// A serializable representation of an exit entry for the snapshot file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitStateEntry {
    pub placeholder_id: String,
    pub primary_order_id: String,
    pub symbol: String,
    pub side: Side,
    pub order_type: ExitLegType,
    pub trigger_price: Decimal,
    pub limit_price: Option<Decimal>,
    pub quantity: Decimal,
    pub original_quantity: Decimal,
    pub status: String,
    pub platform: TradingPlatform,
    pub actual_order_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl ExitStateEntry {
    /// Create from an `ExitEntry`, mapping non-serializable fields.
    fn from_exit_entry(entry: &crate::exit_management::types::ExitEntry) -> Self {
        let (status_str, actual_order_ids) = match &entry.status {
            ExitEntryStatus::Pending => ("pending".to_string(), vec![]),
            ExitEntryStatus::PartiallyTriggered { actual_orders, .. } => (
                "partially_triggered".to_string(),
                actual_orders.iter().map(|o| o.order_id.clone()).collect(),
            ),
            ExitEntryStatus::Placed { actual_orders } => (
                "placed".to_string(),
                actual_orders.iter().map(|o| o.order_id.clone()).collect(),
            ),
            ExitEntryStatus::Failed {
                error,
                actual_orders,
                ..
            } => (
                format!("failed: {error}"),
                actual_orders.iter().map(|o| o.order_id.clone()).collect(),
            ),
            ExitEntryStatus::Cancelled { reason, .. } => (format!("cancelled: {reason:?}"), vec![]),
            ExitEntryStatus::Expired { .. } => ("expired".to_string(), vec![]),
        };

        Self {
            placeholder_id: entry.placeholder_id.clone(),
            primary_order_id: entry.primary_order_id.clone(),
            symbol: entry.symbol.clone(),
            side: entry.side,
            order_type: entry.order_type,
            trigger_price: entry.trigger_price,
            limit_price: entry.limit_price,
            quantity: entry.quantity,
            original_quantity: entry.original_quantity,
            status: status_str,
            platform: entry.platform,
            actual_order_ids,
            created_at: entry.created_at_utc,
            updated_at: entry.updated_at,
        }
    }
}

/// Write non-terminal exit handler entries to a JSON file.
///
/// If no entries exist, no file is written.
/// Uses atomic write (write to .tmp, then rename) to prevent partial files.
pub async fn write_exit_state_snapshot(
    registry: &ExitHandlerRegistry,
    path: &Path,
) -> std::io::Result<()> {
    let handlers = registry.all();
    let mut entries = Vec::new();

    for (_platform, handler) in &handlers {
        for entry in handler.get_non_terminal_entries() {
            entries.push(ExitStateEntry::from_exit_entry(&entry));
        }
    }

    if entries.is_empty() {
        tracing::debug!("No non-terminal exit entries — skipping snapshot");
        return Ok(());
    }

    let snapshot = ExitStateSnapshot {
        timestamp: Utc::now(),
        entries,
    };

    let json = serde_json::to_string_pretty(&snapshot)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let tmp_path = path.with_extension("tmp");
    tokio::fs::write(&tmp_path, json).await?;
    tokio::fs::rename(&tmp_path, path).await?;

    tracing::info!(
        path = %path.display(),
        entry_count = snapshot.entries.len(),
        "Exit state snapshot written to disk",
    );

    Ok(())
}

/// On startup, check for a previous exit state snapshot and log a warning.
///
/// Does NOT delete the file — reconciliation (story 4.4) will consume it.
pub async fn read_exit_state_on_startup(path: &Path) {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => match serde_json::from_str::<ExitStateSnapshot>(&content) {
            Ok(snapshot) => {
                tracing::warn!(
                    path = %path.display(),
                    entry_count = snapshot.entries.len(),
                    snapshot_time = %snapshot.timestamp,
                    "Previous exit state snapshot found — positions may be unprotected",
                );
                for entry in &snapshot.entries {
                    tracing::warn!(
                        primary_order = %entry.primary_order_id,
                        symbol = %entry.symbol,
                        order_type = ?entry.order_type,
                        status = %entry.status,
                        "  Unreconciled exit entry",
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse exit state snapshot — file may be corrupt",
                );
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No snapshot file — normal startup
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "Failed to read exit state snapshot",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rust_decimal_macros::dec;
    use tempfile::TempDir;

    use super::*;
    use crate::exit_management::handler::ExitHandler;
    use crate::exit_management::types::{ExitEntry, ExitEntryParams, ExitLegType};
    use crate::state::StateManager;

    fn test_registry_with_pending(n: usize) -> ExitHandlerRegistry {
        let state_manager = Arc::new(StateManager::new());
        let handler = ExitHandler::with_defaults(state_manager, TradingPlatform::AlpacaPaper);

        for i in 0..n {
            let entry = ExitEntry::new(ExitEntryParams {
                primary_order_id: format!("order-{i}"),
                order_type: ExitLegType::StopLoss,
                symbol: format!("SYM{i}"),
                side: Side::Sell,
                trigger_price: dec!(100.0),
                limit_price: None,
                quantity: dec!(10.0),
                platform: TradingPlatform::AlpacaPaper,
            });
            handler.insert_entry(entry);
        }

        let registry = ExitHandlerRegistry::new();
        registry.register(TradingPlatform::AlpacaPaper, Arc::new(handler));
        registry
    }

    // ========================================================================
    // log_pending_exits tests
    // ========================================================================

    #[test]
    fn log_pending_exits_with_entries_runs_without_panic() {
        let registry = test_registry_with_pending(3);
        assert_eq!(registry.total_pending_count(), 3);
        log_pending_exits(&registry);
    }

    #[test]
    fn log_pending_exits_with_no_entries_is_noop() {
        let registry = ExitHandlerRegistry::new();
        log_pending_exits(&registry);
    }

    // ========================================================================
    // write_exit_state_snapshot tests
    // ========================================================================

    #[tokio::test]
    async fn write_snapshot_with_entries_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exit-state.json");

        let registry = test_registry_with_pending(2);
        write_exit_state_snapshot(&registry, &path).await.unwrap();

        assert!(path.exists());

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let snapshot: ExitStateSnapshot = serde_json::from_str(&content).unwrap();
        assert_eq!(snapshot.entries.len(), 2);

        // Verify fields
        for entry in &snapshot.entries {
            assert_eq!(entry.side, Side::Sell);
            assert_eq!(entry.order_type, ExitLegType::StopLoss);
            assert_eq!(entry.status, "pending");
            assert_eq!(entry.platform, TradingPlatform::AlpacaPaper);
        }
    }

    #[tokio::test]
    async fn write_snapshot_with_no_entries_skips_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exit-state.json");

        let registry = ExitHandlerRegistry::new();
        write_exit_state_snapshot(&registry, &path).await.unwrap();

        assert!(!path.exists());
    }

    // ========================================================================
    // read_exit_state_on_startup tests
    // ========================================================================

    #[tokio::test]
    async fn read_snapshot_on_startup_logs_warning() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exit-state.json");

        // Write a snapshot first
        let registry = test_registry_with_pending(2);
        write_exit_state_snapshot(&registry, &path).await.unwrap();

        // Read it back — should not panic
        read_exit_state_on_startup(&path).await;
    }

    #[tokio::test]
    async fn read_snapshot_on_startup_no_file_is_noop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");

        // Should not panic
        read_exit_state_on_startup(&path).await;
    }

    // ========================================================================
    // Roundtrip test
    // ========================================================================

    #[tokio::test]
    async fn snapshot_roundtrip_preserves_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exit-state.json");

        let registry = test_registry_with_pending(3);
        write_exit_state_snapshot(&registry, &path).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let snapshot: ExitStateSnapshot = serde_json::from_str(&content).unwrap();

        assert_eq!(snapshot.entries.len(), 3);
        // All entries should have unique primary_order_ids
        let mut ids: Vec<_> = snapshot
            .entries
            .iter()
            .map(|e| &e.primary_order_id)
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }
}
