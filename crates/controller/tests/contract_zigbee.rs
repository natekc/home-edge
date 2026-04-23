//! End-to-end contract tests for the Zigbee integration.
//!
//! These tests drive the event fan-out loop with synthetic `ZigbeeEvent`
//! values — no serial port or real coordinator is needed.  The full HTTP
//! layer is exercised via `axum_test::TestServer` so each test covers:
//!
//!   mock ZigbeeEvent  →  run_event_loop  →  stores  →  HTTP API response
//!
//! Feature gate: only compiled when both `transport_wifi` and `zigbee` are
//! enabled.
#![cfg(all(feature = "transport_wifi", feature = "zigbee"))]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use home_edge::zigbee_device_store::ZigbeeDeviceStore;
use home_edge::zigbee_entity_store::ZigbeeEntityStore;
use home_edge::state_store::StateStore;
use home_edge::zigbee_integration::run_event_loop;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zigbee2mqtt_rs::{IeeeAddr, ZigbeeEvent};
use zigbee2mqtt_rs::devices::Device;
use zigbee2mqtt_rs::zigbee::{NwkAddr, EndpointDesc};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Build a minimal Device with the given input clusters on endpoint 1.
fn mock_device(ieee: IeeeAddr, friendly: &str, clusters: Vec<u16>) -> Device {
    let mut dev = Device::new(ieee, 0x1234 as NwkAddr);
    dev.friendly_name = friendly.to_string();
    dev.manufacturer = Some("Acme Corp".to_string());
    dev.model = Some("ACME-SENSOR-01".to_string());
    dev.power_source = Some("Battery".to_string());
    dev.interview_complete = true;
    dev.endpoints = vec![EndpointDesc {
        endpoint: 1,
        profile_id: 0x0104,
        device_id: 0x0100,
        input_clusters: clusters,
        output_clusters: vec![],
    }];
    dev
}

/// Send events, close the channel, then directly await run_event_loop until
/// all events are processed and the loop exits.  Completely deterministic.
async fn drive_events(
    events: impl IntoIterator<Item = ZigbeeEvent>,
    device_store: Arc<ZigbeeDeviceStore>,
    entity_store: Arc<ZigbeeEntityStore>,
    state_store: Arc<StateStore>,
) {
    let (event_tx, event_rx) = mpsc::channel::<ZigbeeEvent>(64);
    for event in events {
        event_tx.send(event).await.expect("send event");
    }
    drop(event_tx); // closing the sender causes run_event_loop to exit cleanly
    run_event_loop(event_rx, device_store, entity_store, state_store).await;
}

// ---------------------------------------------------------------------------
// Test: DeviceJoined creates a minimal, not-yet-interviewed device record.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn device_joined_creates_minimal_record_visible_via_api() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;

    let ieee = IeeeAddr::from_hex("0x00158d0001234567").expect("valid ieee");
    drive_events(
        [ZigbeeEvent::DeviceJoined { ieee_addr: ieee, nwk_addr: 0xbeef }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    let resp = server.get("/api/zigbee/devices").await;
    resp.assert_status_ok();

    let body: Value = resp.json();
    let devices = body.as_array().expect("array");
    assert_eq!(devices.len(), 1);
    let d = &devices[0];
    assert_eq!(d["ieee_addr"], json!(ieee.as_hex()));
    assert_eq!(d["interview_complete"], json!(false));
}

// ---------------------------------------------------------------------------
// Test: DeviceInterviewComplete creates a full record + entities + state.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interview_complete_creates_entities_and_initial_state() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;

    // Temperature + humidity sensor (clusters 0x0402 + 0x0405 + 0x0001 battery).
    let ieee = IeeeAddr::from_hex("0x00158d0009abcdef").expect("valid ieee");
    let mut dev = mock_device(ieee, "living_room_sensor", vec![0x0402, 0x0405, 0x0001]);
    // Pre-populate a state value so push_state fires on interview.
    dev.state.insert("temperature".to_string(), json!(21.5));
    dev.state.insert("humidity".to_string(), json!(55.0));
    dev.state.insert("battery".to_string(), json!(87));

    drive_events(
        [ZigbeeEvent::DeviceInterviewComplete { ieee_addr: ieee, device: dev }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    // Device record should be marked interview_complete.
    let resp = server.get("/api/zigbee/devices").await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let devices = body.as_array().expect("array");
    assert_eq!(devices.len(), 1);
    let d = &devices[0];
    assert_eq!(d["interview_complete"], json!(true));
    assert_eq!(d["model"], json!("ACME-SENSOR-01"));

    // Entities derived from clusters should exist.
    let entities = state
        .zigbee_entities
        .list_for_device(&ieee.as_hex())
        .await
        .expect("list entities");
    let entity_ids: Vec<_> = entities.iter().map(|e| e.entity_id.as_str()).collect();
    assert!(entity_ids.contains(&"sensor.living_room_sensor_temperature"), "temperature entity");
    assert!(entity_ids.contains(&"sensor.living_room_sensor_humidity"), "humidity entity");
    assert!(entity_ids.contains(&"sensor.living_room_sensor_battery"), "battery entity");

    // Initial state should have been pushed to the state store.
    let temp_state = state.states.get("sensor.living_room_sensor_temperature");
    assert!(temp_state.is_some(), "temperature state should be set");
    assert_eq!(temp_state.unwrap().state, "21.5");

    let hum_state = state.states.get("sensor.living_room_sensor_humidity");
    assert_eq!(hum_state.unwrap().state, "55.0");
}

// ---------------------------------------------------------------------------
// Test: StateChanged updates entity state values (verified via REST API).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn state_changed_updates_entity_state() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;

    let ieee = IeeeAddr::from_hex("0x00158d0002222222").expect("valid ieee");
    // Light device: on/off (0x0006) + level (0x0008).
    let dev = mock_device(ieee, "kitchen_bulb", vec![0x0006, 0x0008]);

    // Phase 1: interview to register entities.
    drive_events(
        [ZigbeeEvent::DeviceInterviewComplete { ieee_addr: ieee, device: dev }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    // Confirm entity was registered as a light.
    let entities = state.zigbee_entities.list_for_device(&ieee.as_hex()).await.expect("list");
    assert!(
        entities.iter().any(|e| e.entity_id == "light.kitchen_bulb" && e.domain == "light"),
        "expected a light entity; got: {:?}",
        entities.iter().map(|e| &e.entity_id).collect::<Vec<_>>()
    );

    // Phase 2: turn on the light with brightness.
    let mut new_state = serde_json::Map::new();
    new_state.insert("state".to_string(), json!("ON"));
    new_state.insert("brightness".to_string(), json!(128));
    drive_events(
        [ZigbeeEvent::StateChanged { ieee_addr: ieee, state: new_state }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    // State store entry should reflect "on".
    let ha_state = state.states.get("light.kitchen_bulb").expect("state present");
    assert_eq!(ha_state.state, "on", "light should be on");
    assert_eq!(ha_state.attributes.get("brightness"), Some(&json!(128)));

    // Verify the state is also visible via the HA REST API (no auth needed, LAN endpoint).
    let resp = server.get("/api/states/light.kitchen_bulb").await;
    resp.assert_status_ok();
    let api_state: Value = resp.json();
    assert_eq!(api_state["state"], json!("on"));
    assert_eq!(api_state["attributes"]["brightness"], json!(128));
}

// ---------------------------------------------------------------------------
// Test: DeviceLeft marks all entities unavailable.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn device_left_marks_entities_unavailable() {
    let (_, state) = support::test_server_and_state(support::completed_onboarding()).await;

    let ieee = IeeeAddr::from_hex("0x00158d0003333333").expect("valid ieee");

    // Phase 1: interview with an initial temperature reading.
    let mut dev = mock_device(ieee, "bedroom_sensor", vec![0x0402]);
    dev.state.insert("temperature".to_string(), json!(20.0));
    drive_events(
        [ZigbeeEvent::DeviceInterviewComplete { ieee_addr: ieee, device: dev }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    assert_eq!(
        state.states.get("sensor.bedroom_sensor_temperature").expect("state set after interview").state,
        "20.0"
    );

    // Phase 2: device leaves.
    drive_events(
        [ZigbeeEvent::DeviceLeft { ieee_addr: ieee }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    // Entity state should now be "unavailable" (device record preserved).
    let ha_state = state.states.get("sensor.bedroom_sensor_temperature").expect("state preserved");
    assert_eq!(ha_state.state, "unavailable");
}

// ---------------------------------------------------------------------------
// Test: HTTP DELETE device removes records and state entries.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_device_removes_records_and_state() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;

    let ieee = IeeeAddr::from_hex("0x00158d0004444444").expect("valid ieee");
    let mut dev = mock_device(ieee, "hall_switch", vec![0x0006]);
    dev.state.insert("state".to_string(), json!("ON"));

    drive_events(
        [ZigbeeEvent::DeviceInterviewComplete { ieee_addr: ieee, device: dev }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    // Verify device and state exist.
    assert!(state.states.get("switch.hall_switch").is_some(), "initial state present");

    // DELETE via HTTP (open endpoint, no auth token needed).
    let resp = server
        .delete(&format!("/api/zigbee/devices/{}", ieee.as_hex()))
        .await;
    resp.assert_status(StatusCode::NO_CONTENT);

    // Device record should be empty.
    let devices = state.zigbee_devices.list().await.expect("list devices");
    assert!(devices.is_empty(), "device registry should be empty after delete");

    // Entity records should be gone.
    let entities = state.zigbee_entities.list_for_device(&ieee.as_hex()).await.expect("list entities");
    assert!(entities.is_empty(), "entity store should be empty after delete");

    // State entry should have been cleared.
    assert!(
        state.states.get("switch.hall_switch").is_none(),
        "state should be cleared after delete"
    );
}

// ---------------------------------------------------------------------------
// Test: PATCH device updates name_by_user and returns 204.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_device_updates_name_by_user() {
    let (server, state) = support::test_server_and_state(support::completed_onboarding()).await;

    let ieee = IeeeAddr::from_hex("0x00158d0005555555").expect("valid ieee");
    let dev = mock_device(ieee, "plug_0001", vec![0x0006]);

    drive_events(
        [ZigbeeEvent::DeviceInterviewComplete { ieee_addr: ieee, device: dev }],
        Arc::clone(&state.zigbee_devices),
        Arc::clone(&state.zigbee_entities),
        Arc::clone(&state.states),
    ).await;

    // Rename via PATCH (handler returns 204 No Content on success).
    let resp = server
        .patch(&format!("/api/zigbee/devices/{}", ieee.as_hex()))
        .json(&json!({"name_by_user": "Living Room Plug"}))
        .await;
    resp.assert_status(StatusCode::NO_CONTENT);

    let record = state
        .zigbee_devices
        .get_by_ieee(&ieee.as_hex())
        .await
        .expect("get record")
        .expect("record present");
    assert_eq!(record.name_by_user.as_deref(), Some("Living Room Plug"));
}

// ---------------------------------------------------------------------------
// Test: permit_join returns 503 when the bridge is not configured.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn permit_join_returns_503_when_bridge_not_configured() {
    // Default test server has zigbee: None (no bridge running).
    let (server, _state) = support::test_server_and_state(support::completed_onboarding()).await;

    let resp = server
        .post("/api/zigbee/permit_join")
        .json(&json!({"duration": 60}))
        .await;
    resp.assert_status(StatusCode::SERVICE_UNAVAILABLE);
}
