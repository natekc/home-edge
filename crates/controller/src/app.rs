use anyhow::Result;

#[cfg(feature = "transport_wifi")]
use anyhow::Context;
#[cfg(feature = "transport_ble")]
use anyhow::bail;

use crate::config::AppConfig;
use crate::core::AppCore;
#[cfg(feature = "transport_wifi")]
use crate::core::RuntimeMode;

#[cfg(feature = "transport_wifi")]
use std::sync::Arc;
#[cfg(feature = "transport_wifi")]
use tokio::signal;
#[cfg(feature = "transport_wifi")]
use tracing::info;

#[cfg(feature = "transport_wifi")]
use crate::auth_store::AuthStore;
#[cfg(feature = "transport_wifi")]
use crate::ha_auth::{LoginFlowStore, TokenStore};
#[cfg(feature = "transport_wifi")]
use crate::ha_webhook::WebhookStore;
#[cfg(feature = "transport_wifi")]
use crate::http;
#[cfg(feature = "transport_wifi")]
use crate::mobile_device_store::MobileDeviceStore;
#[cfg(feature = "transport_wifi")]
use crate::mobile_entity_store::MobileEntityStore;
#[cfg(feature = "transport_wifi")]
use crate::service::ServiceRegistry;
#[cfg(feature = "transport_wifi")]
use crate::state_store::StateStore;
#[cfg(feature = "transport_wifi")]
use crate::storage::Storage;
#[cfg(feature = "transport_wifi")]
use crate::zeroconf;

#[cfg(feature = "transport_wifi")]
pub struct AppState {
    pub core: AppCore,
    pub config: AppConfig,
    pub storage: Storage,
    pub auth: AuthStore,
    pub mobile_devices: MobileDeviceStore,
    pub mobile_entities: MobileEntityStore,
    pub states: StateStore,
    pub tokens: TokenStore,
    pub flows: LoginFlowStore,
    pub webhooks: WebhookStore,
    pub services: ServiceRegistry,
}

#[cfg(feature = "transport_wifi")]
impl AppState {
    pub fn new(config: AppConfig, storage: Storage) -> Self {
        let auth = AuthStore::new(storage.root().to_path_buf());
        let mobile_devices = MobileDeviceStore::new(storage.root().to_path_buf());
        let mobile_entities = MobileEntityStore::new(storage.root().to_path_buf());
        Self {
            core: AppCore::new(),
            auth,
            mobile_devices,
            mobile_entities,
            config,
            storage,
            states: StateStore::new(),
            tokens: TokenStore::new(),
            flows: LoginFlowStore::new(),
            webhooks: WebhookStore::new(),
            services: ServiceRegistry::new(),
        }
    }

    pub async fn new_initialized(config: AppConfig, storage: Storage) -> Result<Self> {
        let state = Self::new(config, storage);
        let onboarding = state.storage.load_onboarding().await?;
        state
            .core
            .set_runtime_mode(RuntimeMode::from_persisted_onboarding(onboarding.onboarded));
        Ok(state)
    }
}

#[cfg(feature = "transport_wifi")]
pub async fn run(config: AppConfig) -> Result<()> {
    let listen_addr = config.listen_addr();
    let storage = Storage::new(config.storage.data_dir.clone()).await?;
    let state = Arc::new(AppState::new_initialized(config, storage).await?);
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

#[cfg(feature = "transport_ble")]
pub async fn run(_config: AppConfig) -> Result<()> {
    let _core = AppCore::new();
    bail!("BLE transport build is not implemented yet")
}

#[cfg(feature = "transport_wifi")]
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

#[cfg(all(test, feature = "transport_wifi"))]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    use super::*;
    use crate::config::{ServerConfig, StorageConfig, UiConfig};
    use crate::storage::{OnboardingState, Storage};

    fn test_config() -> AppConfig {
        AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/home-edge-app-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
        }
    }

    #[tokio::test]
    async fn initialized_state_uses_unprovisioned_mode_when_not_onboarded() {
        let storage = Storage::new_in_memory();
        storage
            .save_onboarding(&OnboardingState::default())
            .await
            .expect("save onboarding");

        let state = AppState::new_initialized(test_config(), storage)
            .await
            .expect("init app state");

        assert_eq!(state.core.runtime_mode(), RuntimeMode::UnprovisionedWifi);
    }

    #[tokio::test]
    async fn initialized_state_uses_operational_mode_when_onboarded() {
        let storage = Storage::new_in_memory();
        storage
            .save_onboarding(&OnboardingState {
                onboarded: true,
                done: vec!["user".into(), "core_config".into()],
                ..OnboardingState::default()
            })
            .await
            .expect("save onboarding");

        let state = AppState::new_initialized(test_config(), storage)
            .await
            .expect("init app state");

        assert_eq!(state.core.runtime_mode(), RuntimeMode::WifiOperational);
    }
}
