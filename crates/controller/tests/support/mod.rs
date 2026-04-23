use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum_test::{TestServer, TestServerConfig, Transport};
use home_edge::app::AppState;
use home_edge::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
use home_edge::http;
use home_edge::storage::{OnboardingState, Storage, StoredUser};
use serde_json::{Value, json};

#[allow(dead_code)]
pub async fn test_server(onboarded: bool) -> TestServer {
    test_server_with_onboarding(OnboardingState {
        onboarded,
        ..OnboardingState::default()
    })
    .await
}

#[allow(dead_code)]
pub async fn test_server_with_onboarding(onboarding: OnboardingState) -> TestServer {
    let (server, _) = test_server_and_state(onboarding).await;
    server
}

#[allow(dead_code)]
pub async fn test_server_and_state(onboarding: OnboardingState) -> (TestServer, Arc<AppState>) {
    build_server_and_state(onboarding, false).await
}

#[allow(dead_code)]
pub async fn test_ws_server_and_state(onboarding: OnboardingState) -> (TestServer, Arc<AppState>) {
    build_server_and_state(onboarding, true).await
}

#[allow(dead_code)]
pub fn completed_onboarding() -> OnboardingState {
    OnboardingState {
        onboarded: true,
        done: vec!["user".into(), "core_config".into()],
        user: Some(StoredUser {
            name: "Test User".into(),
            username: "test-user".into(),
            password: "test-pass".into(),
            language: "en".into(),
        }),
        location_name: Some("Test Home".into()),
        country: Some("US".into()),
        language: Some("en".into()),
        time_zone: Some("UTC".into()),
        unit_system: Some("metric".into()),
        ..OnboardingState::default()
    }
}

#[allow(dead_code)]
pub async fn issue_access_token(server: &TestServer) -> String {
    let flow_response = server
        .post("/auth/login_flow")
        .json(&json!({
            "client_id": "https://home-assistant.io/iOS",
            "handler": ["homeassistant", null],
            "redirect_uri": "homeassistant://auth-callback"
        }))
        .await;
    flow_response.assert_status_ok();
    let flow = flow_response.json::<Value>();

    let auth_response = server
        .post(
            format!(
                "/auth/login_flow/{}",
                flow["flow_id"].as_str().expect("flow id")
            )
            .as_str(),
        )
        .json(&json!({
            "client_id": "https://home-assistant.io/iOS",
            "username": "test-user",
            "password": "test-pass"
        }))
        .await;
    auth_response.assert_status_ok();
    let auth_step = auth_response.json::<Value>();

    let token_response = server
        .post("/auth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            (
                "code",
                auth_step["result"].as_str().expect("authorization code"),
            ),
            ("client_id", "https://home-assistant.io/iOS"),
        ])
        .await;
    token_response.assert_status_ok();
    let tokens = token_response.json::<Value>();
    tokens["access_token"]
        .as_str()
        .expect("access token")
        .to_string()
}

async fn build_server_and_state(
    onboarding: OnboardingState,
    websocket_transport: bool,
) -> (TestServer, Arc<AppState>) {
    let storage_root = temp_dir("storage");
    let storage = Storage::new(storage_root.clone())
        .await
        .expect("storage init");
    storage
        .save_onboarding(&onboarding)
        .await
        .expect("save onboarding state");

    let config = AppConfig {
        server: ServerConfig {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            log_level: tracing::Level::INFO,
        },
        storage: StorageConfig {
            data_dir: storage_root,
        },
        ui: UiConfig {
            product_name: "Test Home".into(),
        },
        areas: home_edge::config::AreasConfig::default(),
        home_zone: home_edge::config::HomeZoneConfig::default(),
        history: home_edge::config::HistoryConfig::default(),
            mdns: Default::default(),
    };
    let state = Arc::new(
        AppState::new_initialized(config, storage)
            .await
            .expect("init app state"),
    );
    if let Some(user) = onboarding.user.as_ref() {
        state
            .auth
            .save_user(user)
            .await
            .expect("save auth user");
    }

    let server = if websocket_transport {
        TestServer::new_with_config(
            http::router(Arc::clone(&state)),
            TestServerConfig {
                transport: Some(Transport::HttpRandomPort),
                ..Default::default()
            },
        )
        .expect("test server")
    } else {
        TestServer::new(http::router(Arc::clone(&state))).expect("test server")
    };
    (server, state)
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
