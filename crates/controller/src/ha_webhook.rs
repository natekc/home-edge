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

    /// Register a webhook.
    #[cfg(test)]
    pub async fn register(&self, webhook_id: String, _domain: String, _name: String) {
        let mut inner = self.inner.write().await;
        inner.insert(webhook_id);
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
    // Try to parse body as JSON for storage; fall through gracefully if not JSON.
    let payload: Value = if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("application/json"))
        .unwrap_or(false)
    {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    // Store the received payload for registered webhooks.
    // Source: webhook handler records are dispatched per webhook_id.
    // Even for unknown IDs we return 200 — HA behavior.
    if state.webhooks.is_registered(&webhook_id).await {
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
        use crate::ha_webhook::WebhookStore;
        use crate::storage::Storage;

        let config = AppConfig {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/ha-webhook-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
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
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp"),
            },
            ui: UiConfig {
                product_name: "T".into(),
            },
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
            "topic_pattern must start with discovery_prefix"
        );
    }
}
