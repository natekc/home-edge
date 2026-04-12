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
use crate::area_registry_store::AreaRegistryStore;
#[cfg(feature = "transport_wifi")]
use crate::notification_store::NotificationStore;
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
use tokio::sync::broadcast::error::RecvError;

#[cfg(feature = "transport_wifi")]
pub struct AppState {
    pub core: AppCore,
    pub config: AppConfig,
    pub storage: Storage,
    pub auth: AuthStore,
    pub area_registry: AreaRegistryStore,
    pub mobile_devices: MobileDeviceStore,
    pub mobile_entities: MobileEntityStore,
    pub states: StateStore,
    pub tokens: TokenStore,
    pub flows: LoginFlowStore,
    pub webhooks: WebhookStore,
    pub services: ServiceRegistry,
    pub templates: minijinja::Environment<'static>,
    pub history: crate::history_store::HistoryStore,
    pub logbook: crate::logbook_store::LogbookStore,
    pub notifications: NotificationStore,
}

#[cfg(feature = "transport_wifi")]
impl AppState {
    pub fn new(config: AppConfig, storage: Storage) -> Self {
        let auth = AuthStore::new(storage.root().to_path_buf());
        let area_registry = AreaRegistryStore::new(storage.root().to_path_buf());
        let mobile_devices = MobileDeviceStore::new(storage.root().to_path_buf());
        let mobile_entities = MobileEntityStore::new(storage.root().to_path_buf());
        let tokens = TokenStore::new(storage.root().to_path_buf());
        let history_capacity = config.history.capacity;
        Self {
            core: AppCore::new(),
            auth,
            area_registry,
            mobile_devices,
            mobile_entities,
            config,
            storage,
            states: StateStore::new(),
            tokens,
            flows: LoginFlowStore::new(),
            webhooks: WebhookStore::new(),
            services: ServiceRegistry::new(),
            templates: crate::templates::build_env(),
            history: crate::history_store::HistoryStore::new(history_capacity),
            logbook: crate::logbook_store::LogbookStore::new(history_capacity),
            notifications: NotificationStore::new(),
        }
    }

    pub async fn new_initialized(config: AppConfig, storage: Storage) -> Result<Self> {
        let state = Self::new(config, storage);
        let onboarding = state.storage.load_onboarding().await?;
        state
            .core
            .set_runtime_mode(RuntimeMode::from_persisted_onboarding(onboarding.onboarded));
        state.tokens.load_persisted().await?;
        state
            .area_registry
            .seed_if_empty(&state.config.areas.names)
            .await?;
        Ok(state)
    }

    /// Render a named minijinja template and return the HTML string.
    pub fn render_html(
        &self,
        name: &str,
        ctx: minijinja::Value,
    ) -> Result<String, minijinja::Error> {
        self.templates.get_template(name)?.render(ctx)
    }
}

#[cfg(feature = "transport_wifi")]
pub async fn run(config: AppConfig, reset: bool) -> Result<()> {
    let listen_addr = config.listen_addr();

    if reset {
        let data_dir = &config.storage.data_dir;
        if data_dir.exists() {
            tokio::fs::remove_dir_all(data_dir)
                .await
                .with_context(|| format!("failed to wipe data dir {}", data_dir.display()))?;
            tracing::warn!(path = %data_dir.display(), "--reset: wiped data directory");
        } else {
            tracing::info!("--reset: data directory does not exist, nothing to wipe");
        }
    }

    let storage = Storage::new(config.storage.data_dir.clone()).await?;
    let state = Arc::new(AppState::new_initialized(config, storage).await?);

    // Spawn logbook listener: subscribes to StateStore broadcast and records entries.
    // state.clone() increments the Arc reference count — the spawned task needs its
    // own owned Arc handle since tokio::spawn requires 'static.
    {
        let state_clone = state.clone();
        let mut rx = state.states.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let old = event
                            .old_state
                            .as_ref()
                            .map(|s| s.state.as_str())
                            .unwrap_or("")
                            .to_string();
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .expect("system clock is before UNIX epoch")
                            .as_secs();
                        let display_name = event
                            .state
                            .attributes
                            .get("friendly_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&event.state.entity_id)
                            .to_string();
                        let entry = crate::logbook_store::LogbookEntry {
                            ts,
                            entity_id: event.state.entity_id.clone(),
                            display_name,
                            old_state: old,
                            new_state: event.state.state.clone(),
                        };
                        state_clone.logbook.record(entry).await;
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "logbook listener lagged, {n} events dropped");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

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
pub async fn run(_config: AppConfig, _reset: bool) -> Result<()> {
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
    use crate::config::{AreasConfig, ServerConfig, StorageConfig, UiConfig};
    use crate::storage::{OnboardingState, Storage};

    fn test_config() -> AppConfig {
        AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                log_level: tracing::Level::INFO,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/home-edge-app-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
            areas: AreasConfig::default(),
            history: crate::config::HistoryConfig::default(),
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
