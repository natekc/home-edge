#![cfg(feature = "transport_wifi")]

mod support;

use axum::http::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn rest_get_services_lists_builtin_domains() {
    let server = support::test_server_with_onboarding(support::completed_onboarding()).await;

    let response = server.get("/api/services").await;

    response.assert_status_ok();
    let services = response.json::<Value>();
    let light_domain = services
        .as_array()
        .and_then(|domains| domains.iter().find(|domain| domain["domain"] == "light"))
        .expect("light domain present");
    assert_eq!(light_domain["services"]["turn_on"]["name"], "Turn on");
    assert_eq!(
        light_domain["services"]["turn_on"]["fields"]["brightness"]["required"],
        false
    );
}

#[tokio::test]
async fn rest_call_service_updates_entity_state() {
    let server = support::test_server_with_onboarding(support::completed_onboarding()).await;

    let response = server
        .post("/api/services/light/turn_on")
        .json(&json!({
            "entity_id": "light.kitchen",
            "brightness": 123
        }))
        .await;

    response.assert_status_ok();
    let changed_states = response.json::<Value>();
    assert_eq!(changed_states.as_array().expect("changed states").len(), 1);
    assert_eq!(changed_states[0]["entity_id"], "light.kitchen");
    assert_eq!(changed_states[0]["state"], "on");
    assert_eq!(changed_states[0]["attributes"]["brightness"], 123);

    let saved_state = server
        .get("/api/states/light.kitchen")
        .await
        .json::<Value>();
    assert_eq!(saved_state["state"], "on");
    assert_eq!(saved_state["attributes"]["brightness"], 123);
}

#[tokio::test]
async fn rest_call_service_unknown_service_returns_bad_request() {
    let server = support::test_server_with_onboarding(support::completed_onboarding()).await;

    let response = server.post("/api/services/light/make_coffee").await;

    response.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>(),
        json!({
            "message": "Service not found."
        })
    );
}

#[tokio::test]
async fn websocket_get_services_returns_builtin_map() {
    let (server, _) = support::test_ws_server_and_state(support::completed_onboarding()).await;
    let token = support::issue_access_token(&server).await;
    let mut ws = server
        .get_websocket("/api/websocket")
        .await
        .into_websocket()
        .await;

    let auth_required = ws.receive_json::<Value>().await;
    assert_eq!(auth_required["type"], "auth_required");

    ws.send_json(&json!({"type": "auth", "access_token": token}))
        .await;
    let auth_ok = ws.receive_json::<Value>().await;
    assert_eq!(auth_ok["type"], "auth_ok");

    ws.send_json(&json!({"id": 1, "type": "get_services"}))
        .await;
    let response = ws.receive_json::<Value>().await;

    assert_eq!(response["id"], 1);
    assert_eq!(response["type"], "result");
    assert_eq!(response["success"], true);
    assert_eq!(response["result"]["light"]["turn_on"]["name"], "Turn on");
    assert_eq!(
        response["result"]["switch"]["turn_off"]["description"],
        "Turn off switch entities."
    );
}

#[tokio::test]
async fn websocket_call_service_merges_target_and_returns_context() {
    let (server, state) = support::test_ws_server_and_state(support::completed_onboarding()).await;
    let token = support::issue_access_token(&server).await;
    let mut ws = server
        .get_websocket("/api/websocket")
        .await
        .into_websocket()
        .await;

    let _ = ws.receive_json::<Value>().await;
    ws.send_json(&json!({"type": "auth", "access_token": token}))
        .await;
    let _ = ws.receive_json::<Value>().await;

    ws.send_json(&json!({
        "id": 2,
        "type": "call_service",
        "domain": "light",
        "service": "turn_on",
        "target": {"entity_id": "light.den"},
        "service_data": {"brightness": 88}
    }))
    .await;

    let response = ws.receive_json::<Value>().await;
    assert_eq!(response["id"], 2);
    assert_eq!(response["type"], "result");
    assert_eq!(response["success"], true);
    assert!(response["result"]["context"]["id"].as_str().is_some());

    let saved_state = state.states.get("light.den").expect("updated state");
    assert_eq!(saved_state.state, "on");
    assert_eq!(saved_state.attributes.get("brightness"), Some(&json!(88)));
}

#[tokio::test]
async fn websocket_call_service_reports_not_found() {
    let (server, _) = support::test_ws_server_and_state(support::completed_onboarding()).await;
    let token = support::issue_access_token(&server).await;
    let mut ws = server
        .get_websocket("/api/websocket")
        .await
        .into_websocket()
        .await;

    let _ = ws.receive_json::<Value>().await;
    ws.send_json(&json!({"type": "auth", "access_token": token}))
        .await;
    let _ = ws.receive_json::<Value>().await;

    ws.send_json(&json!({
        "id": 3,
        "type": "call_service",
        "domain": "light",
        "service": "make_coffee",
        "service_data": {"entity_id": "light.den"}
    }))
    .await;

    let response = ws.receive_json::<Value>().await;
    assert_eq!(response["id"], 3);
    assert_eq!(response["type"], "result");
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "not_found");
    assert_eq!(response["error"]["translation_key"], "service_not_found");
    assert_eq!(
        response["error"]["translation_placeholders"]["domain"],
        "light"
    );
    assert_eq!(
        response["error"]["translation_placeholders"]["service"],
        "make_coffee"
    );
}

#[tokio::test]
async fn websocket_call_service_rejects_missing_target() {
    let (server, _) = support::test_ws_server_and_state(support::completed_onboarding()).await;
    let token = support::issue_access_token(&server).await;
    let mut ws = server
        .get_websocket("/api/websocket")
        .await
        .into_websocket()
        .await;

    let _ = ws.receive_json::<Value>().await;
    ws.send_json(&json!({"type": "auth", "access_token": token}))
        .await;
    let _ = ws.receive_json::<Value>().await;

    ws.send_json(&json!({
        "id": 4,
        "type": "call_service",
        "domain": "light",
        "service": "turn_on",
        "service_data": {"brightness": 64}
    }))
    .await;

    let response = ws.receive_json::<Value>().await;
    assert_eq!(response["id"], 4);
    assert_eq!(response["type"], "result");
    assert_eq!(response["success"], false);
    assert_eq!(response["error"]["code"], "invalid_format");
    assert_eq!(
        response["error"]["message"],
        "target must include entity_id"
    );
}
