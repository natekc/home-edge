//! HA-compatible mobile app registration endpoint.
//!
//! Implements the registration flow used by the official Home Assistant
//! companion apps (iOS and Android).
//!
//! Reference Python source:
//!   POST /api/mobile_app/registrations
//!     → homeassistant/components/mobile_app/http_api.py  RegistrationsView.post
//!
//! Field names come from:
//!   homeassistant/components/mobile_app/const.py
//!
//! ## Request (JSON body)
//! Required fields:
//!   app_id, app_name, app_version, device_name, manufacturer, model, os_name,
//!   supports_encryption
//! Optional fields:
//!   app_data, device_id, os_version
//!
//! ## Response (HTTP 201 Created, JSON)
//!   {"webhook_id": "...", "secret": "..." or null, "cloudhook_url": null, "remote_ui_url": null}

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppState;
use crate::mobile_device_store::MobileDeviceRegistration;

// ---------------------------------------------------------------------------
// Request / Response shapes
// ---------------------------------------------------------------------------

/// POST /api/mobile_app/registrations request body.
///
/// Source: RegistrationsView.post schema (mobile_app/http_api.py)
///   vol.Required(ATTR_APP_ID): cv.string,
///   vol.Required(ATTR_APP_NAME): cv.string,
///   vol.Required(ATTR_APP_VERSION): cv.string,
///   vol.Required(ATTR_DEVICE_NAME): cv.string,
///   vol.Required(ATTR_MANUFACTURER): cv.string,
///   vol.Required(ATTR_MODEL): cv.string,
///   vol.Required(ATTR_OS_NAME): cv.string,
///   vol.Required(ATTR_SUPPORTS_ENCRYPTION, default=False): cv.boolean,
///   vol.Optional(ATTR_APP_DATA, default={}): SCHEMA_APP_DATA,
///   vol.Optional(ATTR_DEVICE_ID): cv.string,
///   vol.Optional(ATTR_OS_VERSION): cv.string,
/// Extra fields allowed (REMOVE_EXTRA) for forward compatibility.
#[derive(Deserialize)]
struct RegistrationRequest {
    // Required fields
    app_id: String,
    app_name: String,
    app_version: String,
    device_name: String,
    manufacturer: String,
    model: String,
    os_name: String,
    #[serde(default)]
    supports_encryption: bool,
    // Optional fields
    #[allow(dead_code)]
    #[serde(default)]
    os_version: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    device_id: Option<String>,
}

/// POST /api/mobile_app/registrations response body.
///
/// Source: RegistrationsView.post return value:
///   {
///     CONF_CLOUDHOOK_URL: data.get(CONF_CLOUDHOOK_URL),  # None for embedded
///     CONF_REMOTE_UI_URL: remote_ui_url,                  # None for embedded
///     CONF_SECRET: data.get(CONF_SECRET),                 # hex string or None
///     CONF_WEBHOOK_ID: data[CONF_WEBHOOK_ID],             # hex string
///   }
#[derive(Serialize)]
struct RegistrationResponse {
    /// Source: CONF_WEBHOOK_ID = "webhook_id"
    webhook_id: String,
    /// Source: CONF_SECRET = "secret"
    /// Set only when supports_encryption == true.
    secret: Option<String>,
    /// Source: CONF_CLOUDHOOK_URL = "cloudhook_url"
    /// Always null on embedded devices (no cloud subscription).
    cloudhook_url: Option<String>,
    /// Source: CONF_REMOTE_UI_URL = "remote_ui_url"
    /// Always null on embedded devices.
    remote_ui_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/mobile_app/registrations", post(mobile_app_register))
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// POST /api/mobile_app/registrations
///
/// Source: RegistrationsView.post (mobile_app/http_api.py)
///
/// Validates the registration payload, issues a webhook_id via `secrets.token_hex()`
/// (we use UUID v4 hex), optionally issues an encryption secret, and returns
/// the registration response with HTTP 201 Created.
async fn mobile_app_register(
    State(state): State<Arc<AppState>>,
    body: axum::extract::Json<serde_json::Value>,
) -> Response {
    // Validate required fields — source: RegistrationsView.post schema
    let req: RegistrationRequest = match serde_json::from_value(body.0.clone()) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"message": format!("Invalid request: {e}")})),
            )
                .into_response();
        }
    };

    // Validate required string fields are non-empty
    // Source: cv.string validation (voluptuous) — rejects empty strings
    let required_strings = [
        ("app_id", req.app_id.as_str()),
        ("app_name", req.app_name.as_str()),
        ("app_version", req.app_version.as_str()),
        ("device_name", req.device_name.as_str()),
        ("manufacturer", req.manufacturer.as_str()),
        ("model", req.model.as_str()),
        ("os_name", req.os_name.as_str()),
    ];
    for (field, value) in required_strings {
        if value.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"message": format!("Required field '{field}' must not be empty")})),
            )
                .into_response();
        }
    }

    let owner_username = match state.core.auth_user(&state.auth, &state.storage).await {
        Ok(user) => user.map(|user| user.username),
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load auth user: {err:#}")})),
            )
                .into_response();
        }
    };

    let record = match state
        .mobile_devices
        .register(MobileDeviceRegistration {
            app_id: req.app_id,
            app_name: req.app_name,
            app_version: req.app_version,
            device_name: req.device_name,
            manufacturer: req.manufacturer,
            model: req.model,
            os_name: req.os_name,
            os_version: req.os_version,
            device_id: req.device_id,
            supports_encryption: req.supports_encryption,
            owner_username,
        })
        .await
    {
        Ok(record) => record,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to persist mobile registration: {err:#}")})),
            )
                .into_response();
        }
    };

    let resp = RegistrationResponse {
        webhook_id: record.webhook_id,
        secret: record.secret,
        // Source: "no cloud subscription" — always null for embedded device
        cloudhook_url: None,
        remote_ui_url: None,
    };

    // Source: return self.json(..., status_code=HTTPStatus.CREATED)
    (StatusCode::CREATED, Json(resp)).into_response()
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
                data_dir: PathBuf::from("/tmp/ha-mobile-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
            areas: crate::config::AreasConfig::default(),
            home_zone: crate::config::HomeZoneConfig::default(),
            history: crate::config::HistoryConfig::default(),
            mdns: Default::default(),
        };
        let storage = Storage::new_in_memory();
        let state = Arc::new(AppState::new(config, storage));
        let app = super::router().with_state(state);
        TestServer::new(app).unwrap()
    }

    fn valid_registration() -> serde_json::Value {
        serde_json::json!({
            "app_id": "io.homeassistant.ios",
            "app_name": "Home Assistant",
            "app_version": "2024.1",
            "device_name": "My iPhone",
            "manufacturer": "Apple",
            "model": "iPhone 15",
            "os_name": "iOS",
            "os_version": "17.0",
            "supports_encryption": false
        })
    }

    /// Valid registration → 201 Created with required response fields.
    ///
    /// Source: RegistrationsView.post — returns HTTP 201 with webhook_id etc.
    #[tokio::test]
    async fn post_registration_valid_returns_201() {
        let server = make_server();
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&valid_registration())
            .await;
        resp.assert_status(StatusCode::CREATED);
    }

    /// Response must contain webhook_id, cloudhook_url, remote_ui_url, secret.
    ///
    /// Source: RegistrationsView.post return shape:
    ///   {CONF_WEBHOOK_ID, CONF_CLOUDHOOK_URL, CONF_REMOTE_UI_URL, CONF_SECRET}
    #[tokio::test]
    async fn post_registration_response_has_required_fields() {
        let server = make_server();
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&valid_registration())
            .await;
        let json: Value = resp.json();

        // Source: const.py CONF_WEBHOOK_ID = "webhook_id"
        assert!(
            json.get("webhook_id").is_some(),
            "response must contain webhook_id"
        );
        // Source: const.py CONF_CLOUDHOOK_URL = "cloudhook_url"
        assert!(
            json.get("cloudhook_url").is_some(),
            "response must contain cloudhook_url"
        );
        // Source: const.py CONF_REMOTE_UI_URL = "remote_ui_url"
        assert!(
            json.get("remote_ui_url").is_some(),
            "response must contain remote_ui_url"
        );
        // Source: const.py CONF_SECRET = "secret"
        assert!(
            json.get("secret").is_some(),
            "response must contain secret (even if null)"
        );
    }

    /// cloudhook_url and remote_ui_url must be null (no cloud).
    ///
    /// Source: RegistrationsView.post — embedded device has no cloud subscription
    ///   CONF_CLOUDHOOK_URL: data.get(CONF_CLOUDHOOK_URL)  → None
    ///   CONF_REMOTE_UI_URL: remote_ui_url                  → None
    #[tokio::test]
    async fn post_registration_cloud_fields_are_null() {
        let server = make_server();
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&valid_registration())
            .await;
        let json: Value = resp.json();
        assert!(
            json["cloudhook_url"].is_null(),
            "cloudhook_url must be null when no cloud"
        );
        assert!(
            json["remote_ui_url"].is_null(),
            "remote_ui_url must be null when no cloud"
        );
    }

    /// supports_encryption=false → secret must be null.
    ///
    /// Source: RegistrationsView.post
    ///   if data[ATTR_SUPPORTS_ENCRYPTION]: data[CONF_SECRET] = secrets.token_hex(...)
    ///   → only set when encryption requested
    #[tokio::test]
    async fn post_registration_no_encryption_secret_is_null() {
        let server = make_server();
        let mut payload = valid_registration();
        payload["supports_encryption"] = serde_json::json!(false);
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&payload)
            .await;
        let json: Value = resp.json();
        assert!(
            json["secret"].is_null(),
            "secret must be null when supports_encryption=false"
        );
    }

    /// supports_encryption=true → secret must be a non-empty string (64 hex chars).
    ///
    /// Source: RegistrationsView.post
    ///   data[CONF_SECRET] = secrets.token_hex(SecretBox.KEY_SIZE)
    ///   SecretBox.KEY_SIZE == 32 → 64 hex characters
    #[tokio::test]
    async fn post_registration_with_encryption_secret_is_hex_string() {
        let server = make_server();
        let mut payload = valid_registration();
        payload["supports_encryption"] = serde_json::json!(true);
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&payload)
            .await;
        resp.assert_status(StatusCode::CREATED);
        let json: Value = resp.json();
        let secret = json["secret"].as_str().expect("secret must be a string");
        assert_eq!(secret.len(), 64, "secret must be 64 hex chars (32 bytes)");
        assert!(
            secret.chars().all(|c| c.is_ascii_hexdigit()),
            "secret must be hex-encoded"
        );
    }

    /// Each registration must get a unique webhook_id.
    ///
    /// Source: webhook_id = secrets.token_hex() — random per registration
    #[tokio::test]
    async fn post_registration_webhook_ids_are_unique() {
        let server = make_server();
        let resp1 = server
            .post("/api/mobile_app/registrations")
            .json(&valid_registration())
            .await;
        let resp2 = server
            .post("/api/mobile_app/registrations")
            .json(&valid_registration())
            .await;
        let id1 = resp1.json::<Value>()["webhook_id"]
            .as_str()
            .unwrap()
            .to_string();
        let id2 = resp2.json::<Value>()["webhook_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(id1, id2, "webhook_ids must be unique per registration");
    }

    #[tokio::test]
    async fn post_registration_reuses_device_id_when_present() {
        let server = make_server();
        let payload = serde_json::json!({
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
            .json::<Value>();
        let second = server
            .post("/api/mobile_app/registrations")
            .json(&payload)
            .await
            .json::<Value>();

        assert_eq!(first["webhook_id"], second["webhook_id"]);
        assert_eq!(first["secret"], second["secret"]);
    }

    /// Missing required field → 400 Bad Request.
    ///
    /// Source: RegistrationsView.post — schema validation rejects missing required fields
    #[tokio::test]
    async fn post_registration_missing_required_field_returns_400() {
        let server = make_server();
        let mut payload = valid_registration();
        // Remove required field "app_id"
        payload.as_object_mut().unwrap().remove("app_id");
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&payload)
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
    }

    /// Extra unknown fields are ignored (forward compatible).
    ///
    /// Source: RegistrationsView.post schema — extra=vol.REMOVE_EXTRA
    #[tokio::test]
    async fn post_registration_extra_fields_are_ignored() {
        let server = make_server();
        let mut payload = valid_registration();
        payload["unknown_future_field"] = serde_json::json!("some value");
        let resp = server
            .post("/api/mobile_app/registrations")
            .json(&payload)
            .await;
        resp.assert_status(StatusCode::CREATED);
    }
}
