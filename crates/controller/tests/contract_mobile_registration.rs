#![cfg(feature = "transport_wifi")]

mod support;

use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn mobile_registration_persists_device_record() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;

    let response = server
        .post("/api/mobile_app/registrations")
        .json(&json!({
            "app_id": "io.homeassistant.ios",
            "app_name": "Home Assistant",
            "app_version": "2024.1",
            "device_name": "My iPhone",
            "manufacturer": "Apple",
            "model": "iPhone 15",
            "device_id": "device-123",
            "os_name": "iOS",
            "os_version": "17.0",
            "supports_encryption": true
        }))
        .await;

    response.assert_status(StatusCode::CREATED);
    let json = response.json::<serde_json::Value>();
    let devices = state.mobile_devices.all().await.expect("load devices");

    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].webhook_id, json["webhook_id"].as_str().unwrap());
    assert_eq!(devices[0].device_id.as_deref(), Some("device-123"));
    assert_eq!(devices[0].owner_username.as_deref(), Some("test-user"));
}

#[tokio::test]
async fn mobile_registration_reuses_existing_device_identity() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;
    let payload = json!({
        "app_id": "io.homeassistant.ios",
        "app_name": "Home Assistant",
        "app_version": "2024.1",
        "device_name": "My iPhone",
        "manufacturer": "Apple",
        "model": "iPhone 15",
        "device_id": "device-123",
        "os_name": "iOS",
        "os_version": "17.0",
        "supports_encryption": true
    });

    let first = server
        .post("/api/mobile_app/registrations")
        .json(&payload)
        .await
        .json::<serde_json::Value>();
    let second = server
        .post("/api/mobile_app/registrations")
        .json(&payload)
        .await
        .json::<serde_json::Value>();

    assert_eq!(first["webhook_id"], second["webhook_id"]);
    assert_eq!(
        state
            .mobile_devices
            .all()
            .await
            .expect("load devices")
            .len(),
        1
    );
}
