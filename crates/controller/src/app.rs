use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::signal;
use tracing::info;

use crate::auth_store::AuthStore;
use crate::config::AppConfig;
use crate::ha_auth::{LoginFlowStore, TokenStore};
use crate::ha_webhook::WebhookStore;
use crate::http;
use crate::mobile_device_store::MobileDeviceStore;
use crate::service::ServiceRegistry;
use crate::state_store::StateStore;
use crate::storage::Storage;
use crate::zeroconf;

pub struct AppState {
    pub config: AppConfig,
    pub storage: Storage,
    pub auth: AuthStore,
    pub mobile_devices: MobileDeviceStore,
    pub states: StateStore,
    pub tokens: TokenStore,
    pub flows: LoginFlowStore,
    pub webhooks: WebhookStore,
    pub services: ServiceRegistry,
}

impl AppState {
    pub fn new(config: AppConfig, storage: Storage) -> Self {
        let auth = AuthStore::new(storage.root().to_path_buf());
        let mobile_devices = MobileDeviceStore::new(storage.root().to_path_buf());
        Self {
            auth,
            mobile_devices,
            config,
            storage,
            states: StateStore::new(),
            tokens: TokenStore::new(),
            flows: LoginFlowStore::new(),
            webhooks: WebhookStore::new(),
            services: ServiceRegistry::new(),
        }
    }
}

pub async fn run(config: AppConfig) -> Result<()> {
    let listen_addr = config.listen_addr();
    let storage = Storage::new(config.storage.data_dir.clone()).await?;
    let state = Arc::new(AppState::new(config, storage));
    let _zeroconf = zeroconf::announce(&state).await?;

    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;

    info!(address = %listen_addr, "home edge listening");

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
