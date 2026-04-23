//! HA-compatible webhook and MQTT discovery endpoints.
//!
//! ## Webhook endpoint
//!
//!   GET|POST|PUT|HEAD /api/webhook/{webhook_id}
//!     → homeassistant/components/webhook/__init__.py  WebhookView
//!
//! The webhook endpoint:
//!   - Requires NO authentication (requires_auth = False)
//!   - Accepts GET, HEAD, POST, PUT
//!   - Returns HTTP 200 for all requests (even unknown webhook IDs — HA logs a
//!     warning but still returns 200 to avoid leaking information about which
//!     webhook IDs are registered)
//!   - Delivers the payload to any registered handler
//!
//! Source constants:
//!   homeassistant/components/webhook/__init__.py
//!     URL_WEBHOOK_PATH = "/api/webhook/{webhook_id}"
//!
//! ## MQTT discovery info endpoint
//!
//!   GET /api/mqtt/discovery
//!     (non-standard helper endpoint for devices to query the discovery topic)
//!
//! The standard MQTT discovery topic pattern (source: mqtt/discovery.py):
//!   {discovery_prefix}/{component}/{node_id}/{object_id}/config
//! where discovery_prefix defaults to "homeassistant".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::app::AppState;
use crate::mobile_device_store::MobileDeviceRecord;
use crate::mobile_entity_store::{MobileEntityRecord, MobileEntityRegistration};
use crate::state_store::{StateAttributes, make_state};

// ---------------------------------------------------------------------------
// Webhook store — registered handlers keyed by webhook_id
// ---------------------------------------------------------------------------

/// In-memory webhook registry.
pub struct WebhookStore {
    inner: RwLock<HashSet<String>>,
    /// Payloads received, keyed by webhook_id (for testability).
    pub received: RwLock<HashMap<String, Vec<Value>>>,
}

impl WebhookStore {
    pub fn new() -> Self {
        WebhookStore {
            inner: RwLock::new(HashSet::new()),
            received: RwLock::new(HashMap::new()),
        }
    }

    pub async fn remember(&self, webhook_id: String) {
        let mut inner = self.inner.write().await;
        inner.insert(webhook_id);
    }

    /// Register a webhook.
    #[cfg(test)]
    pub async fn register(&self, webhook_id: String, _domain: String, _name: String) {
        self.remember(webhook_id).await;
    }

    /// Returns true if the webhook_id is registered.
    pub async fn is_registered(&self, webhook_id: &str) -> bool {
        let inner = self.inner.read().await;
        inner.contains(webhook_id)
    }

    /// Store a received payload (for testing / forwarding).
    pub async fn store_payload(&self, webhook_id: &str, payload: Value) {
        let mut received = self.received.write().await;
        received
            .entry(webhook_id.to_string())
            .or_default()
            .push(payload);
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Webhook endpoint — GET, POST, PUT, HEAD all use the same handler
        // Source: webhook/__init__.py  WebhookView  get=_handle, post=_handle, put=_handle
        .route(
            "/api/webhook/{webhook_id}",
            get(webhook_handle)
                .post(webhook_handle)
                .put(webhook_handle)
                .head(webhook_handle),
        )
        // MQTT discovery info endpoint (non-standard helper)
        .route("/api/mqtt/discovery", get(mqtt_discovery_info))
}

// ---------------------------------------------------------------------------
// Webhook handler
// ---------------------------------------------------------------------------

/// Handle any HTTP method to `/api/webhook/{webhook_id}`.
///
/// Source: webhook/__init__.py  WebhookView._handle / async_handle_webhook
///
/// Key protocol detail (source: async_handle_webhook lines 138-145):
///   if (webhook := handlers.get(webhook_id)) is None:
///       _LOGGER.warning("Received message for unregistered webhook %s")
///       # Falls through and returns HTTP 200 anyway
///   ...
///   response = None → Response(status=HTTPStatus.OK)
///
/// This means:
///   - Unknown webhook_id → HTTP 200 (no body leaked)
///   - No authentication required (requires_auth = False)
///   - CORS is allowed (cors_allowed = True)
async fn webhook_handle(
    State(state): State<Arc<AppState>>,
    Path(webhook_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let is_json = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("application/json"))
        .unwrap_or(false);

    let mobile_device = match state.mobile_devices.get_by_webhook_id(&webhook_id).await {
        Ok(device) => device,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load mobile device: {err:#}")})),
            )
                .into_response();
        }
    };
    let webhook_registered = state.webhooks.is_registered(&webhook_id).await;

    if mobile_device.is_none() && !webhook_registered {
        return (StatusCode::OK, Json(json!({}))).into_response();
    }

    let payload = if is_json {
        match serde_json::from_slice::<Value>(&body) {
            Ok(payload) => payload,
            Err(_) if mobile_device.is_some() => {
                return (StatusCode::BAD_REQUEST, Json(json!({}))).into_response();
            }
            Err(_) => Value::Null,
        }
    } else {
        Value::Null
    };

    if let Some(device) = mobile_device.as_ref() {
        state.webhooks.remember(webhook_id.clone()).await;
        let response = handle_mobile_webhook(&state, device, &payload).await;
        state.webhooks.store_payload(&webhook_id, payload).await;
        return response;
    }

    // Store the received payload for registered webhooks.
    // Source: webhook handler records are dispatched per webhook_id.
    // Even for unknown IDs we return 200 — HA behavior.
    if webhook_registered {
        state.webhooks.store_payload(&webhook_id, payload).await;
    }

    // Source: async_handle_webhook — always HTTP 200, no body for base handler
    // "if response is None: response = Response(status=HTTPStatus.OK)"
    (StatusCode::OK, Json(json!({}))).into_response()
}

// ---------------------------------------------------------------------------
// MQTT discovery info
// ---------------------------------------------------------------------------

/// MQTT discovery configuration reported by this device.
///
/// This is a non-standard helper endpoint that lets MQTT-capable clients
/// know what topic prefix to use for auto-discovery.
///
/// Source: mqtt/discovery.py  DISCOVERY_TOPIC pattern:
///   f"{discovery_topic}/{component}/{object_id}/config"
/// Default discovery_prefix: "homeassistant"
/// (source: mqtt/const.py  CONF_DISCOVERY_PREFIX, default in config schema)
#[derive(Serialize)]
struct MqttDiscoveryInfo {
    /// The MQTT discovery topic prefix.
    /// Source: homeassistant/components/mqtt/__init__.py  discovery_prefix default
    discovery_prefix: String,
    /// Full topic pattern for a sensor:
    ///   {discovery_prefix}/sensor/{node_id}/{object_id}/config
    topic_pattern: String,
}

async fn mqtt_discovery_info(State(_state): State<Arc<AppState>>) -> Response {
    let info = MqttDiscoveryInfo {
        discovery_prefix: "homeassistant".into(),
        topic_pattern: "homeassistant/{component}/{node_id}/{object_id}/config".into(),
    };
    (StatusCode::OK, Json(info)).into_response()
}

async fn handle_mobile_webhook(
    state: &Arc<AppState>,
    device: &MobileDeviceRecord,
    payload: &Value,
) -> Response {
    let Some(object) = payload.as_object() else {
        return (StatusCode::OK, Json(json!({}))).into_response();
    };

    let Some(webhook_type) = object.get("type").and_then(Value::as_str) else {
        return (StatusCode::OK, Json(json!({}))).into_response();
    };

    match webhook_type {
        "register_sensor" => {
            let data = object.get("data").cloned().unwrap_or_else(|| json!({}));
            register_sensor_command(state, device, &data).await
        }
        "update_sensor_states" => {
            let data = object.get("data").cloned().unwrap_or_else(|| json!([]));
            update_sensor_states_command(state, device, &data).await
        }
        // Source: homeassistant/components/mobile_app/webhook.py  handle_webhook_get_config
        // Returns device and app configuration back to the companion app.
        "get_config" => get_config_command(state, device).await,
        // Source: homeassistant/components/mobile_app/webhook.py  handle_webhook_get_zones
        // Returns zone.home (synthetic, from OnboardingState) + all user-defined zones.
        // Source: homeassistant/components/zone/__init__.py  get_zones returns all zone
        //         entities including the synthetic zone.home entity.
        "get_zones" => {
            let (onboarding, user_zones) = match tokio::try_join!(
                state.storage.load_onboarding(),
                state.zone_store.list(),
            ) {
                Ok(pair) => pair,
                Err(_) => return (StatusCode::OK, Json(json!([]))).into_response(),
            };
            let mut zones: Vec<serde_json::Value> =
                vec![crate::zone_store::home_zone_state(&onboarding)];
            // Only include user zones that have coordinates — HAKit's Zone model
            // decodes latitude/longitude as non-optional Double, so null coordinates
            // would cause decoding failures in the iOS companion app.
            zones.extend(
                user_zones.iter()
                    .filter(|z| z.latitude.is_some() && z.longitude.is_some())
                    .map(crate::zone_store::zone_to_state)
            );
            (StatusCode::OK, Json(json!(zones))).into_response()
        }
        // Source: homeassistant/components/mobile_app/webhook.py  handle_webhook_update_location
        // Accepts a device location update. Home-edge acknowledges but does not store.
        "update_location" => {
            (StatusCode::OK, Json(json!({}))).into_response()
        }
        // Source: homeassistant/components/mobile_app/webhook.py  handle_webhook_fire_event
        // Fires a Home Assistant event. Home-edge acknowledges but does not dispatch events yet.
        "fire_event" => {
            (StatusCode::OK, Json(json!({}))).into_response()
        }
        _ => (StatusCode::OK, Json(json!({}))).into_response(),
    }
}

#[derive(Clone)]
struct SensorCommand {
    entity_type: String,
    unique_id: String,
    name: String,
    state: Option<Value>,
    attributes: serde_json::Map<String, Value>,
    device_class: Option<String>,
    unit_of_measurement: Option<String>,
    icon: Option<String>,
    entity_category: Option<String>,
    state_class: Option<String>,
    disabled: bool,
}

/// Handle `type: "get_config"` webhook command.
///
/// Source: homeassistant/components/mobile_app/webhook.py  handle_webhook_get_config
/// Returns the registration details and current app config back to the companion app.
/// The companion uses this to sync webhook_id and server capabilities on reconnect.
async fn get_config_command(state: &Arc<AppState>, device: &MobileDeviceRecord) -> Response {
    let config = match state
        .core
        .execute(
            crate::core::CoreDeps {
                config: &state.config,
                states: &state.states,
                services: &state.services,
            },
            crate::core::OperationRequest::GetConfigSummary,
        ) {
        crate::core::OperationResult::ConfigSummary(cfg) => cfg,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Source: handle_webhook_get_config return value (mobile_app/webhook.py):
    //   {
    //     CONF_CLOUDHOOK_URL: entry.data.get(CONF_CLOUDHOOK_URL),
    //     CONF_REMOTE_UI_URL: entry.data.get(CONF_REMOTE_UI_URL),
    //     CONF_SECRET: entry.data.get(CONF_SECRET),
    //     CONF_WEBHOOK_ID: entry.data[CONF_WEBHOOK_ID],
    //     "entities": [],
    //     "user_id": user.id,
    //   }
    (
        StatusCode::OK,
        Json(json!({
            "cloudhook_url": null,
            "remote_ui_url": null,
            "secret": device.secret,
            "webhook_id": device.webhook_id,
            "location_name": config.location_name,
            "entities": [],
            "user_id": null,
        })),
    )
        .into_response()
}

async fn register_sensor_command(
    state: &Arc<AppState>,
    device: &MobileDeviceRecord,
    data: &Value,
) -> Response {
    let Some(command) = parse_sensor_command(data, true) else {
        return (StatusCode::OK, Json(json!({}))).into_response();
    };

    let record = match state
        .mobile_entities
        .register(MobileEntityRegistration {
            webhook_id: device.webhook_id.clone(),
            entity_type: command.entity_type.clone(),
            sensor_unique_id: command.unique_id.clone(),
            sensor_name: command.name.clone(),
            device_class: command.device_class.clone(),
            unit_of_measurement: command.unit_of_measurement.clone(),
            icon: command.icon.clone(),
            entity_category: command.entity_category.clone(),
            state_class: command.state_class.clone(),
            disabled: command.disabled,
        })
        .await
    {
        Ok(record) => record,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to persist mobile entity: {err:#}")})),
            )
                .into_response();
        }
    };

    if let Err(err) = apply_sensor_state(state, &record, &command) {
        return (StatusCode::BAD_REQUEST, Json(json!({"message": err}))).into_response();
    }

    // Record numeric value to history for sparklines / history API.
    // Source: homeassistant/components/sensor/__init__.py _numeric_state_expected()
    // A sensor is numeric if state_class is set OR unit_of_measurement is set.
    if is_numeric_sensor(&record) {
        if let Some(st) = state.states.get(&record.entity_id) {
            if let Ok(v) = st.state.parse::<f64>() {
                state.history.record(&record.entity_id, v).await;
            }
        }
    }

    (StatusCode::CREATED, Json(json!({"success": true}))).into_response()
}

async fn update_sensor_states_command(
    state: &Arc<AppState>,
    device: &MobileDeviceRecord,
    data: &Value,
) -> Response {
    let Some(items) = data.as_array() else {
        return (StatusCode::OK, Json(json!({}))).into_response();
    };

    if items.iter().any(|item| {
        item.get("type").and_then(Value::as_str).is_none()
            || item.get("unique_id").and_then(Value::as_str).is_none()
    }) {
        return (StatusCode::OK, Json(json!({}))).into_response();
    }

    let mut response = serde_json::Map::new();

    for item in items {
        let Some(unique_id) = item.get("unique_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(entity_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };

        let record = match state
            .mobile_entities
            .get(&device.webhook_id, entity_type, unique_id)
            .await
        {
            Ok(record) => record,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"message": format!("failed to load mobile entity: {err:#}")})),
                )
                    .into_response();
            }
        };

        let Some(record) = record else {
            response.insert(
                unique_id.to_string(),
                json!({
                    "success": false,
                    "error": {
                        "code": "not_registered",
                        "message": format!("{entity_type} {unique_id} is not registered")
                    }
                }),
            );
            continue;
        };

        let Some(command) = parse_sensor_command(item, false) else {
            response.insert(
                unique_id.to_string(),
                json!({
                    "success": false,
                    "error": {
                        "code": "invalid_format",
                        "message": format!("invalid sensor payload for {entity_type} {unique_id}")
                    }
                }),
            );
            continue;
        };

        if let Err(err) = apply_sensor_state(state, &record, &command) {
            response.insert(
                unique_id.to_string(),
                json!({
                    "success": false,
                    "error": {
                        "code": "invalid_format",
                        "message": err
                    }
                }),
            );
            continue;
        }

        // Record numeric value to history for sparklines / history API.
        // Source: homeassistant/components/sensor/__init__.py _numeric_state_expected()
        // A sensor is numeric if state_class is set OR unit_of_measurement is set.
        if is_numeric_sensor(&record) {
            if let Some(st) = state.states.get(&record.entity_id) {
                if let Ok(v) = st.state.parse::<f64>() {
                    state.history.record(&record.entity_id, v).await;
                }
            }
        }

        let mut result = json!({"success": true});
        if record.disabled {
            result["is_disabled"] = Value::Bool(true);
        }
        response.insert(unique_id.to_string(), result);
    }

    (StatusCode::OK, Json(Value::Object(response))).into_response()
}

/// Returns true if the sensor should have numeric history tracked.
///
/// Source: homeassistant/components/sensor/__init__.py _numeric_state_expected()
///   A sensor is numeric when state_class OR unit_of_measurement (native) is set.
///   Binary sensors and category sensors without these attributes are not numeric.
fn is_numeric_sensor(record: &MobileEntityRecord) -> bool {
    record.state_class.is_some() || record.unit_of_measurement.is_some()
}

fn parse_sensor_command(value: &Value, require_name: bool) -> Option<SensorCommand> {
    let object = value.as_object()?;
    let entity_type = object.get("type")?.as_str()?.to_string();
    if !matches!(entity_type.as_str(), "sensor" | "binary_sensor") {
        return None;
    }

    let unique_id = object.get("unique_id")?.as_str()?.to_string();
    if unique_id.is_empty() {
        return None;
    }

    let name = match object.get("name").and_then(Value::as_str) {
        Some(name) if !name.is_empty() => name.to_string(),
        Some(_) => return None,
        None if require_name => return None,
        None => unique_id.clone(),
    };

    let state_class = nullable_string(object.get("state_class"))?;
    if entity_type != "sensor" && state_class.is_some() {
        return None;
    }

    let attributes = object
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    Some(SensorCommand {
        entity_type,
        unique_id,
        name,
        state: object.get("state").cloned(),
        attributes,
        device_class: nullable_string(object.get("device_class"))?,
        unit_of_measurement: nullable_string(object.get("unit_of_measurement"))?,
        icon: match object.get("icon") {
            Some(Value::Null) => Some("mdi:cellphone".to_string()),
            Some(Value::String(icon)) => Some(icon.clone()),
            Some(_) => return None,
            None => Some("mdi:cellphone".to_string()),
        },
        entity_category: nullable_string(object.get("entity_category"))?,
        state_class,
        disabled: object
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn nullable_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(value)) => Some(Some(value.clone())),
        Some(_) => None,
    }
}

fn apply_sensor_state(
    state: &Arc<AppState>,
    record: &MobileEntityRecord,
    command: &SensorCommand,
) -> Result<(), String> {
    let mut attributes: HashMap<String, Value> = command.attributes.clone().into_iter().collect();
    attributes.insert("friendly_name".into(), Value::String(command.name.clone()));
    if let Some(device_class) = command
        .device_class
        .clone()
        .or_else(|| record.device_class.clone())
    {
        attributes.insert("device_class".into(), Value::String(device_class));
    }
    if let Some(unit) = command
        .unit_of_measurement
        .clone()
        .or_else(|| record.unit_of_measurement.clone())
    {
        attributes.insert("unit_of_measurement".into(), Value::String(unit));
    }
    if let Some(icon) = command.icon.clone().or_else(|| record.icon.clone()) {
        attributes.insert("icon".into(), Value::String(icon));
    }
    if let Some(category) = command
        .entity_category
        .clone()
        .or_else(|| record.entity_category.clone())
    {
        attributes.insert("entity_category".into(), Value::String(category));
    }
    if let Some(state_class) = command
        .state_class
        .clone()
        .or_else(|| record.state_class.clone())
    {
        attributes.insert("state_class".into(), Value::String(state_class));
    }

    let state_value = match (
        &record.entity_type[..],
        command.state.clone().unwrap_or(Value::Null),
    ) {
        ("binary_sensor", Value::Bool(value)) => {
            if value {
                "on".to_string()
            } else {
                "off".to_string()
            }
        }
        (_, Value::Null) => "unknown".to_string(),
        (_, Value::Bool(value)) => value.to_string(),
        (_, Value::Number(value)) => value.to_string(),
        (_, Value::String(value)) => value,
        (_, other) => return Err(format!("unsupported sensor state payload: {other}")),
    };

    state
        .states
        .set(make_state(&record.entity_id, state_value, StateAttributes::from_hash(attributes)))
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use serde_json::Value;

    fn make_server() -> TestServer {
        use std::net::{IpAddr, Ipv4Addr};
        use std::path::PathBuf;
        use std::sync::Arc;

        use crate::app::AppState;
        use crate::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
        use crate::storage::Storage;

        let config = AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                log_level: tracing::Level::INFO,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/ha-webhook-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
            areas: crate::config::AreasConfig::default(),
            home_zone: crate::config::HomeZoneConfig::default(),
            history: crate::config::HistoryConfig::default(),
            mdns: Default::default(),
            zigbee: None,
        };
        let storage = Storage::new_in_memory();
        let state = Arc::new(AppState::new(config, storage));
        let app = super::router().with_state(state);
        TestServer::new(app).unwrap()
    }

    // -----------------------------------------------------------------------
    // POST /api/webhook/{webhook_id}
    // -----------------------------------------------------------------------

    /// Unknown webhook_id sends HTTP 200 (not 404).
    ///
    /// Source: async_handle_webhook — logs warning but returns 200
    ///   "# Falls through — response = Response(status=HTTPStatus.OK)"
    #[tokio::test]
    async fn post_webhook_unknown_id_returns_200() {
        let server = make_server();
        let resp = server
            .post("/api/webhook/unknown-webhook-id")
            .json(&serde_json::json!({"action": "call_service"}))
            .await;
        resp.assert_status_ok();
    }

    /// GET also returns 200 (WebhookView.get = _handle).
    ///
    /// Source: webhook/__init__.py  WebhookView  get = _handle
    #[tokio::test]
    async fn get_webhook_returns_200() {
        let server = make_server();
        let resp = server.get("/api/webhook/some-webhook-id").await;
        resp.assert_status_ok();
    }

    /// PUT also returns 200 (WebhookView.put = _handle).
    ///
    /// Source: webhook/__init__.py  WebhookView  put = _handle
    #[tokio::test]
    async fn put_webhook_returns_200() {
        let server = make_server();
        let resp = server
            .put("/api/webhook/some-webhook-id")
            .json(&serde_json::json!({}))
            .await;
        resp.assert_status_ok();
    }

    /// Webhook requires NO authentication.
    ///
    /// Source: WebhookView  requires_auth = False
    #[tokio::test]
    async fn webhook_does_not_require_auth() {
        let server = make_server();
        // No Authorization header — must still return 200, not 401
        let resp = server
            .post("/api/webhook/any-id")
            .json(&serde_json::json!({"test": true}))
            .await;
        resp.assert_status(StatusCode::OK);
    }

    /// Payload is stored for registered webhooks.
    #[tokio::test]
    async fn webhook_stores_payload_for_registered_id() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::path::PathBuf;
        use std::sync::Arc;

        use crate::app::AppState;
        use crate::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
        use crate::ha_webhook::WebhookStore;
        use crate::storage::Storage;

        let config = AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                log_level: tracing::Level::INFO,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp"),
            },
            ui: UiConfig {
                product_name: "T".into(),
            },
            areas: crate::config::AreasConfig::default(),
            home_zone: crate::config::HomeZoneConfig::default(),
            history: crate::config::HistoryConfig::default(),
            mdns: Default::default(),
            zigbee: None,
        };
        let webhooks = WebhookStore::new();
        let mut base = AppState::new(config, Storage::new_in_memory());
        base.webhooks = webhooks;
        let state = Arc::new(base);
        // Pre-register a webhook
        state
            .webhooks
            .register(
                "test-hook-123".to_string(),
                "mobile_app".to_string(),
                "My Phone".to_string(),
            )
            .await;

        let app = super::router().with_state(Arc::clone(&state));
        let server = TestServer::new(app).unwrap();

        server
            .post("/api/webhook/test-hook-123")
            .json(&serde_json::json!({"type": "fire_event", "data": {}}))
            .await;

        // Verify payload was stored
        let received = state.webhooks.received.read().await;
        let payloads = received
            .get("test-hook-123")
            .expect("should have stored payload");
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["type"], "fire_event");
    }

    // -----------------------------------------------------------------------
    // GET /api/mqtt/discovery
    // -----------------------------------------------------------------------

    /// GET /api/mqtt/discovery returns discovery_prefix and topic_pattern.
    ///
    /// Source: mqtt/discovery.py — default discovery_prefix is "homeassistant"
    #[tokio::test]
    async fn get_mqtt_discovery_returns_info() {
        let server = make_server();
        let resp = server.get("/api/mqtt/discovery").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert!(json.get("discovery_prefix").is_some());
        assert_eq!(json["discovery_prefix"], "homeassistant");
        assert!(json.get("topic_pattern").is_some());
    }

    /// MQTT discovery topic_pattern must include the discovery_prefix.
    ///
    /// Source: mqtt/discovery.py  topic pattern:
    ///   f"{discovery_topic}/{component}/{object_id}/config"
    #[tokio::test]
    async fn mqtt_discovery_topic_pattern_uses_prefix() {
        let server = make_server();
        let resp = server.get("/api/mqtt/discovery").await;
        let json: Value = resp.json();
        let prefix = json["discovery_prefix"].as_str().unwrap();
        let pattern = json["topic_pattern"].as_str().unwrap();
        assert!(
            pattern.starts_with(prefix),
            "topic_pattern must start with discord_prefix"
        );
    }

    // -----------------------------------------------------------------------
    // History recording — grounded in homeassistant/components/sensor/__init__.py
    // _numeric_state_expected(): numeric when state_class OR unit_of_measurement is set.
    // -----------------------------------------------------------------------

    /// Build a full-router TestServer and return it together with the shared AppState.
    fn make_full_server() -> (axum_test::TestServer, std::sync::Arc<crate::app::AppState>) {
        use std::net::{IpAddr, Ipv4Addr};
        use std::path::PathBuf;
        use std::sync::Arc;

        use crate::app::AppState;
        use crate::config::{AppConfig, HistoryConfig, ServerConfig, StorageConfig, UiConfig};
        use crate::http;
        use crate::storage::Storage;

        let config = AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                log_level: tracing::Level::INFO,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/ha-webhook-history-test"),
            },
            ui: UiConfig { product_name: "Test".into() },
            areas: crate::config::AreasConfig::default(),
            home_zone: crate::config::HomeZoneConfig::default(),
            // Small capacity to keep tests fast
            history: HistoryConfig { capacity: 50 },
            mdns: Default::default(),
            zigbee: None,
        };
        let storage = Storage::new_in_memory();
        let state = Arc::new(AppState::new(config, storage));
        let app = http::router(Arc::clone(&state));
        let server = axum_test::TestServer::new(app).unwrap();
        (server, state)
    }

    /// Helper: register a device and return its webhook_id.
    async fn register_device(server: &axum_test::TestServer) -> String {
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&serde_json::json!({
                "app_id": "io.homeassistant.companion.test",
                "app_name": "HA Test",
                "app_version": "1.0",
                "device_id": "test-device-001",
                "device_name": "Test Phone",
                "manufacturer": "Test",
                "model": "Model X",
                "os_name": "TestOS",
                "os_version": "1.0",
                "supports_encryption": false
            }))
            .await;
        resp.assert_status(StatusCode::CREATED);
        resp.json::<Value>()["webhook_id"]
            .as_str()
            .expect("webhook_id in registration response")
            .to_string()
    }

    /// Sensor with state_class=measurement and a numeric state IS recorded.
    ///
    /// Source: homeassistant/components/sensor/__init__.py _numeric_state_expected()
    ///   state_class is not None → numeric
    #[tokio::test]
    async fn numeric_sensor_with_state_class_is_recorded_in_history() {
        let (server, state) = make_full_server();
        let webhook_id = register_device(&server).await;

        // Register sensor with state_class=measurement
        server
            .post(&format!("/api/webhook/{webhook_id}"))
            .json(&serde_json::json!({
                "type": "register_sensor",
                "data": {
                    "type": "sensor",
                    "unique_id": "battery_level",
                    "name": "Battery",
                    "state": "85",
                    "state_class": "measurement",
                    "unit_of_measurement": "%"
                }
            }))
            .await
            .assert_status(StatusCode::CREATED);

        // Update the state to confirm recording
        server
            .post(&format!("/api/webhook/{webhook_id}"))
            .json(&serde_json::json!({
                "type": "update_sensor_states",
                "data": [{
                    "type": "sensor",
                    "unique_id": "battery_level",
                    "state": "90"
                }]
            }))
            .await
            .assert_status_ok();

        // History should contain both the initial and updated reading.
        // Look up the actual entity_id (includes the random webhook_id, not the device name).
        let entities = state
            .mobile_entities
            .list_by_webhook_id(&webhook_id)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|e| e.sensor_unique_id == "battery_level")
            .expect("registered sensor should appear in entity store");
        let entity_id = &entity.entity_id;
        let entries = state.history.last_n(entity_id, 10).await;
        assert!(
            !entries.is_empty(),
            "numeric sensor with state_class must be recorded in history"
        );
        // The most recent reading should be 90
        let last_val = entries.last().unwrap().value;
        assert!(
            (last_val - 90.0).abs() < f64::EPSILON,
            "last recorded value should be 90, got {last_val}"
        );
    }

    /// Sensor with unit_of_measurement but no state_class IS recorded.
    ///
    /// Source: homeassistant/components/sensor/__init__.py _numeric_state_expected()
    ///   native_unit_of_measurement is not None → numeric
    #[tokio::test]
    async fn numeric_sensor_with_unit_only_is_recorded_in_history() {
        let (server, state) = make_full_server();
        let webhook_id = register_device(&server).await;

        server
            .post(&format!("/api/webhook/{webhook_id}"))
            .json(&serde_json::json!({
                "type": "register_sensor",
                "data": {
                    "type": "sensor",
                    "unique_id": "temperature",
                    "name": "Temperature",
                    "state": "21.5",
                    "unit_of_measurement": "°C"
                }
            }))
            .await
            .assert_status(StatusCode::CREATED);

        let entities = state
            .mobile_entities
            .list_by_webhook_id(&webhook_id)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|e| e.sensor_unique_id == "temperature")
            .expect("registered sensor should appear in entity store");
        let entity_id = &entity.entity_id;
        let entries = state.history.last_n(entity_id, 10).await;
        assert!(
            !entries.is_empty(),
            "sensor with unit_of_measurement must be recorded even without state_class"
        );
    }

    /// Sensor with no state_class and no unit_of_measurement is NOT recorded.
    ///
    /// Source: homeassistant/components/sensor/__init__.py _numeric_state_expected()
    ///   Neither state_class nor unit_of_measurement → not numeric (e.g. category/text sensor)
    #[tokio::test]
    async fn non_numeric_sensor_without_state_class_is_not_recorded() {
        let (server, state) = make_full_server();
        let webhook_id = register_device(&server).await;

        server
            .post(&format!("/api/webhook/{webhook_id}"))
            .json(&serde_json::json!({
                "type": "register_sensor",
                "data": {
                    "type": "sensor",
                    "unique_id": "activity",
                    "name": "Activity",
                    "state": "walking"
                    // No state_class, no unit_of_measurement → category/text sensor
                }
            }))
            .await
            .assert_status(StatusCode::CREATED);

        // Look up the actual entity_id to ensure we're checking the right slot.
        let entities = state
            .mobile_entities
            .list_by_webhook_id(&webhook_id)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|e| e.sensor_unique_id == "activity")
            .expect("registered sensor should appear in entity store");
        let entity_id = &entity.entity_id;
        let entries = state.history.last_n(entity_id, 10).await;
        assert!(
            entries.is_empty(),
            "text sensor without state_class/unit must NOT be recorded; got {} entries",
            entries.len()
        );
    }

    /// binary_sensor is never recorded regardless of numeric-looking state.
    ///
    /// Source: binary_sensor states are "on"/"off" strings — not numeric.
    ///   They have no state_class and no unit_of_measurement.
    #[tokio::test]
    async fn binary_sensor_is_not_recorded_in_history() {
        let (server, state) = make_full_server();
        let webhook_id = register_device(&server).await;

        server
            .post(&format!("/api/webhook/{webhook_id}"))
            .json(&serde_json::json!({
                "type": "register_sensor",
                "data": {
                    "type": "binary_sensor",
                    "unique_id": "charging",
                    "name": "Charging",
                    "state": "on"
                }
            }))
            .await
            .assert_status(StatusCode::CREATED);

        let entities = state
            .mobile_entities
            .list_by_webhook_id(&webhook_id)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|e| e.sensor_unique_id == "charging")
            .expect("registered binary_sensor should appear in entity store");
        let entity_id = &entity.entity_id;
        let entries = state.history.last_n(entity_id, 10).await;
        assert!(
            entries.is_empty(),
            "binary_sensor must never be recorded in history"
        );
    }

    /// GET /api/edge/history/{entity_id} returns readings for a numeric sensor.
    ///
    /// The /api/edge/history path is intentionally non-standard (diverges from
    /// HA's /api/history/period format which uses compressed state {"s","a","lu"}).
    #[tokio::test]
    async fn edge_history_endpoint_returns_readings_for_numeric_sensor() {
        let (server, state) = make_full_server();
        let webhook_id = register_device(&server).await;

        // Register and update a numeric sensor
        server
            .post(&format!("/api/webhook/{webhook_id}"))
            .json(&serde_json::json!({
                "type": "register_sensor",
                "data": {
                    "type": "sensor",
                    "unique_id": "steps",
                    "name": "Steps",
                    "state": "1000",
                    "state_class": "total_increasing",
                    "unit_of_measurement": "steps"
                }
            }))
            .await
            .assert_status(StatusCode::CREATED);

        server
            .post(&format!("/api/webhook/{webhook_id}"))
            .json(&serde_json::json!({
                "type": "update_sensor_states",
                "data": [{"type": "sensor", "unique_id": "steps", "state": "1500"}]
            }))
            .await
            .assert_status_ok();

        // Look up actual entity_id (includes random webhook_id in path).
        let entities = state
            .mobile_entities
            .list_by_webhook_id(&webhook_id)
            .await
            .unwrap();
        let entity = entities
            .iter()
            .find(|e| e.sensor_unique_id == "steps")
            .expect("registered sensor should appear in entity store");
        let entity_id = &entity.entity_id;
        let resp = server
            .get(&format!("/api/edge/history/{entity_id}"))
            .await;
        resp.assert_status_ok();
        let entries: Value = resp.json();
        assert!(entries.is_array(), "response must be a JSON array");
        assert!(
            !entries.as_array().unwrap().is_empty(),
            "history array must be non-empty for numeric sensor"
        );
        // Each entry has ts (u64) and value (f64)
        let first = &entries[0];
        assert!(first.get("ts").is_some(), "entry must have 'ts' field");
        assert!(first.get("value").is_some(), "entry must have 'value' field");
    }

    /// Old /api/history path no longer exists (returns 404).
    ///
    /// Renamed to /api/edge/history to avoid confusion with HA's history API format.
    #[tokio::test]
    async fn old_api_history_path_returns_404() {
        let (server, _state) = make_full_server();
        let resp = server.get("/api/history/sensor.test").await;
        resp.assert_status(StatusCode::NOT_FOUND);
    }
}

