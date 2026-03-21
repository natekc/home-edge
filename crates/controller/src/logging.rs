use tracing_subscriber::{EnvFilter, fmt};

pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("pi_control_plane=info,tower_http=info,axum=info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
