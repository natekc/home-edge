//! HA-compatible REST API endpoints.
//!
//! This module implements the externally visible Home Assistant REST API
//! surface required for official app and common API client compatibility.
//!
//! Endpoint sources (Python reference):
//!   GET /api/            → homeassistant/components/api/__init__.py  APIStatusView
//!   GET /api/core/state  → homeassistant/components/api/__init__.py  APICoreStateView
//!   GET /api/config      → homeassistant/components/api/__init__.py  APIConfigView
//!                          homeassistant/core_config.py  Config.as_dict()
//!
//! All response types are defined in ha-types and share tests with the
//! protocol golden values derived from the HA Python source.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
#[allow(unused_imports)]
use axum::Json;

use ha_types::api::{ApiConfigResponse, ApiStatusResponse, UnitSystem};
use ha_types::core_state::{CoreState, CoreStateResponse, RecorderState};

use crate::app::AppState;

/// Return a router for all HA-compatible API endpoints.
///
/// State is NOT applied here so the caller can merge this router into the
/// main router and apply state once at the top level.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // GET /api/  — HA API status
        // Source: homeassistant/components/api/__init__.py  APIStatusView
        .route("/api/", get(api_status))
        // GET /api/core/state  — lightweight liveness + recorder status
        // Source: homeassistant/components/api/__init__.py  APICoreStateView
        // URL constant: homeassistant/const.py  URL_API_CORE_STATE = "/api/core/state"
        .route("/api/core/state", get(api_core_state))
        // GET /api/config  — instance configuration
        // Source: homeassistant/components/api/__init__.py  APIConfigView
        // URL constant: homeassistant/const.py  URL_API_CONFIG = "/api/config"
        .route("/api/config", get(api_config))
}

/// GET /api/
///
/// Source: homeassistant/components/api/__init__.py  APIStatusView.get
///   return self.json_message("API running.")
///   → HTTP 200 {"message": "API running."}
async fn api_status() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(ApiStatusResponse::default()))
}

/// GET /api/core/state
///
/// Source: homeassistant/components/api/__init__.py  APICoreStateView.get
/// Response:
///   {"state": "<CoreState.value>",
///    "recorder_state": {"migration_in_progress": bool, "migration_is_live": bool}}
///
/// This is the endpoint supervisor and the frontend use to check if HA is up.
async fn api_core_state(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let resp = CoreStateResponse {
        state: CoreState::Running,
        recorder_state: RecorderState {
            migration_in_progress: false,
            migration_is_live: false,
        },
    };
    (StatusCode::OK, axum::Json(resp))
}

/// GET /api/config
///
/// Source: homeassistant/components/api/__init__.py  APIConfigView.get
///   return self.json(request.app[KEY_HASS].config.as_dict())
///
/// Returns the instance configuration. Field set mirrors Config.as_dict() in
/// homeassistant/core_config.py.  Non-configured fields use defaults.
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
        components: vec![
            "api".into(),
            "core".into(),
        ],
        whitelist_external_dirs: vec![],
    };
    (StatusCode::OK, axum::Json(cfg)).into_response()
}

// ---------------------------------------------------------------------------
// Integration tests
//
// These tests verify the HTTP response shape against the HA protocol contract,
// not just that our code compiles.  Each test cites the Python source it is
// derived from.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use axum_test::TestServer;
    use serde_json::Value;

    fn make_server() -> TestServer {
        use std::sync::Arc;
        use crate::app::AppState;
        use crate::config::{AppConfig, ServerConfig, StorageConfig, UiConfig};
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
        let state = Arc::new(AppState { config, storage });
        // Build only the HA API router for focused tests
        let app = super::router().with_state(state);
        TestServer::new(app).unwrap()
    }

    /// Golden: GET /api/ must return HTTP 200 with {"message": "API running."}
    ///
    /// Source: homeassistant/components/api/__init__.py  APIStatusView.get
    ///   `return self.json_message("API running.")`
    ///   HomeAssistantView.json_message → HTTP 200 {"message": "..."}
    #[tokio::test]
    async fn get_api_status_returns_200_with_message() {
        let server = make_server();
        let resp = server.get("/api/").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(
            json["message"], "API running.",
            "must match HA APIStatusView exactly"
        );
        // Must have exactly one field — no extra keys
        assert_eq!(
            json.as_object().unwrap().len(),
            1,
            "status response must have only `message`"
        );
    }

    /// Golden: GET /api/core/state must return correct shape and RUNNING state.
    ///
    /// Source: homeassistant/components/api/__init__.py  APICoreStateView.get
    ///   Returns {"state": hass.state.value, "recorder_state": {...}}
    ///
    /// The supervisor checks `json["state"] == "RUNNING"` to decide if HA is up.
    #[tokio::test]
    async fn get_api_core_state_returns_running() {
        let server = make_server();
        let resp = server.get("/api/core/state").await;
        resp.assert_status_ok();
        let json: Value = resp.json();

        assert_eq!(
            json["state"], "RUNNING",
            "state must be CoreState.Running value"
        );
        assert!(
            json.get("recorder_state").is_some(),
            "recorder_state must be present"
        );
        assert_eq!(json["recorder_state"]["migration_in_progress"], false);
        assert_eq!(json["recorder_state"]["migration_is_live"], false);
    }

    /// Golden: GET /api/config must return required HA config fields.
    ///
    /// Source: homeassistant/core_config.py  Config.as_dict()
    ///   Returns a dict with at least: version, location_name, time_zone,
    ///   language, latitude, longitude, elevation, unit_system, state, components
    #[tokio::test]
    async fn get_api_config_returns_required_fields() {
        let server = make_server();
        let resp = server.get("/api/config").await;
        resp.assert_status_ok();
        let json: Value = resp.json();

        let required = [
            "version",
            "location_name",
            "time_zone",
            "language",
            "latitude",
            "longitude",
            "elevation",
            "unit_system",
            "state",
            "components",
        ];
        for field in required {
            assert!(
                json.get(field).is_some(),
                "/api/config missing required field: {field}"
            );
        }
        // unit_system must be an object with specific keys
        let us = &json["unit_system"];
        for key in &["length", "temperature", "mass", "volume", "pressure", "wind_speed"] {
            assert!(us.get(key).is_some(), "unit_system missing key: {key}");
        }
    }

    /// Content-Type must be application/json for all HA API endpoints.
    ///
    /// HA clients check Content-Type before parsing the body.
    #[tokio::test]
    async fn api_endpoints_return_json_content_type() {
        let server = make_server();
        for path in ["/api/", "/api/core/state", "/api/config"] {
            let resp = server.get(path).await;
            let ct = resp.headers().get("content-type").expect("content-type header missing");
            assert!(
                ct.to_str().unwrap().contains("application/json"),
                "{path} must return application/json"
            );
        }
    }
}
