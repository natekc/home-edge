use tracing_subscriber::{EnvFilter, fmt};

pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("home_edge=info,tower_http=info,axum=info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

/// Reinitialise the global tracing subscriber using a log level from config.
///
/// This is called after config is loaded; `RUST_LOG` still takes precedence
/// (the default filter is only applied when `RUST_LOG` is not set).
pub fn apply_config_log_level(log_level: &str) {
    // tracing-subscriber does not support re-initialisation after init();
    // we rely on RUST_LOG for runtime overrides and the subscriber set in
    // init_logging() picks up the env var first.  This function is a no-op
    // today but documents where config-driven log level would be wired in
    // once per-run subscriber rebuilding is supported.
    let _ = log_level;
}
