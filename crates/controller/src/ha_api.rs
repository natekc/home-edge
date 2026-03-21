//! HA-compatible REST API endpoints.
//!
//! This module implements the externally visible Home Assistant REST API
//! surface required for official app and common API client compatibility.
//!
//! Endpoint sources (Python reference):
//!   GET /api/                      → homeassistant/components/api/__init__.py  APIStatusView
//!   GET /api/core/state            → homeassistant/components/api/__init__.py  APICoreStateView
//!   GET /api/config                → homeassistant/components/api/__init__.py  APIConfigView
//!   GET /api/states                → homeassistant/components/api/__init__.py  APIStatesView
//!   GET /api/states/{entity_id}    → homeassistant/components/api/__init__.py  APIEntityStateView
//!   POST /api/states/{entity_id}   → homeassistant/components/api/__init__.py  APIEntityStateView

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;

use ha_types::api::{ApiConfigResponse, ApiStatusResponse, UnitSystem};
use ha_types::core_state::{CoreState, CoreStateResponse, RecorderState};

use crate::app::AppState;
use crate::state_store::make_state;

/// Return a router for all HA-compatible API endpoints.
///
/// State is NOT applied here so the caller can merge this router into the
/// main router and apply state once at the top level.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/", get(api_status))
        .route("/api/core/state", get(api_core_state))
        .route("/api/config", get(api_config))
        // States endpoints
        // URL constants from homeassistant/const.py:
        //   URL_API_STATES        = "/api/states"
        //   URL_API_STATES_ENTITY = "/api/states/{}"
        .route("/api/states", get(api_states_list))
        .route("/api/states/{entity_id}", get(api_state_get).post(api_state_set))
}

/// GET /api/
///
/// Source: homeassistant/components/api/__init__.py  APIStatusView.get
///   return self.json_message("API running.")
async fn api_status() -> impl IntoResponse {
    (StatusCode::OK, Json(ApiStatusResponse::default()))
}

/// GET /api/core/state
///
/// Source: homeassistant/components/api/__init__.py  APICoreStateView.get
async fn api_core_state(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let resp = CoreStateResponse {
        state: CoreState::Running,
        recorder_state: RecorderState {
            migration_in_progress: false,
            migration_is_live: false,
        },
    };
    (StatusCode::OK, Json(resp))
}

/// GET /api/config
///
/// Source: homeassistant/components/api/__init__.py  APIConfigView.get
async fn api_config(State(state): State<Arc<AppState>>) -> Response {
    let cfg = ApiConfigResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        location_name: state.config.ui.product_name.clone(),
        time_zone: "UTC".into(),
        language: "en".into(),
        latitude: 0.0,
        longitude: 0.0,
        elevation: 0.0,
        unit_system: UnitSystem::metric(),
        state: "RUNNING".into(),
        components: vec!["api".into(), "core".into()],
        whitelist_external_dirs: vec![],
    };
    (StatusCode::OK, Json(cfg)).into_response()
}

/// GET /api/states
///
/// Source: homeassistant/components/api/__init__.py  APIStatesView.get
/// Returns a JSON array of all entity states.
/// HA returns HTTP 200 with an empty array [] when no states exist.
async fn api_states_list(State(state): State<Arc<AppState>>) -> Response {
    let states = state.states.all();
    (StatusCode::OK, Json(states)).into_response()
}

/// GET /api/states/{entity_id}
///
/// Source: homeassistant/components/api/__init__.py  APIEntityStateView.get
/// Returns 200 + state object if found, 404 + {"message": "Entity not found."} if missing.
async fn api_state_get(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
) -> Response {
    match state.states.get(&entity_id) {
        Some(s) => (StatusCode::OK, Json(s)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"message": "Entity not found."})),
        )
            .into_response(),
    }
}

/// POST /api/states/{entity_id}
///
/// Source: homeassistant/components/api/__init__.py  APIEntityStateView.post
///
/// Request body: {"state": "<value>", "attributes": {...}, "force_update": false}
/// Response:
///   201 Created  + state object (new entity)
///   200 OK       + state object (existing entity updated)
///   400 Bad Request  for invalid entity ID, missing state, or invalid JSON
async fn api_state_set(
    State(app): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
    body: axum::extract::Json<serde_json::Value>,
) -> Response {
    // Source: APIEntityStateView.post – validate entity_id format
    if !ha_types::entity::State::is_valid_entity_id(&entity_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"message": "Invalid entity ID specified."})),
        )
            .into_response();
    }

    let data = &body.0;

    // Source: "No state specified."  check
    let Some(new_state_val) = data.get("state").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"message": "No state specified."})),
        )
            .into_response();
    };

    // Source: MAX_LENGTH_STATE_STATE = 255
    if new_state_val.len() > 255 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"message": "Invalid state specified."})),
        )
            .into_response();
    }

    let attributes: HashMap<String, serde_json::Value> = data
        .get("attributes")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    let is_new = app.states.get(&entity_id).is_none();
    let new_state = make_state(&entity_id, new_state_val, attributes);
    // Entity-id validity already checked above; store won't fail.
    let _ = app.states.set(new_state);

    let saved = app.states.get(&entity_id).unwrap();
    let status = if is_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (status, Json(saved)).into_response()
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use axum_test::TestServer;
    use serde_json::{Value, json};

    fn make_server() -> TestServer {
        use std::sync::Arc;
        use crate::app::AppState;
        use crate::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
        use crate::state_store::StateStore;
        use crate::storage::Storage;
        use std::net::{IpAddr, Ipv4Addr};
        use std::path::PathBuf;

        let config = AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/ha-compat-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
        };
        let storage = Storage::new_in_memory();
        let state = Arc::new(AppState {
            config,
            storage,
            states: StateStore::new(),
        });
        let app = super::router().with_state(state);
        TestServer::new(app).unwrap()
    }

    // -----------------------------------------------------------------------
    // GET /api/ — APIStatusView
    // -----------------------------------------------------------------------

    /// Source: homeassistant/components/api/__init__.py  APIStatusView.get
    #[tokio::test]
    async fn get_api_status_returns_200_with_message() {
        let server = make_server();
        let resp = server.get("/api/").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(json["message"], "API running.");
        assert_eq!(json.as_object().unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------
    // GET /api/core/state — APICoreStateView
    // -----------------------------------------------------------------------

    /// Source: homeassistant/components/api/__init__.py  APICoreStateView.get
    #[tokio::test]
    async fn get_api_core_state_returns_running() {
        let server = make_server();
        let resp = server.get("/api/core/state").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(json["state"], "RUNNING");
        assert_eq!(json["recorder_state"]["migration_in_progress"], false);
        assert_eq!(json["recorder_state"]["migration_is_live"], false);
    }

    // -----------------------------------------------------------------------
    // GET /api/config — APIConfigView
    // -----------------------------------------------------------------------

    /// Source: homeassistant/core_config.py  Config.as_dict()
    #[tokio::test]
    async fn get_api_config_returns_required_fields() {
        let server = make_server();
        let resp = server.get("/api/config").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        for field in ["version", "location_name", "time_zone", "language",
                      "latitude", "longitude", "elevation", "unit_system",
                      "state", "components"] {
            assert!(json.get(field).is_some(), "missing /api/config field: {field}");
        }
    }

    // -----------------------------------------------------------------------
    // Content-Type check
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn api_endpoints_return_json_content_type() {
        let server = make_server();
        for path in ["/api/", "/api/core/state", "/api/config", "/api/states"] {
            let resp = server.get(path).await;
            let ct = resp.headers().get("content-type").expect("content-type missing");
            assert!(
                ct.to_str().unwrap().contains("application/json"),
                "{path} must return application/json"
            );
        }
    }

    // -----------------------------------------------------------------------
    // GET /api/states — APIStatesView
    // Source: homeassistant/components/api/__init__.py  APIStatesView.get
    // -----------------------------------------------------------------------

    /// Empty state store returns [] — not null, not 404.
    ///
    /// Source: APIStatesView returns json array; empty array when no entities.
    #[tokio::test]
    async fn get_api_states_empty_returns_array() {
        let server = make_server();
        let resp = server.get("/api/states").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert!(json.is_array(), "must be a JSON array");
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    /// After posting a state, GET /api/states includes it.
    #[tokio::test]
    async fn get_api_states_returns_posted_state() {
        let server = make_server();
        server
            .post("/api/states/light.living_room")
            .json(&json!({"state": "on", "attributes": {"brightness": 255}}))
            .await;
        let resp = server.get("/api/states").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["entity_id"], "light.living_room");
        assert_eq!(arr[0]["state"], "on");
    }

    /// Each state in the array has the required HA state fields.
    ///
    /// Source: homeassistant/core.py  State._as_dict — required keys
    #[tokio::test]
    async fn get_api_states_each_entry_has_required_fields() {
        let server = make_server();
        server
            .post("/api/states/sensor.temperature")
            .json(&json!({"state": "21.5"}))
            .await;
        let resp = server.get("/api/states").await;
        let json: Value = resp.json();
        let entry = &json.as_array().unwrap()[0];
        for field in ["entity_id", "state", "attributes", "last_changed",
                      "last_reported", "last_updated", "context"] {
            assert!(entry.get(field).is_some(), "missing state field: {field}");
        }
        // context must have id, parent_id, user_id
        assert!(entry["context"].get("id").is_some());
        assert!(entry["context"].get("parent_id").is_some());
        assert!(entry["context"].get("user_id").is_some());
    }

    // -----------------------------------------------------------------------
    // GET /api/states/{entity_id} — APIEntityStateView (get)
    // Source: homeassistant/components/api/__init__.py  APIEntityStateView.get
    // -----------------------------------------------------------------------

    /// Returns 404 with {"message": "Entity not found."} for unknown entities.
    ///
    /// Source: APIEntityStateView.get
    ///   return self.json_message("Entity not found.", HTTPStatus.NOT_FOUND)
    #[tokio::test]
    async fn get_entity_state_missing_returns_404() {
        let server = make_server();
        let resp = server.get("/api/states/light.nonexistent").await;
        resp.assert_status(StatusCode::NOT_FOUND);
        let json: Value = resp.json();
        assert_eq!(json["message"], "Entity not found.", "404 message must match HA exactly");
    }

    /// Returns 200 + state object for a known entity.
    #[tokio::test]
    async fn get_entity_state_returns_state() {
        let server = make_server();
        server
            .post("/api/states/switch.kitchen")
            .json(&json!({"state": "off"}))
            .await;
        let resp = server.get("/api/states/switch.kitchen").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(json["entity_id"], "switch.kitchen");
        assert_eq!(json["state"], "off");
    }

    // -----------------------------------------------------------------------
    // POST /api/states/{entity_id} — APIEntityStateView (post)
    // Source: homeassistant/components/api/__init__.py  APIEntityStateView.post
    // -----------------------------------------------------------------------

    /// New entity → HTTP 201 Created.
    ///
    /// Source: APIEntityStateView.post
    ///   status_code = HTTPStatus.CREATED if is_new_state else HTTPStatus.OK
    #[tokio::test]
    async fn post_new_entity_state_returns_201() {
        let server = make_server();
        let resp = server
            .post("/api/states/light.new_light")
            .json(&json!({"state": "on"}))
            .await;
        resp.assert_status(StatusCode::CREATED);
        let json: Value = resp.json();
        assert_eq!(json["entity_id"], "light.new_light");
        assert_eq!(json["state"], "on");
    }

    /// Existing entity → HTTP 200 OK.
    ///
    /// Source: APIEntityStateView.post
    ///   status_code = HTTPStatus.OK (for existing entity)
    #[tokio::test]
    async fn post_existing_entity_state_returns_200() {
        let server = make_server();
        // Create first
        server
            .post("/api/states/light.existing")
            .json(&json!({"state": "on"}))
            .await;
        // Update
        let resp = server
            .post("/api/states/light.existing")
            .json(&json!({"state": "off"}))
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>()["state"], "off");
    }

    /// Missing "state" key → 400 Bad Request with message.
    ///
    /// Source: APIEntityStateView.post
    ///   if (new_state := data.get("state")) is None:
    ///       return self.json_message("No state specified.", HTTPStatus.BAD_REQUEST)
    #[tokio::test]
    async fn post_entity_state_missing_state_returns_400() {
        let server = make_server();
        let resp = server
            .post("/api/states/light.test")
            .json(&json!({"attributes": {}}))
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(resp.json::<Value>()["message"], "No state specified.");
    }

    /// State value longer than 255 chars → 400.
    ///
    /// Source: homeassistant/core.py  MAX_LENGTH_STATE_STATE = 255
    #[tokio::test]
    async fn post_entity_state_too_long_returns_400() {
        let server = make_server();
        let long = "x".repeat(256);
        let resp = server
            .post("/api/states/sensor.test")
            .json(&json!({"state": long}))
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
    }

    use axum::http::StatusCode;
}
