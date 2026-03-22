mod support;

use axum::http::StatusCode;
use serde_json::json;

use home_edge::storage::{OnboardingState, StoredUser};

#[tokio::test]
async fn onboarding_status_returns_step_progress() {
    let server = support::test_server(false).await;

    let response = server.get("/api/onboarding").await;

    response.assert_status_ok();
    let json = response.json::<serde_json::Value>();
    assert_eq!(
        json,
        json!([
            {"step": "user", "done": false},
            {"step": "core_config", "done": false}
        ])
    );
}

#[tokio::test]
async fn onboarding_installation_type_available_before_completion() {
    let server = support::test_server(false).await;

    let response = server.get("/api/onboarding/installation_type").await;

    response.assert_status_ok();
    let json = response.json::<serde_json::Value>();
    assert_eq!(json, json!({"installation_type": "Home Edge"}));
}

#[tokio::test]
async fn onboarding_users_creates_first_user_and_returns_auth_code() {
    let server = support::test_server(false).await;

    let response = server
        .post("/api/onboarding/users")
        .json(&json!({
            "client_id": "https://example.com",
            "name": "Test Name",
            "username": "test-user",
            "password": "test-pass",
            "language": "en"
        }))
        .await;

    response.assert_status_ok();
    let json = response.json::<serde_json::Value>();
    assert!(json["auth_code"].as_str().is_some());

    let progress = server.get("/api/onboarding").await.json::<serde_json::Value>();
    assert_eq!(
        progress,
        json!([
            {"step": "user", "done": true},
            {"step": "core_config", "done": false}
        ])
    );
}

#[tokio::test]
async fn onboarding_users_cannot_run_twice() {
    let server = support::test_server_with_onboarding(OnboardingState {
        done: vec!["user".into()],
        user: Some(StoredUser {
            name: "Test Name".into(),
            username: "test-user".into(),
            password: "test-pass".into(),
            language: "en".into(),
        }),
        ..OnboardingState::default()
    })
    .await;

    let response = server
        .post("/api/onboarding/users")
        .json(&json!({
            "client_id": "https://example.com",
            "name": "Second User",
            "username": "second-user",
            "password": "second-pass",
            "language": "en"
        }))
        .await;

    response.assert_status(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn onboarding_core_config_requires_user_step_first() {
    let server = support::test_server(false).await;

    let response = server.post("/api/onboarding/core_config").await;

    response.assert_status(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn onboarding_core_config_marks_system_onboarded() {
    let server = support::test_server_with_onboarding(OnboardingState {
        done: vec!["user".into()],
        user: Some(StoredUser {
            name: "Test Name".into(),
            username: "test-user".into(),
            password: "test-pass".into(),
            language: "en".into(),
        }),
        ..OnboardingState::default()
    })
    .await;

    let response = server
        .post("/api/onboarding/core_config")
        .json(&json!({
            "location_name": "My Home",
            "country": "US",
            "language": "en",
            "time_zone": "UTC",
            "unit_system": "metric"
        }))
        .await;

    response.assert_status_ok();

    let progress = server.get("/api/onboarding").await.json::<serde_json::Value>();
    assert_eq!(
        progress,
        json!([
            {"step": "user", "done": true},
            {"step": "core_config", "done": true}
        ])
    );

    let auth_response = server.get("/auth/providers").await;
    auth_response.assert_status_ok();
}

#[tokio::test]
async fn login_flow_uses_stored_onboarding_credentials() {
    let server = support::test_server_with_onboarding(OnboardingState {
        onboarded: true,
        done: vec!["user".into(), "core_config".into()],
        user: Some(StoredUser {
            name: "Test Name".into(),
            username: "test-user".into(),
            password: "test-pass".into(),
            language: "en".into(),
        }),
        ..OnboardingState::default()
    })
    .await;

    let flow = server
        .post("/auth/login_flow")
        .json(&json!({
            "client_id": "https://example.com",
            "handler": ["homeassistant", null],
            "redirect_uri": "https://example.com/callback"
        }))
        .await
        .json::<serde_json::Value>();

    let invalid = server
        .post(format!("/auth/login_flow/{}", flow["flow_id"].as_str().unwrap()).as_str())
        .json(&json!({
            "client_id": "https://example.com",
            "username": "test-user",
            "password": "wrong-pass"
        }))
        .await;
    invalid.assert_status_ok();
    let invalid_json = invalid.json::<serde_json::Value>();
    assert_eq!(invalid_json["errors"]["base"], "invalid_auth");

    let success = server
        .post(format!("/auth/login_flow/{}", flow["flow_id"].as_str().unwrap()).as_str())
        .json(&json!({
            "client_id": "https://example.com",
            "username": "test-user",
            "password": "test-pass"
        }))
        .await;
    success.assert_status_ok();
    let success_json = success.json::<serde_json::Value>();
    assert_eq!(success_json["type"], "create_entry");
    assert!(success_json["result"].as_str().is_some());
}
