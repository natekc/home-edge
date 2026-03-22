mod support;

use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn auth_providers_requires_completed_onboarding() {
    let server = support::test_server(false).await;

    let response = server.get("/auth/providers").await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let json = response.json::<serde_json::Value>();
    assert_eq!(
        json,
        json!({
            "message": "Onboarding not finished",
            "code": "onboarding_required"
        })
    );
}

#[tokio::test]
async fn auth_providers_returns_local_provider_after_onboarding() {
    let server = support::test_server(true).await;

    let response = server.get("/auth/providers").await;

    response.assert_status_ok();
    let json = response.json::<serde_json::Value>();
    assert_eq!(
        json,
        json!({
            "providers": [
                {
                    "name": "Home Assistant Local",
                    "id": null,
                    "type": "homeassistant"
                }
            ],
            "preselect_remember_me": true
        })
    );
}

#[tokio::test]
async fn login_flow_init_requires_completed_onboarding() {
    let server = support::test_server(false).await;

    let response = server
        .post("/auth/login_flow")
        .json(&json!({
            "client_id": "https://example.com",
            "handler": ["homeassistant", null],
            "redirect_uri": "https://example.com/callback"
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let json = response.json::<serde_json::Value>();
    assert_eq!(
        json,
        json!({
            "message": "Onboarding not finished",
            "code": "onboarding_required"
        })
    );
}

#[tokio::test]
async fn login_flow_init_returns_form_after_onboarding() {
    let server = support::test_server(true).await;

    let response = server
        .post("/auth/login_flow")
        .json(&json!({
            "client_id": "https://example.com",
            "handler": ["homeassistant", null],
            "redirect_uri": "https://example.com/callback"
        }))
        .await;

    response.assert_status_ok();
    let json = response.json::<serde_json::Value>();
    assert_eq!(json["type"], "form");
    assert_eq!(json["step_id"], "init");
    assert!(json["flow_id"].as_str().is_some());
    assert_eq!(json["errors"], json!({}));
}
