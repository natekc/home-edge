//! HA-compatible WebSocket API (`/api/websocket`).
//!
//! Implements the HA WebSocket protocol described in:
//!   homeassistant/components/websocket_api/auth.py
//!   homeassistant/components/websocket_api/commands.py
//!   homeassistant/components/websocket_api/messages.py
//!   homeassistant/components/websocket_api/const.py
//!
//! ## Protocol synopsis
//!
//! 1. On connect server → `{"type":"auth_required","ha_version":"..."}`
//! 2. Client → `{"type":"auth","access_token":"..."}`
//! 3. Server → `{"type":"auth_ok","ha_version":"..."}` or `{"type":"auth_invalid","message":"..."}`
//! 4. Client → `{"id":N,"type":"<command>", ...params}`
//! 5. Server → `{"id":N,"type":"result","success":true,"result":...}`
//!             or `{"id":N,"type":"result","success":false,"error":{"code":"...","message":"..."}}`

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::app::AppState;
use crate::service::ServiceError;

/// Version string sent in auth handshake messages.
/// Source: homeassistant/const.py  __version__ (we mimic a recent HA version).
const HA_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Message types (source: websocket_api/auth.py and const.py)
// ---------------------------------------------------------------------------

/// Client → server: authenticate with an access token.
///
/// Source: auth.py  AUTH_MESSAGE_SCHEMA
#[derive(Deserialize)]
struct AuthMessage {
    access_token: Option<String>,
}

/// Client → server: any command after auth.
///
/// Source: messages.py  BASE_COMMAND_MESSAGE_SCHEMA
///   {vol.Required("id"): cv.positive_int, vol.Required("type"): cv.string}
#[derive(Deserialize)]
struct CommandMessage {
    id: u64,
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(flatten)]
    #[allow(dead_code)]
    extra: Value,
}

// ---------------------------------------------------------------------------
// Message constructors (mirrors messages.py helpers)
// ---------------------------------------------------------------------------

/// `{"type":"auth_required","ha_version":"..."}`
///
/// Source: auth.py  AUTH_REQUIRED_MESSAGE
fn auth_required_msg() -> String {
    json!({"type": "auth_required", "ha_version": HA_VERSION}).to_string()
}

/// `{"type":"auth_ok","ha_version":"..."}`
///
/// Source: auth.py  AUTH_OK_MESSAGE
fn auth_ok_msg() -> String {
    json!({"type": "auth_ok", "ha_version": HA_VERSION}).to_string()
}

/// `{"type":"auth_invalid","message":"..."}`
///
/// Source: auth.py  auth_invalid_message(message)
fn auth_invalid_msg(message: &str) -> String {
    json!({"type": "auth_invalid", "message": message}).to_string()
}

/// `{"id":N,"type":"result","success":true,"result":...}`
///
/// Source: messages.py  result_message(iden, result)
fn result_ok(id: u64, result: Value) -> String {
    json!({"id": id, "type": "result", "success": true, "result": result}).to_string()
}

/// `{"id":N,"type":"result","success":false,"error":{"code":"...","message":"..."}}`
///
/// Source: messages.py  error_message(iden, code, message)
fn result_err(id: u64, code: &str, message: &str) -> String {
    json!({
        "id": id,
        "type": "result",
        "success": false,
        "error": {"code": code, "message": message}
    })
    .to_string()
}

fn result_err_value(id: u64, error: Value) -> String {
    json!({
        "id": id,
        "type": "result",
        "success": false,
        "error": error
    })
    .to_string()
}

#[derive(Deserialize)]
struct CallServiceRequest {
    domain: String,
    service: String,
    #[serde(default)]
    service_data: Map<String, Value>,
    #[serde(default)]
    target: Option<Value>,
    #[serde(default)]
    return_response: bool,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Return a router for the HA WebSocket endpoint (state not applied yet).
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/websocket", get(ws_handler))
}

// ---------------------------------------------------------------------------
// Upgrade handler
// ---------------------------------------------------------------------------

/// HTTP upgrade handler for `GET /api/websocket`.
///
/// Source: homeassistant/components/websocket_api/http.py  WebsocketAPIView.get
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

// ---------------------------------------------------------------------------
// Per-connection state machine
// ---------------------------------------------------------------------------

enum Phase {
    /// Waiting for the client's `{"type":"auth","access_token":"..."}`.
    Auth,
    /// Client authenticated; processing commands.
    Active,
}

async fn handle_socket(mut ws: WebSocket, state: Arc<AppState>) {
    // Step 1: send auth_required
    // Source: auth.py — server sends AUTH_REQUIRED_MESSAGE immediately on connect
    if ws
        .send(Message::Text(auth_required_msg().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut phase = Phase::Auth;

    while let Some(Ok(msg)) = ws.recv().await {
        match msg {
            Message::Text(text) => {
                let reply = match phase {
                    Phase::Auth => handle_auth_phase(&text, &state, &mut phase).await,
                    Phase::Active => handle_command_phase(&text, &state).await,
                };
                if let Some(reply_text) = reply {
                    if ws.send(Message::Text(reply_text.into())).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            // Ignore binary / ping / pong frames
            _ => {}
        }
    }
}

/// Handle a message during the auth phase.
///
/// Source: auth.py  AuthPhase.async_handle
async fn handle_auth_phase(text: &str, state: &Arc<AppState>, phase: &mut Phase) -> Option<String> {
    let parsed: Value = serde_json::from_str(text).ok()?;

    // Source: AUTH_MESSAGE_SCHEMA — type must be "auth"
    if parsed.get("type").and_then(|v| v.as_str()) != Some("auth") {
        return Some(auth_invalid_msg(
            "Auth message incorrectly formatted: expected type auth",
        ));
    }

    let auth_msg: AuthMessage = serde_json::from_value(parsed).ok()?;

    let token = match auth_msg.access_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return Some(auth_invalid_msg("Invalid access token or password"));
        }
    };

    // Source: auth.py — validate access token against the token store
    if state.tokens.validate_access_token(&token).await.is_some() {
        debug!("WebSocket auth OK");
        *phase = Phase::Active;
        Some(auth_ok_msg())
    } else {
        Some(auth_invalid_msg("Invalid access token or password"))
    }
}

/// Handle a command message in the active phase.
///
/// Source: commands.py  handle_* functions
async fn handle_command_phase(text: &str, state: &Arc<AppState>) -> Option<String> {
    let parsed: Value = serde_json::from_str(text).ok()?;

    // Source: messages.py  BASE_COMMAND_MESSAGE_SCHEMA
    //   {vol.Required("id"): cv.positive_int, vol.Required("type"): cv.string}
    let cmd: CommandMessage = match serde_json::from_value(parsed) {
        Ok(c) => c,
        Err(_) => {
            // We can't extract an id to reply with; HA closes the connection here.
            return None;
        }
    };

    let reply = dispatch_command(cmd.id, &cmd.msg_type, &cmd.extra, state).await;
    Some(reply)
}

/// Dispatch a command to the appropriate handler.
async fn dispatch_command(id: u64, msg_type: &str, extra: &Value, state: &Arc<AppState>) -> String {
    match msg_type {
        // Source: commands.py  handle_ping
        //   {"id": iden, "type": "pong"}   — returned as a result message
        "ping" => result_ok(id, json!("pong")),

        // Source: commands.py  handle_get_states
        // Returns array of entity states.
        "get_states" => {
            let states = state.states.all();
            result_ok(id, serde_json::to_value(states).unwrap_or(json!([])))
        }

        // Source: commands.py  handle_get_config
        // Returns the HA config dict (same as GET /api/config).
        "get_config" => {
            use ha_types::api::{ApiConfigResponse, UnitSystem};
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
            result_ok(id, serde_json::to_value(cfg).unwrap_or(json!(null)))
        }

        "get_services" => result_ok(id, state.services.describe()),

        "call_service" => {
            let request: CallServiceRequest = match serde_json::from_value(extra.clone()) {
                Ok(request) => request,
                Err(err) => {
                    return result_err_value(
                        id,
                        ServiceError::InvalidFormat(format!("Invalid call_service payload: {err}"))
                            .as_json(),
                    );
                }
            };

            match state.services.call(
                state,
                &request.domain,
                &request.service,
                request.service_data,
                request.target.as_ref(),
                request.return_response,
            ) {
                Ok(outcome) => {
                    let mut result = json!({"context": outcome.context});
                    if request.return_response {
                        result["response"] = outcome.response.unwrap_or(json!(null));
                    }
                    result_ok(id, result)
                }
                Err(error) => result_err_value(id, error.as_json()),
            }
        }

        // Source: const.py  ERR_UNKNOWN_COMMAND = "unknown_command"
        _ => result_err(
            id,
            "unknown_command",
            &format!("Unknown command: {msg_type}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum_test::{TestServer, TestServerConfig, Transport};
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
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/ha-ws-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
        };
        let storage = Storage::new_in_memory();
        let state = Arc::new(AppState::new(config, storage));

        let app = super::router()
            .merge(crate::ha_auth::router())
            .with_state(state);

        // WebSocket requires a real HTTP transport layer (not mock).
        // Source: axum-test docs — "WebSocket requires a HTTP based transport layer"
        TestServer::new_with_config(
            app,
            TestServerConfig {
                transport: Some(Transport::HttpRandomPort),
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// GET /api/websocket without Upgrade header -> 400.
    #[tokio::test]
    async fn get_ws_endpoint_without_upgrade_returns_400() {
        let server = make_server();
        let resp = server.get("/api/websocket").await;
        resp.assert_status(StatusCode::BAD_REQUEST);
    }

    async fn get_access_token(server: &TestServer) -> String {
        let resp = server
            .post("/auth/login_flow")
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "handler": ["homeassistant", null],
                "redirect_uri": "homeassistant://auth-callback"
            }))
            .await;
        let flow: Value = resp.json();
        let flow_id = flow["flow_id"].as_str().unwrap().to_string();

        let resp = server
            .post(&format!("/auth/login_flow/{}", flow_id))
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "username": "admin",
                "password": "secret"
            }))
            .await;
        let step: Value = resp.json();
        let auth_code = step["result"].as_str().unwrap().to_string();

        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", auth_code.as_str()),
                ("client_id", "https://home-assistant.io/iOS"),
            ])
            .await;
        let tokens: Value = resp.json();
        tokens["access_token"].as_str().unwrap().to_string()
    }

    /// Server sends auth_required with ha_version immediately on connect.
    ///
    /// Source: auth.py AUTH_REQUIRED_MESSAGE
    #[tokio::test]
    async fn ws_connect_receives_auth_required_with_version() {
        let server = make_server();
        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;

        let msg: Value = ws.receive_json().await;
        assert_eq!(msg["type"], "auth_required");
        assert!(msg.get("ha_version").is_some());
    }

    /// WS auth handshake: valid token -> auth_ok.
    ///
    /// Source: auth.py AUTH_OK_MESSAGE
    #[tokio::test]
    async fn ws_auth_handshake_valid_token() {
        let server = make_server();
        let token = get_access_token(&server).await;

        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;
        let _ = ws.receive_json::<Value>().await; // auth_required

        ws.send_json(&serde_json::json!({
            "type": "auth",
            "access_token": token
        }))
        .await;

        let msg: Value = ws.receive_json().await;
        assert_eq!(msg["type"], "auth_ok");
        assert!(
            msg.get("ha_version").is_some(),
            "auth_ok must include ha_version"
        );
    }

    /// Invalid token -> auth_invalid.
    ///
    /// Source: auth.py auth_invalid_message
    #[tokio::test]
    async fn ws_auth_invalid_token_receives_auth_invalid() {
        let server = make_server();
        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;
        let _ = ws.receive_json::<Value>().await;

        ws.send_json(&serde_json::json!({
            "type": "auth",
            "access_token": "totally-fake-token"
        }))
        .await;

        let msg: Value = ws.receive_json().await;
        assert_eq!(msg["type"], "auth_invalid");
        assert!(msg.get("message").is_some());
    }

    /// ping command -> result with success=true.
    ///
    /// Source: commands.py handle_ping
    #[tokio::test]
    async fn ws_ping_returns_pong_result() {
        let server = make_server();
        let token = get_access_token(&server).await;
        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;
        let _ = ws.receive_json::<Value>().await;
        ws.send_json(&serde_json::json!({"type": "auth", "access_token": token}))
            .await;
        let _ = ws.receive_json::<Value>().await;

        ws.send_json(&serde_json::json!({"id": 1, "type": "ping"}))
            .await;
        let resp: Value = ws.receive_json().await;
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["type"], "result");
        assert_eq!(resp["success"], true);
    }

    /// get_states -> result array.
    ///
    /// Source: commands.py handle_get_states
    #[tokio::test]
    async fn ws_get_states_returns_array() {
        let server = make_server();
        let token = get_access_token(&server).await;
        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;
        let _ = ws.receive_json::<Value>().await;
        ws.send_json(&serde_json::json!({"type": "auth", "access_token": token}))
            .await;
        let _ = ws.receive_json::<Value>().await;

        ws.send_json(&serde_json::json!({"id": 2, "type": "get_states"}))
            .await;
        let resp: Value = ws.receive_json().await;
        assert_eq!(resp["id"], 2);
        assert_eq!(resp["type"], "result");
        assert_eq!(resp["success"], true);
        assert!(resp["result"].is_array());
    }

    /// get_config -> result config object.
    ///
    /// Source: commands.py handle_get_config
    #[tokio::test]
    async fn ws_get_config_returns_config() {
        let server = make_server();
        let token = get_access_token(&server).await;
        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;
        let _ = ws.receive_json::<Value>().await;
        ws.send_json(&serde_json::json!({"type": "auth", "access_token": token}))
            .await;
        let _ = ws.receive_json::<Value>().await;

        ws.send_json(&serde_json::json!({"id": 3, "type": "get_config"}))
            .await;
        let resp: Value = ws.receive_json().await;
        assert_eq!(resp["id"], 3);
        assert_eq!(resp["success"], true);
        let result = &resp["result"];
        assert!(result.get("version").is_some());
        assert!(result.get("location_name").is_some());
    }

    /// Unknown command -> success=false, code=unknown_command.
    ///
    /// Source: const.py ERR_UNKNOWN_COMMAND
    #[tokio::test]
    async fn ws_unknown_command_returns_error() {
        let server = make_server();
        let token = get_access_token(&server).await;
        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;
        let _ = ws.receive_json::<Value>().await;
        ws.send_json(&serde_json::json!({"type": "auth", "access_token": token}))
            .await;
        let _ = ws.receive_json::<Value>().await;

        ws.send_json(&serde_json::json!({"id": 99, "type": "render_template"}))
            .await;
        let resp: Value = ws.receive_json().await;
        assert_eq!(resp["id"], 99);
        assert_eq!(resp["success"], false);
        assert_eq!(resp["error"]["code"], "unknown_command");
    }

    /// Result shape: id + type + success always present.
    ///
    /// Source: messages.py result_message
    #[tokio::test]
    async fn ws_result_message_has_required_fields() {
        let server = make_server();
        let token = get_access_token(&server).await;
        let mut ws = server
            .get_websocket("/api/websocket")
            .await
            .into_websocket()
            .await;
        let _ = ws.receive_json::<Value>().await;
        ws.send_json(&serde_json::json!({"type": "auth", "access_token": token}))
            .await;
        let _ = ws.receive_json::<Value>().await;

        ws.send_json(&serde_json::json!({"id": 5, "type": "ping"}))
            .await;
        let resp: Value = ws.receive_json().await;
        assert!(resp.get("id").is_some());
        assert_eq!(resp["type"], "result");
        assert!(resp.get("success").is_some());
        assert!(resp.get("result").is_some());
    }
}
