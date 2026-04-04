use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Detects GCP via `K_SERVICE` (Cloud Run) or `ENVIRONMENT` != "local".
pub fn is_gcp() -> bool {
    std::env::var("K_SERVICE").is_ok()
        || std::env::var("ENVIRONMENT")
            .map(|e| e != "local")
            .unwrap_or(false)
}

/// Initializes tracing with environment-aware formatting.
///
/// - **GCP**: JSON-formatted structured logs for Cloud Logging.
/// - **Local**: Pretty-printed human-readable logs.
///
/// Default level is `"info"`, overridden by `RUST_LOG`.
pub fn init_tracing(service_name: &str, version: &str) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if is_gcp() {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().pretty())
            .init();
    }

    tracing::info!(
        service = %service_name,
        version = %version,
        "Gateway telemetry initialized"
    );
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    /// Helper to clear GCP-detection env vars.
    unsafe fn clear_env() {
        unsafe {
            std::env::remove_var("K_SERVICE");
            std::env::remove_var("ENVIRONMENT");
        }
    }

    /// All is_gcp tests share process env vars, so they run sequentially
    /// within a single test to avoid races from parallel test threads.
    #[test]
    fn is_gcp_detection() {
        // --- false locally (no env vars) ---
        unsafe { clear_env() };
        assert!(!is_gcp());

        // --- true when K_SERVICE set ---
        unsafe { std::env::set_var("K_SERVICE", "my-service") };
        assert!(is_gcp());

        // --- true when ENVIRONMENT is not "local" ---
        unsafe {
            clear_env();
            std::env::set_var("ENVIRONMENT", "production");
        }
        assert!(is_gcp());

        // --- false when ENVIRONMENT is "local" ---
        unsafe {
            clear_env();
            std::env::set_var("ENVIRONMENT", "local");
        }
        assert!(!is_gcp());

        // cleanup
        unsafe { clear_env() };
    }
}
