use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::signal;
use tracing::info;

use crate::config::AppConfig;
use crate::ha_auth::{LoginFlowStore, TokenStore};
use crate::ha_webhook::WebhookStore;
use crate::http;
use crate::state_store::StateStore;
use crate::storage::Storage;

pub struct AppState {
    pub config: AppConfig,
    pub storage: Storage,
    pub states: StateStore,
    pub tokens: TokenStore,
    pub flows: LoginFlowStore,
    pub webhooks: WebhookStore,
}

pub async fn run(config: AppConfig) -> Result<()> {
    let listen_addr = config.listen_addr();
    let storage = Storage::new(config.storage.data_dir.clone()).await?;
    let state = Arc::new(AppState {
        config,
        storage,
        states: StateStore::new(),
        tokens: TokenStore::new(),
        flows: LoginFlowStore::new(),
        webhooks: WebhookStore::new(),
    });

    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;

    info!(address = %listen_addr, "control plane listening");

    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server exited unexpectedly")?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut signal = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let _ = signal.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
