use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum_test::TestServer;
use home_edge::app::AppState;
use home_edge::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
use home_edge::ha_auth::{LoginFlowStore, TokenStore};
use home_edge::ha_webhook::WebhookStore;
use home_edge::http;
use home_edge::state_store::StateStore;
use home_edge::storage::{OnboardingState, Storage};

pub async fn test_server(onboarded: bool) -> TestServer {
    test_server_with_onboarding(OnboardingState {
        onboarded,
        ..OnboardingState::default()
    })
    .await
}

pub async fn test_server_with_onboarding(onboarding: OnboardingState) -> TestServer {
    let storage_root = temp_dir("storage");
    let storage = Storage::new(storage_root.clone()).await.expect("storage init");
    storage
        .save_onboarding(&onboarding)
        .await
        .expect("save onboarding state");

    let state = Arc::new(AppState {
        config: AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
            },
            storage: StorageConfig {
                data_dir: storage_root,
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
        },
        storage,
        states: StateStore::new(),
        tokens: TokenStore::new(),
        flows: LoginFlowStore::new(),
        webhooks: WebhookStore::new(),
    });

    TestServer::new(http::router(state)).expect("test server")
}

fn temp_dir(prefix: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("home-edge-{prefix}-{nanos}-{unique}"))
}