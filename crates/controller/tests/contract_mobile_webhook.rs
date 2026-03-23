mod support;

use axum::http::StatusCode;
use serde_json::json;

async fn register_mobile_device(server: &axum_test::TestServer) -> serde_json::Value {
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
            "supports_encryption": false
        }))
        .await;

    response.assert_status(StatusCode::CREATED);
    response.json::<serde_json::Value>()
}

#[tokio::test]
async fn register_sensor_creates_mobile_entity_and_visible_state() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;
    let registration = register_mobile_device(&server).await;
    let webhook_id = registration["webhook_id"].as_str().expect("webhook id");

    let response = server
        .post(&format!("/api/webhook/{webhook_id}"))
        .json(&json!({
            "type": "register_sensor",
            "data": {
                "type": "sensor",
                "unique_id": "battery_level",
                "name": "Battery Level",
                "state": 98,
                "unit_of_measurement": "%",
                "device_class": "battery",
                "state_class": "measurement",
                "icon": "mdi:battery",
                "attributes": {
                    "source": "mobile_app"
                }
            }
        }))
        .await;

    response.assert_status(StatusCode::CREATED);
    assert_eq!(
        response.json::<serde_json::Value>(),
        json!({"success": true})
    );

    let entities = state.mobile_entities.all().await.expect("load entities");
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].sensor_unique_id, "battery_level");

    let state_response = server
        .get(&format!("/api/states/{}", entities[0].entity_id))
        .await;
    state_response.assert_status_ok();
    let state_json = state_response.json::<serde_json::Value>();
    assert_eq!(state_json["state"], json!("98"));
    assert_eq!(state_json["attributes"]["unit_of_measurement"], json!("%"));
    assert_eq!(state_json["attributes"]["device_class"], json!("battery"));
    assert_eq!(state_json["attributes"]["source"], json!("mobile_app"));
}

#[tokio::test]
async fn update_sensor_states_updates_registered_sensor() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;
    let registration = register_mobile_device(&server).await;
    let webhook_id = registration["webhook_id"].as_str().expect("webhook id");

    server
        .post(&format!("/api/webhook/{webhook_id}"))
        .json(&json!({
            "type": "register_sensor",
            "data": {
                "type": "sensor",
                "unique_id": "battery_level",
                "name": "Battery Level",
                "state": 98
            }
        }))
        .await
        .assert_status(StatusCode::CREATED);

    let response = server
        .post(&format!("/api/webhook/{webhook_id}"))
        .json(&json!({
            "type": "update_sensor_states",
            "data": [
                {
                    "type": "sensor",
                    "unique_id": "battery_level",
                    "state": 87,
                    "attributes": {
                        "charging": true
                    }
                }
            ]
        }))
        .await;

    response.assert_status_ok();
    assert_eq!(
        response.json::<serde_json::Value>(),
        json!({
            "battery_level": {
                "success": true
            }
        })
    );

    let entity_id = state.mobile_entities.all().await.expect("load entities")[0]
        .entity_id
        .clone();
    let state_response = server.get(&format!("/api/states/{entity_id}")).await;
    state_response.assert_status_ok();
    let state_json = state_response.json::<serde_json::Value>();
    assert_eq!(state_json["state"], json!("87"));
    assert_eq!(state_json["attributes"]["charging"], json!(true));
}

#[tokio::test]
async fn update_sensor_states_reports_unregistered_sensor() {
    let (server, _state) = support::test_server_and_state(support::completed_onboarding()).await;
    let registration = register_mobile_device(&server).await;
    let webhook_id = registration["webhook_id"].as_str().expect("webhook id");

    let response = server
        .post(&format!("/api/webhook/{webhook_id}"))
        .json(&json!({
            "type": "update_sensor_states",
            "data": [
                {
                    "type": "sensor",
                    "unique_id": "battery_level",
                    "state": 87
                }
            ]
        }))
        .await;

    response.assert_status_ok();
    assert_eq!(
        response.json::<serde_json::Value>(),
        json!({
            "battery_level": {
                "success": false,
                "error": {
                    "code": "not_registered",
                    "message": "sensor battery_level is not registered"
                }
            }
        })
    );
}
