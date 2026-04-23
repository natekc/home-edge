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

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, interval};
use tracing;

use crate::app::AppState;
use crate::core::{Consistency, CoreDeps, DeadlineClass, OperationError, OperationMeta, OperationRequest, OperationResult, PageRequest, StateFilter};
use crate::service::{ServiceCall, ServiceData, ServiceError, ServiceTarget};
use crate::state_store::StateEvent;

/// Version string sent in auth handshake messages.
/// Must be a modern HA-style version (YYYY.M.patch) so the iOS app's
/// version-gated feature checks don't bail out.
const HA_VERSION: &str = "2025.1.0";

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
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(no user-agent)");
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(no origin)");
    tracing::debug!(user_agent, origin, "WebSocket: upgrade request");
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
    tracing::debug!("WebSocket: client connected");
    if ws
        .send(Message::Text(auth_required_msg().into()))
        .await
        .is_err()
    {
        tracing::debug!("WebSocket: failed to send auth_required, dropping");
        return;
    }

    let mut phase = Phase::Auth;
    // Channel for subscription tasks to push outbound messages.
    let (push_tx, mut push_rx) = mpsc::channel::<String>(256);
    // Active subscriptions keyed by the command id that created them.
    let mut subscriptions: HashMap<u64, JoinHandle<()>> = HashMap::new();
    // Keepalive: send a WebSocket Ping frame every 30 s so NAT/proxy idle
    // timeouts don't silently drop the connection.
    let mut keepalive = interval(Duration::from_secs(30));
    keepalive.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            msg = ws.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        tracing::debug!(len = text.len(), "WebSocket: recv text message");
                        let reply = match phase {
                            Phase::Auth => handle_auth_phase(&text, &state, &mut phase).await,
                            Phase::Active => handle_command_phase(&text, &state, &push_tx, &mut subscriptions).await,
                        };
                        if let Some(reply_text) = reply {
                            if ws.send(Message::Text(reply_text.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(cf))) => {
                        tracing::debug!("WebSocket: client sent Close frame: {:?}", cf);
                        break;
                    }
                    None => {
                        tracing::debug!("WebSocket: recv() returned None (stream ended)");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::debug!("WebSocket: recv() error: {e}");
                        break;
                    }
                    // Ping frames are auto-responded by tungstenite; Pong and Binary are ignored.
                    _ => {}
                }
            }
            Some(text) = push_rx.recv() => {
                if ws.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            _ = keepalive.tick() => {
                if ws.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // Clean up all active subscription tasks.
    let n = subscriptions.len();
    if n > 0 {
        tracing::warn!("WebSocket: live connection dropped ({n} subscriptions aborted)");
    } else {
        tracing::debug!("WebSocket: client disconnected (no active subscriptions)");
    }
    for (_, handle) in subscriptions {
        handle.abort();
    }
}

/// Handle a message during the auth phase.
///
/// Source: auth.py  AuthPhase.async_handle
async fn handle_auth_phase(text: &str, state: &Arc<AppState>, phase: &mut Phase) -> Option<String> {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Some(auth_invalid_msg("Auth message incorrectly formatted: expected JSON")),
    };

    // Source: AUTH_MESSAGE_SCHEMA — type must be "auth"
    if parsed.get("type").and_then(|v| v.as_str()) != Some("auth") {
        return Some(auth_invalid_msg(
            "Auth message incorrectly formatted: expected type auth",
        ));
    }

    let auth_msg: AuthMessage = match serde_json::from_value(parsed) {
        Ok(m) => m,
        Err(_) => return Some(auth_invalid_msg("Auth message incorrectly formatted")),
    };

    let token = match auth_msg.access_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return Some(auth_invalid_msg("Invalid access token or password"));
        }
    };

    // Source: auth.py — validate access token against the token store
    if state.tokens.validate_access_token(&token).await.is_some() {
        tracing::info!("WebSocket: auth OK");
        *phase = Phase::Active;
        Some(auth_ok_msg())
    } else {
        tracing::info!("WebSocket: auth_invalid — token not found (len={})", token.len());
        Some(auth_invalid_msg("Invalid access token or password"))
    }
}

/// Handle a command message in the active phase.
///
/// Source: commands.py  handle_* functions
async fn handle_command_phase(
    text: &str,
    state: &Arc<AppState>,
    push_tx: &mpsc::Sender<String>,
    subscriptions: &mut HashMap<u64, JoinHandle<()>>,
) -> Option<String> {
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

    let reply = dispatch_command(cmd.id, &cmd.msg_type, &cmd.extra, state, push_tx, subscriptions).await;
    Some(reply)
}

/// Dispatch a command to the appropriate handler.
async fn dispatch_command(
    id: u64,
    msg_type: &str,
    extra: &Value,
    state: &Arc<AppState>,
    push_tx: &mpsc::Sender<String>,
    subscriptions: &mut HashMap<u64, JoinHandle<()>>,
) -> String {
    match msg_type {
        // Source: commands.py  pong_message
        //   {"id": iden, "type": "pong"}
        // NOTE: must be "type":"pong" not a result message — ha-websocket
        // checks specifically for type=="pong" to reset its keepalive timer.
        "ping" => json!({"id": id, "type": "pong"}).to_string(),

        // Source: commands.py  handle_get_states
        // Returns array of entity states.
        "get_states" => match state.core.execute(
            CoreDeps {
                config: &state.config,
                states: &state.states,
                services: &state.services,
            },
            OperationRequest::ListEntityStates {
                page: PageRequest {
                    limit: state.core.transport_policy().max_page_size,
                    cursor: None,
                    include_attributes: true,
                },
                filter: StateFilter {
                    domain: crate::core::DomainKind::Any,
                    changed_since: None,
                    include_attributes: true,
                },
                meta: default_operation_meta(id),
            },
        ) {
            OperationResult::EntityStates(states) => {
                result_ok(id, serde_json::to_value(states).unwrap_or(json!([])))
            }
            _ => result_err(id, "internal_error", "Failed to fetch states"),
        },

        // Source: commands.py  handle_get_config
        // Returns the HA config dict (same as GET /api/config).
        "get_config" => match state.core.execute(
            CoreDeps {
                config: &state.config,
                states: &state.states,
                services: &state.services,
            },
            OperationRequest::GetConfigSummary,
        ) {
            OperationResult::ConfigSummary(cfg) => {
                result_ok(id, serde_json::to_value(cfg).unwrap_or(json!(null)))
            }
            _ => result_err(id, "internal_error", "Failed to fetch config"),
        },

        "get_services" => match state.core.execute(
            CoreDeps {
                config: &state.config,
                states: &state.states,
                services: &state.services,
            },
            OperationRequest::ListServices {
                page: PageRequest {
                    limit: state.core.transport_policy().max_page_size,
                    cursor: None,
                    include_attributes: false,
                },
                meta: default_operation_meta(id),
            },
        ) {
            OperationResult::ServiceCatalog(services) => {
                let domains = services
                    .into_iter()
                    .map(|entry| {
                        let services = entry
                            .services
                            .into_iter()
                            .map(|service| {
                                let fields = service
                                    .fields
                                    .into_iter()
                                    .map(|field| {
                                        (
                                            field.field,
                                            json!({
                                                "required": field.required,
                                                "selector": field.selector
                                            }),
                                        )
                                    })
                                    .collect::<Map<String, Value>>();
                                (
                                    service.service,
                                    json!({
                                        "name": service.name,
                                        "description": service.description,
                                        "fields": fields,
                                    }),
                                )
                            })
                            .collect::<Map<String, Value>>();
                        (entry.domain, Value::Object(services))
                    })
                    .collect::<Map<String, Value>>();
                result_ok(id, Value::Object(domains))
            }
            _ => result_err(id, "internal_error", "Failed to fetch services"),
        },

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

            let target = match ServiceTarget::from_parts(request.target.as_ref(), Some(&request.service_data)) {
                Ok(target) => target,
                Err(err) => return result_err_value(id, err.as_json()),
            };
            let service_data = match ServiceData::from_json(&request.service_data) {
                Ok(service_data) => service_data,
                Err(err) => return result_err_value(id, err.as_json()),
            };

            match state.core.execute(
                CoreDeps {
                    config: &state.config,
                    states: &state.states,
                    services: &state.services,
                },
                OperationRequest::CallService {
                    call: ServiceCall {
                        domain: request.domain.clone(),
                        service: request.service.clone(),
                        target,
                        data: service_data,
                        return_response: request.return_response,
                    },
                    meta: OperationMeta {
                        allow_deferred: request.return_response,
                        ..default_operation_meta(id)
                    },
                },
            ) {
                OperationResult::ServiceCallCompleted(outcome) => {
                    let mut result = json!({"context": outcome.context});
                    if request.return_response {
                        result["response"] = outcome.response.unwrap_or(json!(null));
                    }
                    result_ok(id, result)
                }
                OperationResult::Error(OperationError::NotFound) => result_err_value(
                    id,
                    ServiceError::NotFound {
                        domain: request.domain,
                        service: request.service,
                    }
                    .as_json(),
                ),
                OperationResult::Error(OperationError::InvalidRequest) => result_err_value(
                    id,
                    ServiceError::InvalidFormat("target must include entity_id".into()).as_json(),
                ),
                _ => result_err(id, "internal_error", "Service call failed"),
            }
        }

        // Source: commands.py  handle_subscribe_entities (~L422)
        // Sends an ack then streams compressed-state events for all entity changes.
        "subscribe_entities" => {
            let initial = state.states.all();
            let rx = state.states.subscribe();
            let handle = spawn_subscribe_entities(id, initial, rx, push_tx.clone());
            subscriptions.insert(id, handle);
            result_ok(id, Value::Null)
        }

        // Source: commands.py  handle_subscribe_events (~L175)
        // Sends an ack then streams state_changed events (filtered by event_type if given).
        "subscribe_events" => {
            let event_type_filter = extra
                .get("event_type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let rx = state.states.subscribe();
            let handle = spawn_subscribe_events(id, event_type_filter, rx, push_tx.clone());
            subscriptions.insert(id, handle);
            result_ok(id, Value::Null)
        }

        // Source: commands.py  handle_unsubscribe_events (~L237)
        "unsubscribe_events" => {
            let sub_id = match extra.get("subscription").and_then(|v| v.as_u64()) {
                Some(s) => s,
                None => {
                    return result_err(id, "invalid_format", "subscription must be a positive integer");
                }
            };
            match subscriptions.remove(&sub_id) {
                Some(handle) => {
                    handle.abort();
                    result_ok(id, Value::Null)
                }
                None => result_err(id, "not_found", "Subscription not found."),
            }
        }

        "config/device_registry/list" => {
            match state.mobile_devices.all().await {
                Ok(devices) => {
                    let entries: Vec<Value> = devices
                        .into_iter()
                        .map(|d| {
                            json!({
                                "id": d.webhook_id,
                                "config_entries": [],
                                "config_entries_subentries": {},
                                "connections": [],
                                "created_at": 0.0,
                                "entry_type": "service",
                                "identifiers": [["mobile_app", d.webhook_id]],
                                "labels": [],
                                "modified_at": 0.0,
                                "name": d.device_name,
                                "manufacturer": d.manufacturer,
                                "model": d.model,
                                "sw_version": d.os_version,
                                "name_by_user": null,
                                "area_id": null,
                                "configuration_url": null,
                                "disabled_by": null,
                                "hw_version": null,
                                "model_id": null,
                                "primary_config_entry": null,
                                "serial_number": null,
                                "via_device_id": null,
                            })
                        })
                        .collect();
                    result_ok(id, json!(entries))
                }
                Err(_) => result_err(id, "internal_error", "Failed to fetch device registry"),
            }
        }

        // Source: homeassistant/components/auth/websocket_api.py  handle_get_current_user
        // iOS HAKit calls this via connection.caches.user on every connect.
        "auth/current_user" => {
            let user = state
                .auth
                .load_user_with_legacy_fallback(&state.storage)
                .await
                .ok()
                .flatten();
            let (name, username) = user
                .as_ref()
                .map(|u| (u.name.as_str(), u.username.as_str()))
                .unwrap_or(("Admin", "admin"));
            // Derive a stable UUID v5 from the username so the id is consistent
            // across restarts without requiring persistent storage.
            let user_id = format!("{:x}", md5_hex(username.as_bytes()));
            result_ok(id, json!({
                "id": user_id,
                "name": name,
                "is_owner": true,
                "is_admin": true,
                "system_generated": false,
                "credentials": [],
                "mfa_modules": [],
            }))
        }

        // Source: homeassistant/components/frontend/__init__.py  websocket_get_panels
        // iOS HAKit calls connection.caches.panels() which subscribes to get_panels.
        // We expose a minimal lovelace panel so the app has a home panel entry.
        "get_panels" => {
            result_ok(id, json!({
                "lovelace": {
                    "component_name": "lovelace",
                    "icon": null,
                    "title": null,
                    "config": null,
                    "url_path": "lovelace",
                    "require_admin": false,
                    "show_in_sidebar": true,
                    "default_visible": true,
                }
            }))
        }

        // Source: homeassistant/components/frontend/__init__.py  websocket_get_themes
        // WebSocketBridge.js (injected by iOS into every WKWebView page) calls this
        // immediately after auth to apply themes. Returning empty themes is correct.
        "frontend/get_themes" => {
            result_ok(id, json!({
                "themes": {},
                "default_theme": "default",
                "default_dark_theme": null,
                "theme_color": null,
            }))
        }

        // Source: homeassistant/components/frontend/storage.py  handle_get_user_data
        // Stores user-specific frontend preferences (dashboard layout, etc).
        "frontend/get_user_data" => {
            result_ok(id, json!({"data": null}))
        }

        // Source: homeassistant/components/config/area_registry.py
        "config/area_registry/list" => {
            match state.area_registry.list().await {
                Ok(areas) => {
                    let entries: Vec<Value> = areas
                        .into_iter()
                        .map(|a| json!({
                            "area_id": a.area_id,
                            "name": a.name,
                            "aliases": a.aliases,
                            "labels": [],
                            "floor_id": a.floor_id,
                            "icon": a.icon,
                            "picture": a.picture,
                        }))
                        .collect();
                    result_ok(id, json!(entries))
                }
                Err(_) => result_err(id, "internal_error", "Failed to load area registry"),
            }
        }

        // Source: homeassistant/components/config/area_registry.py  websocket_create_area
        "config/area_registry/create" => {
            let name = match extra.get("name").and_then(|v| v.as_str()) {
                Some(n) if !n.trim().is_empty() => n.to_string(),
                _ => return result_err(id, "invalid_format", "name is required"),
            };
            match state.area_registry.create(name).await {
                Ok(area) => result_ok(id, json!({
                    "area_id": area.area_id,
                    "name": area.name,
                    "aliases": area.aliases,
                    "labels": [],
                    "floor_id": area.floor_id,
                    "icon": area.icon,
                    "picture": area.picture,
                })),
                Err(_) => result_err(id, "internal_error", "Failed to create area"),
            }
        }

        // Source: homeassistant/components/config/area_registry.py  websocket_update_area
        "config/area_registry/update" => {
            let area_id = match extra.get("area_id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => return result_err(id, "invalid_format", "area_id is required"),
            };
            // Each field is optional; absent = leave unchanged, null = clear.
            let name    = extra.get("name").and_then(|v| v.as_str()).map(str::to_string);
            let aliases = extra.get("aliases").and_then(|v| v.as_array()).map(|arr| {
                arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()
            });
            let floor_id = extra.get("floor_id").map(|v| v.as_str().map(str::to_string));
            let icon     = extra.get("icon").map(|v| v.as_str().map(str::to_string));
            let picture  = extra.get("picture").map(|v| v.as_str().map(str::to_string));

            match state.area_registry.update(&area_id, name, aliases, floor_id, icon, picture).await {
                Ok(Some(area)) => result_ok(id, json!({
                    "area_id": area.area_id,
                    "name": area.name,
                    "aliases": area.aliases,
                    "labels": [],
                    "floor_id": area.floor_id,
                    "icon": area.icon,
                    "picture": area.picture,
                })),
                Ok(None) => result_err(id, "not_found", "Area not found"),
                Err(_)   => result_err(id, "internal_error", "Failed to update area"),
            }
        }

        // Source: homeassistant/components/config/area_registry.py  websocket_delete_area
        "config/area_registry/delete" => {
            let area_id = match extra.get("area_id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => return result_err(id, "invalid_format", "area_id is required"),
            };
            match state.area_registry.delete(&area_id).await {
                Ok(true)  => result_ok(id, Value::Null),
                Ok(false) => result_err(id, "not_found", "Area not found"),
                Err(_)    => result_err(id, "internal_error", "Failed to delete area"),
            }
        }

        // -----------------------------------------------------------------------
        // Zone registry
        // Source: homeassistant/components/zone/__init__.py
        //   DictStorageCollectionWebsocket registers zone/{list,create,update,delete}.
        //
        // Note: zone.home is synthetic (derived from hass.config) and is NOT in the
        // zone storage collection; WS zone/list returns only user-defined zones.
        // -----------------------------------------------------------------------

        // Source: homeassistant/components/zone/__init__.py  DictStorageCollectionWebsocket.ws_list_items
        "zone/list" => {
            match state.zone_store.list().await {
                Ok(zones) => {
                    let items: Vec<Value> = zones.iter().map(|z| json!({
                        "id":        z.zone_id,
                        "name":      z.name,
                        "latitude":  z.latitude,
                        "longitude": z.longitude,
                        "radius":    z.radius,
                        "passive":   z.passive,
                        "icon":      z.icon,
                    })).collect();
                    result_ok(id, json!({"items": items}))
                }
                Err(_) => result_err(id, "internal_error", "Failed to load zones"),
            }
        }

        // Source: homeassistant/components/zone/__init__.py  DictStorageCollectionWebsocket.ws_create_item
        "zone/create" => {
            let name = match extra.get("name").and_then(|v| v.as_str()) {
                Some(n) if !n.trim().is_empty() => n.to_string(),
                _ => return result_err(id, "invalid_format", "name is required"),
            };
            let latitude  = extra.get("latitude").and_then(|v| v.as_f64());
            let longitude = extra.get("longitude").and_then(|v| v.as_f64());
            let radius    = extra.get("radius").and_then(|v| v.as_f64());
            let passive   = extra.get("passive").and_then(|v| v.as_bool());
            let icon      = extra.get("icon").and_then(|v| v.as_str()).map(str::to_string);
            match state.zone_store.create(name, latitude, longitude, radius, passive, icon).await {
                Ok(zone) => result_ok(id, json!({
                    "id":        zone.zone_id,
                    "name":      zone.name,
                    "latitude":  zone.latitude,
                    "longitude": zone.longitude,
                    "radius":    zone.radius,
                    "passive":   zone.passive,
                    "icon":      zone.icon,
                })),
                Err(_) => result_err(id, "internal_error", "Failed to create zone"),
            }
        }

        // Source: homeassistant/components/zone/__init__.py  DictStorageCollectionWebsocket.ws_update_item
        "zone/update" => {
            // Accept both "zone_id" (home-edge) and "id" (HA core) for compat.
            let zone_id = match extra.get("zone_id").or_else(|| extra.get("id"))
                .and_then(|v| v.as_str()) {
                Some(zid) => zid.to_string(),
                None => return result_err(id, "invalid_format", "zone_id is required"),
            };
            // zone.home is synthetic — it cannot be edited through the storage collection.
            if zone_id == "home" {
                return result_err(id, "not_allowed", "zone.home cannot be edited through this API");
            }
            let name      = extra.get("name").and_then(|v| v.as_str()).map(str::to_string);
            let latitude  = extra.get("latitude").map(|v| v.as_f64());
            let longitude = extra.get("longitude").map(|v| v.as_f64());
            let radius    = extra.get("radius").and_then(|v| v.as_f64());
            let passive   = extra.get("passive").and_then(|v| v.as_bool());
            let icon      = extra.get("icon").map(|v| v.as_str().map(str::to_string));
            match state.zone_store.update(&zone_id, name, latitude, longitude, radius, passive, icon).await {
                Ok(Some(zone)) => result_ok(id, json!({
                    "id":        zone.zone_id,
                    "name":      zone.name,
                    "latitude":  zone.latitude,
                    "longitude": zone.longitude,
                    "radius":    zone.radius,
                    "passive":   zone.passive,
                    "icon":      zone.icon,
                })),
                Ok(None) => result_err(id, "not_found", "Zone not found"),
                Err(_)   => result_err(id, "internal_error", "Failed to update zone"),
            }
        }

        // Source: homeassistant/components/zone/__init__.py  DictStorageCollectionWebsocket.ws_delete_item
        "zone/delete" => {
            let zone_id = match extra.get("zone_id").or_else(|| extra.get("id"))
                .and_then(|v| v.as_str()) {
                Some(zid) => zid.to_string(),
                None => return result_err(id, "invalid_format", "zone_id is required"),
            };
            // zone.home is synthetic — it cannot be deleted.
            if zone_id == "home" {
                return result_err(id, "not_allowed", "zone.home cannot be deleted");
            }
            match state.zone_store.delete(&zone_id).await {
                Ok(true)  => result_ok(id, Value::Null),
                Ok(false) => result_err(id, "not_found", "Zone not found"),
                Err(_)    => result_err(id, "internal_error", "Failed to delete zone"),
            }
        }

        // Source: homeassistant/components/config/entity_registry.py  websocket_list_entities
        // iOS model manager caches the full entity + device registry.
        "config/entity_registry/list" => {
            match (state.mobile_entities.all().await, state.mobile_devices.all().await) {
                (Ok(entities), Ok(devices)) => {
                    let entries: Vec<Value> = entities
                        .iter()
                        .map(|e| {
                            let device_id = devices
                                .iter()
                                .find(|d| d.webhook_id == e.webhook_id)
                                .and_then(|d| d.device_id.as_deref())
                                .unwrap_or(&e.webhook_id);
                            json!({
                                "entity_id": e.entity_id,
                                "name": e.name_by_user,
                                "original_name": e.sensor_name,
                                "platform": "mobile_app",
                                "device_id": device_id,
                                "area_id": e.user_area_id,
                                "disabled_by": if e.disabled { Value::String("user".into()) } else { Value::Null },
                                "hidden_by": Value::Null,
                                "aliases": [],
                                "labels": [],
                                "config_entry_id": e.webhook_id,
                                "unique_id": e.sensor_unique_id,
                                "icon": e.icon,
                                "entity_category": e.entity_category,
                            })
                        })
                        .collect();
                    result_ok(id, json!(entries))
                }
                _ => result_err(id, "internal_error", "Failed to fetch entity registry"),
            }
        }

        // Source: homeassistant/components/config/entity_registry.py  websocket_list_for_display
        // Compact variant used by configEntityRegistryListForDisplay() in the iOS app.
        "config/entity_registry/list_for_display" => {
            match (state.mobile_entities.all().await, state.mobile_devices.all().await) {
                (Ok(entities), Ok(devices)) => {
                    let entries: Vec<Value> = entities
                        .iter()
                        .map(|e| {
                            let device_id = devices
                                .iter()
                                .find(|d| d.webhook_id == e.webhook_id)
                                .and_then(|d| d.device_id.as_deref())
                                .unwrap_or(&e.webhook_id);
                            // Compact keys match HAKit's EntityRegistryListForDisplay model:
                            // ei=entity_id, n=name, di=device_id, pl=platform,
                            // ai=area_id, dp=device_class, lb=labels, hb=hidden_by, db=disabled_by
                            json!({
                                "ei": e.entity_id,
                                "n": e.name_by_user.as_deref().unwrap_or(&e.sensor_name),
                                "di": device_id,
                                "pl": "mobile_app",
                                "ai": e.user_area_id,
                                "dp": e.device_class,
                                "lb": [],
                                "hb": Value::Null,
                                "db": if e.disabled { Value::String("user".into()) } else { Value::Null },
                                "ic": e.icon,
                                "ec": e.entity_category,
                            })
                        })
                        .collect();
                    result_ok(id, json!({"entities": entries, "entity_categories": {}}))
                }
                _ => result_err(id, "internal_error", "Failed to fetch entity registry"),
            }
        }

        // Source: homeassistant/components/lovelace/websocket.py  websocket_lovelace_config
        // iOS sidebar uses get_panels to find lovelace, then fetches lovelace/config.
        // We return a minimal dashboard so the app doesn't error.
        "lovelace/config" => {
            result_ok(id, json!({
                "views": [{"title": "Home", "path": "home", "cards": []}]
            }))
        }

        // Source: const.py  ERR_UNKNOWN_COMMAND = "unknown_command"
        _ => result_err(
            id,
            "unknown_command",
            &format!("Unknown command: {msg_type}"),
        ),
    }
}

/// Compute a simple 16-hex-char fingerprint from bytes — used as a stable user id.
/// Not cryptographic; just needs to be deterministic and collision-resistant enough
/// for a single-user device.
fn md5_hex(data: &[u8]) -> u128 {
    // FNV-1a 128-bit variant (no external dep).
    let mut hash: u128 = 0x6c62272e07bb0142_62b821756295c58d_u128;
    for &b in data {
        hash ^= b as u128;
        hash = hash.wrapping_mul(0x0000000001000000_000000000000013B_u128);
    }
    hash
}

// ---------------------------------------------------------------------------
// Subscription helpers
// ---------------------------------------------------------------------------

/// Serialize a State into the compressed-state format used by subscribe_entities events.
/// Source: homeassistant/components/websocket_api/messages.py  compressed_state_dict_add
fn compressed_state(state: &ha_types::entity::State) -> Value {
    // Convert ISO 8601 timestamp to Unix epoch float.
    fn iso_to_epoch(ts: &str) -> f64 {
        // Parse "YYYY-MM-DDTHH:MM:SS.ffffffZ" or with +00:00 offset.
        // We split on 'T', parse the date, then the time.
        let ts = ts.trim_end_matches("+00:00").trim_end_matches('Z');
        let parts: Vec<&str> = ts.splitn(2, 'T').collect();
        if parts.len() != 2 {
            return 0.0;
        }
        let date: Vec<u64> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
        let time_frac: Vec<&str> = parts[1].splitn(2, '.').collect();
        let time_parts: Vec<u64> = time_frac[0].split(':').filter_map(|p| p.parse().ok()).collect();
        if date.len() < 3 || time_parts.len() < 3 {
            return 0.0;
        }
        let micros: f64 = time_frac
            .get(1)
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v / 1_000_000.0)
            .unwrap_or(0.0);
        // Approximate: ignore leap seconds, assume UTC.
        let days_from_epoch = date_to_epoch_days(date[0], date[1], date[2]);
        let secs = days_from_epoch * 86400
            + time_parts[0] * 3600
            + time_parts[1] * 60
            + time_parts[2];
        secs as f64 + micros
    }

    fn date_to_epoch_days(y: u64, m: u64, d: u64) -> u64 {
        // Days from 1970-01-01 to y-m-d (Gregorian, no leap-second correction).
        let mut total = 0u64;
        for yr in 1970..y {
            total += if (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0 { 366 } else { 365 };
        }
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let months = [31u64, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        for mi in 0..(m as usize - 1) { total += months[mi]; }
        total + d - 1
    }

    json!({
        "s": state.state,
        "a": state.attributes,
        "lc": iso_to_epoch(&state.last_changed),
        "lu": iso_to_epoch(&state.last_updated),
    })
}

/// Build the full state dict used in state_changed events.
/// Source: homeassistant/components/websocket_api/messages.py  state_diff_msg
fn full_state_dict(state: &ha_types::entity::State) -> Value {
    json!({
        "entity_id": state.entity_id,
        "state": state.state,
        "attributes": state.attributes,
        "last_changed": state.last_changed,
        "last_updated": state.last_updated,
        "context": {
            "id": state.context.id,
            "parent_id": null,
            "user_id": null,
        },
    })
}

/// Spawn a subscribe_entities subscription task.
/// Source: homeassistant/components/websocket_api/commands.py  handle_subscribe_entities
fn spawn_subscribe_entities(
    sub_id: u64,
    initial: Vec<ha_types::entity::State>,
    mut rx: tokio::sync::broadcast::Receiver<StateEvent>,
    push_tx: mpsc::Sender<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Send initial full-state dump as an additions event.
        let additions: serde_json::Map<String, Value> = initial
            .iter()
            .map(|s| (s.entity_id.clone(), compressed_state(s)))
            .collect();
        let initial_event = json!({
            "id": sub_id,
            "type": "event",
            "event": {"a": additions, "c": {}, "r": []},
        })
        .to_string();
        if push_tx.send(initial_event).await.is_err() {
            return;
        }

        // Forward subsequent state changes.
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let entity_id = event.state.entity_id.clone();
                    let change = json!({
                        "id": sub_id,
                        "type": "event",
                        "event": {
                            "a": {entity_id: compressed_state(&event.state)},
                            "c": {},
                            "r": [],
                        },
                    })
                    .to_string();
                    if push_tx.send(change).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Spawn a subscribe_events subscription task.
/// Source: homeassistant/components/websocket_api/commands.py  handle_subscribe_events
fn spawn_subscribe_events(
    sub_id: u64,
    event_type_filter: Option<String>,
    mut rx: tokio::sync::broadcast::Receiver<StateEvent>,
    push_tx: mpsc::Sender<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let et = "state_changed";
                    if let Some(ref filter) = event_type_filter {
                        if filter != et {
                            continue;
                        }
                    }
                    let now = crate::state_store::now_iso8601();
                    let msg = json!({
                        "id": sub_id,
                        "type": "event",
                        "event": {
                            "event_type": et,
                            "data": {
                                "entity_id": event.state.entity_id,
                                "new_state": full_state_dict(&event.state),
                                "old_state": event.old_state.as_ref().map(full_state_dict),
                            },
                            "origin": "local",
                            "time_fired": now,
                            "context": {
                                "id": event.state.context.id,
                                "parent_id": null,
                                "user_id": null,
                            },
                        },
                    })
                    .to_string();
                    if push_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn default_operation_meta(id: u64) -> OperationMeta {
    OperationMeta {
        request_id: id as u32,
        consistency: Consistency::LivePreferred,
        deadline: DeadlineClass::Interactive,
        allow_cached: true,
        allow_deferred: false,
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

    fn completed_onboarding() -> crate::storage::OnboardingState {
        crate::storage::OnboardingState {
            onboarded: true,
            done: vec!["user".into(), "core_config".into()],
            user: Some(crate::storage::StoredUser {
                name: "Admin".into(),
                username: "admin".into(),
                password: "secret".into(),
                language: "en".into(),
            }),
            location_name: Some("Test Home".into()),
            country: Some("US".into()),
            language: Some("en".into()),
            time_zone: Some("UTC".into()),
            unit_system: Some("metric".into()),
            ..Default::default()
        }
    }

    async fn make_server() -> TestServer {
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
                data_dir: PathBuf::from("/tmp/ha-ws-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
            areas: crate::config::AreasConfig::default(),
            home_zone: crate::config::HomeZoneConfig::default(),
            history: crate::config::HistoryConfig::default(),
        };
        let storage = Storage::new_in_memory();
        storage
            .save_onboarding(&completed_onboarding())
            .await
            .expect("save onboarding state");
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
        let server = make_server().await;
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
        let server = make_server().await;
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
        let server = make_server().await;
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
        let server = make_server().await;
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

    /// ping command -> {"type":"pong","id":N}.
    ///
    /// Source: commands.py pong_message
    #[tokio::test]
    async fn ws_ping_returns_pong_result() {
        let server = make_server().await;
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
        assert_eq!(resp["type"], "pong");
    }

    /// get_states -> result array.
    ///
    /// Source: commands.py handle_get_states
    #[tokio::test]
    async fn ws_get_states_returns_array() {
        let server = make_server().await;
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
        let server = make_server().await;
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
        let server = make_server().await;
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
    /// Source: commands.py pong_message
    #[tokio::test]
    async fn ws_result_message_has_required_fields() {
        let server = make_server().await;
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
        assert_eq!(resp["id"], 5);
        assert_eq!(resp["type"], "pong");
    }
}
