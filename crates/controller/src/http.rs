use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use minijinja::context;
use minijinja::Value;
use serde::{Deserialize, Serialize};
use serde_json::{Map, json};

use crate::app::AppState;
use crate::entity_view::{AreaCard, EntityView};
use crate::core::{
    CompleteAnalyticsOutcome, CompleteIntegrationOutcome, CompleteCoreConfigOutcome,
    Consistency, CoreDeps, DeadlineClass, CreateOnboardingUserOutcome, OnboardingCoreConfigInput,
    OnboardingUserInput, OperationError, OperationMeta, OperationRequest, OperationResult,
};
use crate::ha_api;
use crate::ha_auth;
use crate::ha_mobile;
use crate::ha_webhook;
use crate::ha_ws;
use crate::history_store;
use crate::mobile_entity_store::{EntityMetaUpdate, MobileEntityRecord};
use crate::service::{ServiceCall, ServiceData, ServiceTarget};

const STEP_USER: &str = "user";
const STEP_CORE_CONFIG: &str = "core_config";
const STEP_ANALYTICS: &str = "analytics";
const STEP_INTEGRATION: &str = "integration";

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

pub fn internal_error(err: &anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(format!("{err:#}"))),
    )
        .into_response()
}

fn forbidden(msg: &'static str) -> Response {
    (StatusCode::FORBIDDEN, Json(ErrorResponse::new(msg.into()))).into_response()
}

fn unauthorized(msg: &'static str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(ErrorResponse::new(msg.into()))).into_response()
}

/// Self-contained HTML error page for page-navigation contexts.
///
/// Does NOT use the template engine (avoids recursion if templates fail).
/// Fires `connection-status:connected` immediately so the iOS 10-second
/// disconnect timer (started on every `didStartProvisionalNavigation`) is
/// cancelled even when an error occurs.
fn page_error(status: StatusCode, msg: &str) -> Response {
    let safe = msg
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let html = format!(
        concat!(
            "<!doctype html><html lang=\"en\"><head>",
            "<meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">",
            "<title>Error</title>",
            "<script>(function(){{try{{window.webkit.messageHandlers.externalBus",
            ".postMessage({{type:'connection-status',payload:{{event:'connected'}}}});",
            "}}catch(e){{}}}})()</script>",
            "<style>body{{font-family:-apple-system,BlinkMacSystemFont,sans-serif;",
            "padding:32px 24px;color:#333;background:#f5f5f5}}",
            "h2{{font-weight:400;color:#c62828}}a{{color:#009ac7;text-decoration:none}}</style>",
            "</head><body><h2>Error</h2><p>{}</p>",
            "<p><a href=\"javascript:history.back()\">\u{2190} Go back</a></p>",
            "</body></html>"
        ),
        safe
    );
    (status, Html(html)).into_response()
}

/// Router-level 404 — any URL not matched by the router returns HTML so the
/// iOS connected script fires rather than leaving the disconnect timer running.
async fn fallback_404() -> Response {
    page_error(StatusCode::NOT_FOUND, "Page not found")
}

/// Render a named minijinja template into an HTML response.
fn render_template(state: &AppState, name: &str, ctx: Value) -> Response {
    match state.render_html(name, ctx) {
        Ok(html) => Html(html).into_response(),
        Err(err) => page_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Template error ({name}): {err:#}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Context helpers
// ---------------------------------------------------------------------------

fn local_host() -> String {
    local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

async fn load_areas(state: &AppState) -> Vec<crate::area_registry_store::StoredArea> {
    state.area_registry.list().await.unwrap_or_default()
}

async fn load_zones(state: &AppState) -> Vec<crate::zone_store::StoredZone> {
    state.zone_store.list().await.unwrap_or_default()
}

/// Load the configured location name («Nathan's Home») for the sidebar header.
/// Falls back to product_name if onboarding hasn't set one yet.
async fn load_location_name(state: &AppState) -> String {
    state
        .storage
        .load_onboarding()
        .await
        .ok()
        .and_then(|o| o.location_name)
        .unwrap_or_else(|| state.config.ui.product_name.clone())
}

/// Common template context variables present on every app-shell page.
macro_rules! app_ctx {
    ($state:expr, $active:expr, $location_name:expr, $areas:expr, $($rest:tt)*) => {
        context! {
            product_name  => $state.config.ui.product_name.as_str(),
            location_name => $location_name,
            transport     => if cfg!(feature = "transport_wifi") { "WiFi" } else { "BLE" },
            is_ble_build  => cfg!(feature = "transport_ble"),
            zigbee_enabled => cfg!(feature = "zigbee") && $state.config.zigbee.is_some(),
            active_page   => $active,
            server_host   => local_host(),
            server_port   => $state.config.server.port,
            areas         => Value::from_serialize($areas),
            $($rest)*
        }
    };
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Returns the Zigbee-specific routes when the `zigbee` feature is enabled.
/// When the feature is disabled this returns an empty router with no overhead.
fn zigbee_router(state: Arc<AppState>) -> Router {
    #[cfg(feature = "zigbee")]
    {
        Router::new()
            // REST
            .route("/api/zigbee/devices",                    get(api_zigbee_devices))
            .route("/api/zigbee/devices/{ieee}",             patch(api_zigbee_device_rename).delete(api_zigbee_device_delete))
            // POST /delete mirrors the pattern used by area_delete and zone_delete so that
            // plain HTML forms (which only support GET/POST) can trigger device removal.
            .route("/api/zigbee/devices/{ieee}/delete",      post(api_zigbee_device_delete))
            .route("/api/zigbee/entities/{entity_id}",       patch(api_zigbee_entity_update))
            .route("/api/zigbee/permit_join",                post(api_zigbee_permit_join))
            .route("/api/zigbee/permit_join/stop",           post(api_zigbee_permit_join_stop))
            .route("/api/zigbee/permit_join/status",         get(api_zigbee_permit_join_status))
            // HTMX fragments (transport_wifi UI)
            .route("/fragments/zigbee/devices",              get(fragment_zigbee_devices))
            // UI pages
            .route("/zigbee",                                get(zigbee_devices_page))
            .route("/zigbee/{ieee}",                         get(zigbee_device_detail_page))
            .with_state(state)
    }
    #[cfg(not(feature = "zigbee"))]
    {
        let _ = state;
        Router::new()
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // HA-compatible REST API surface
        .merge(ha_api::router())
        .merge(ha_auth::router())
        .merge(ha_ws::router())
        .merge(ha_mobile::router())
        .merge(ha_webhook::router())
        // Web UI pages
        .route("/",                                              get(index))
        .route("/onboarding",                                    get(onboarding_page))
        .route("/ble",                                           get(ble_scan_page))
        .route("/settings",                                      get(settings_page))
        .route("/profile",                                       get(profile_page))
        .route("/devices",                                        get(devices_list_page))
        .route("/devices/{webhook_id}",                          get(device_detail_page))
        .route("/devices/{webhook_id}/entities/{entity_id}",     get(entity_edit_page))
        .route(
            "/devices/{webhook_id}/entities/{entity_id}/save",
            post(entity_edit_save),
        )
        // New UI pages
        .route("/history",                                           get(history_page))
        .route("/logbook",                                           get(logbook_page))
        .route("/developer-tools",                                   get(developer_tools_page))
        .route("/notifications",                                     get(notifications_page))
        .route("/system",                                            get(system_page))
        .route("/areas",                                             get(areas_page).post(areas_create))
        .route("/areas/{area_id}",                                   get(area_detail_page))
        .route("/areas/{area_id}/delete",                            post(area_delete))
        // Zones (geographic geofence zones — mirrors HA core zone component)
        .route("/zones",                                             post(zones_create))
        .route("/zones/{zone_id}",                                   get(zone_edit_page).post(zone_update))
        .route("/zones/{zone_id}/delete",                            post(zone_delete))
        // HTMX fragments
        .route("/fragments/dashboard-sensors",                   get(fragment_dashboard_sensors))
        .route("/fragments/area-sensors/{area_id}",              get(fragment_area_sensors))
        .route("/fragments/more-info/{entity_id}",               get(fragment_more_info))
        // UI service call (form-encoded, returns 204 for hx-swap="none")
        .route("/ui/services/{domain}/{service}",                post(ui_service_call))
        // Mutation API (HTMX)
        .route("/api/devices/{webhook_id}",                      patch(api_device_rename))
        // BLE stubs
        .route("/api/ble/scan",                                  post(api_ble_scan))
        .route("/api/ble/pair",                                  post(api_ble_pair))
        // History JSON
        // Edge-internal history API. Not a replica of HA's /api/history/period endpoint
        // (which uses compressed-state wire format: {"s", "a", "lu"}).
        .route("/api/edge/history/{entity_id}",                  get(api_history))
        // System API
        .route("/api/system/restart",                            post(api_system_restart))
        // Health + onboarding REST API
        .route("/api/health",                                    get(health))
        .route("/api/onboarding",                                get(onboarding_status))
        .route(
            "/api/onboarding/installation_type",
            get(onboarding_installation_type),
        )
        .route("/api/onboarding/users",                          post(create_onboarding_user))
        .route("/api/onboarding/core_config",                    post(complete_core_config))
        .route("/api/onboarding/analytics",                      post(complete_analytics))
        .route("/api/onboarding/integration",                    post(complete_integration))
        .route("/api/onboarding/integration/wait",               post(onboarding_integration_wait))
        .route("/api/onboarding/complete",                       post(complete_onboarding))
        .with_state(state.clone())
        // Any unmatched route returns an HTML 404 so the iOS connected script fires.
        .fallback(fallback_404)
        .merge(zigbee_router(state))
}

// ---------------------------------------------------------------------------
// Web UI page handlers
// ---------------------------------------------------------------------------

async fn index(State(state): State<Arc<AppState>>) -> Response {
    match state.core.onboarding_progress(&state.storage).await {
        Ok(progress) if !progress.onboarded => redirect("/onboarding"),
        Ok(_) => dashboard_response(&state).await,
        Err(err) => page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// POST/Redirect/GET: always 303 See Other so WKWebView converts the POST to a GET.
/// 307 Temporary Redirect preserves the POST method — WKWebView would re-POST with
/// an empty body, axum's Form extractor would reject it with 422 (plain text,
/// no HTML), the iOS disconnect timer would fire, and the user would see a black screen.
fn redirect(path: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, HeaderValue::from_static(path));
    (StatusCode::SEE_OTHER, headers).into_response()
}

async fn profile_page(State(state): State<Arc<AppState>>) -> Response {
    let onboarding = state.storage.load_onboarding().await.unwrap_or_default();
    let user = state
        .auth
        .load_user_with_legacy_fallback(&state.storage)
        .await
        .ok()
        .flatten();
    let location_name = onboarding
        .location_name
        .clone()
        .unwrap_or_else(|| state.config.ui.product_name.clone());
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "profile", location_name.as_str(), &areas,
        user_name     => user.as_ref().map(|u| u.name.as_str()).unwrap_or("—"),
        user_username => user.as_ref().map(|u| u.username.as_str()).unwrap_or("—"),
        language      => onboarding.language.as_deref().unwrap_or("—"),
        time_zone     => onboarding.time_zone.as_deref().unwrap_or("—"),
        unit_system   => onboarding.unit_system.as_deref().unwrap_or("—"),
        country       => onboarding.country.as_deref().unwrap_or("—"),
    );
    render_template(&state, "profile.html", ctx)
}

async fn dashboard_response(state: &AppState) -> Response {
    let devices = match state.mobile_devices.all().await {
        Ok(d) => d,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let all_entities = match state.mobile_entities.all().await {
        Ok(e) => e,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let device_summaries: Vec<_> = devices
        .iter()
        .map(|d| {
            let count = all_entities
                .iter()
                .filter(|e| e.webhook_id == d.webhook_id)
                .count();
            json!({
                "webhook_id":   d.webhook_id,
                "device_name":  d.display_name(),
                "manufacturer": d.manufacturer,
                "model":        d.model,
                "os_name":      d.os_name,
                "entity_count": count,
            })
        })
        .collect();
    let all_views = match collect_all_entity_views(state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let area_cards = build_area_cards(state, &all_views).await;
    let location_name = load_location_name(state).await;
    let areas = load_areas(state).await;
    let ctx = app_ctx!(state, "dashboard", location_name.as_str(), &areas,
        devices    => Value::from_serialize(&device_summaries),
        area_cards => Value::from_serialize(&area_cards),
    );
    render_template(state, "dashboard.html", ctx)
}

async fn onboarding_page(State(state): State<Arc<AppState>>) -> Response {
    let progress = match state.core.onboarding_progress(&state.storage).await {
        Ok(p) => p,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let steps = vec![
        json!({"done": progress.user_done,        "label": "Create owner account"}),
        json!({"done": progress.core_config_done, "label": "Configure your home"}),
        json!({"done": progress.analytics_done,   "label": "Analytics preference"}),
        json!({"done": progress.integration_done, "label": "Connect Home Assistant app"}),
    ];
    let ctx = context! {
        product_name => state.config.ui.product_name.as_str(),
        onboarded    => progress.onboarded,
        server_host  => local_host(),
        server_port  => state.config.server.port,
        steps        => Value::from_serialize(&steps),
    };
    render_template(&state, "onboarding.html", ctx)
}

async fn settings_page(State(state): State<Arc<AppState>>) -> Response {
    let mode = format!("{:?}", state.core.runtime_mode());
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "settings", location_name.as_str(), &areas,
        version      => env!("CARGO_PKG_VERSION"),
        runtime_mode => mode.as_str(),
    );
    render_template(&state, "settings.html", ctx)
}

async fn devices_list_page(State(state): State<Arc<AppState>>) -> Response {
    let devices = match state.mobile_devices.all().await {
        Ok(d) => d,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let all_entities = match state.mobile_entities.all().await {
        Ok(e) => e,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let device_summaries: Vec<_> = devices
        .iter()
        .map(|d| {
            let count = all_entities
                .iter()
                .filter(|e| e.webhook_id == d.webhook_id)
                .count();
            json!({
                "webhook_id":   d.webhook_id,
                "device_name":  d.display_name(),
                "manufacturer": d.manufacturer,
                "model":        d.model,
                "os_name":      d.os_name,
                "entity_count": count,
            })
        })
        .collect();
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "settings", location_name.as_str(), &areas,
        devices => Value::from_serialize(&device_summaries),
    );
    render_template(&state, "devices.html", ctx)
}

async fn ble_scan_page(State(state): State<Arc<AppState>>) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "ble", location_name.as_str(), &areas,);
    render_template(&state, "ble_scan.html", ctx)
}

async fn history_page(State(state): State<Arc<AppState>>) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let all_entities = match state.mobile_entities.all().await {
        Ok(e) => e,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let mut entity_list: Vec<serde_json::Value> = all_entities
        .iter()
        .filter(|e| !e.disabled)
        .map(|e| json!({
            "entity_id": e.entity_id,
            "display_name": e.display_name(),
            "entity_type": e.entity_type,
        }))
        .collect();
    entity_list.sort_by(|lhs, rhs| {
        lhs["display_name"].as_str().unwrap_or("").cmp(rhs["display_name"].as_str().unwrap_or(""))
    });
    let ctx = app_ctx!(state, "history", location_name.as_str(), &areas,
        entities => Value::from_serialize(&entity_list),
    );
    render_template(&state, "history.html", ctx)
}

async fn logbook_page(State(state): State<Arc<AppState>>) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let entries = state.logbook.recent_n(200).await;
    let ctx = app_ctx!(state, "logbook", location_name.as_str(), &areas,
        entries => Value::from_serialize(&entries),
    );
    render_template(&state, "logbook.html", ctx)
}

async fn developer_tools_page(State(state): State<Arc<AppState>>) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let all_entities = match state.mobile_entities.all().await {
        Ok(e) => e,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    // Include disabled entities intentionally — developer tools shows the full picture.
    // Reuse entity_to_view so the state fallback logic is not duplicated here.
    let entity_states: Vec<serde_json::Value> = all_entities
        .iter()
        .map(|e| {
            let view = entity_to_view(e, &state);
            json!({
                "entity_id": view.entity_id,
                "display_name": view.display_name,
                "entity_type": view.entity_type,
                "state": view.value,
            })
        })
        .collect();
    let ctx = app_ctx!(state, "developer-tools", location_name.as_str(), &areas,
        entity_states => Value::from_serialize(&entity_states),
    );
    render_template(&state, "developer_tools.html", ctx)
}

async fn notifications_page(State(state): State<Arc<AppState>>) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let notifications = state.notifications.all().await;
    let ctx = app_ctx!(state, "notifications", location_name.as_str(), &areas,
        notifications => Value::from_serialize(&notifications),
    );
    render_template(&state, "notifications.html", ctx)
}

async fn system_page(State(state): State<Arc<AppState>>) -> Response {
    let mode = format!("{:?}", state.core.runtime_mode());
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "system", location_name.as_str(), &areas,
        version      => env!("CARGO_PKG_VERSION"),
        runtime_mode => mode.as_str(),
    );
    render_template(&state, "system.html", ctx)
}

async fn areas_page(State(state): State<Arc<AppState>>) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let zones = load_zones(&state).await;
    let ctx = app_ctx!(state, "areas", location_name.as_str(), &areas,
        zones     => Value::from_serialize(&zones),
        back_url  => "/settings",
        nav_title => "Areas, labels & zones",
    );
    render_template(&state, "areas.html", ctx)
}

#[derive(Deserialize)]
struct AreaCreateForm {
    name: String,
}

async fn areas_create(
    State(state): State<Arc<AppState>>,
    axum::extract::Form(form): axum::extract::Form<AreaCreateForm>,
) -> Response {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return redirect("/areas");
    }
    if let Err(err) = state.area_registry.create(name).await {
        return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"));
    }
    redirect("/areas")
}

async fn area_delete(
    State(state): State<Arc<AppState>>,
    Path(area_id): Path<String>,
) -> Response {
    if let Err(err) = state.area_registry.delete(&area_id).await {
        return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"));
    }
    redirect("/areas")
}

// ---------------------------------------------------------------------------
// Zone handlers
// Source: homeassistant/components/zone/__init__.py
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ZoneCreateForm {
    name: String,
}

async fn zones_create(
    State(state): State<Arc<AppState>>,
    axum::extract::Form(form): axum::extract::Form<ZoneCreateForm>,
) -> Response {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return redirect("/areas");
    }
    match state.zone_store.create(name, None, None, None, None, None).await {
        Ok(zone) => {
            // Redirect to edit page so the user can set coordinates immediately.
            let path = format!("/zones/{}", zone.zone_id);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::LOCATION,
                HeaderValue::from_str(&path).unwrap_or_else(|_| HeaderValue::from_static("/areas")),
            );
            (StatusCode::SEE_OTHER, headers).into_response()
        }
        Err(err) => page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

async fn zone_edit_page(
    State(state): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> Response {
    let zone = match state.zone_store.list().await {
        Ok(list) => list.into_iter().find(|z| z.zone_id == zone_id),
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let zone = match zone {
        Some(z) => z,
        None => return page_error(StatusCode::NOT_FOUND, "Zone not found"),
    };
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "areas", location_name.as_str(), &areas,
        zones     => Value::from_serialize(&load_zones(&state).await),
        zone      => Value::from_serialize(&zone),
        saved     => false,
        back_url  => "/areas",
        nav_title => "Edit Zone",
    );
    render_template(&state, "zone_edit.html", ctx)
}

#[derive(Deserialize)]
struct ZoneUpdateForm {
    name: String,
    // Empty string when the field is left blank; parsed as Option<f64> manually.
    #[serde(default)]
    latitude: String,
    #[serde(default)]
    longitude: String,
    #[serde(default)]
    radius: String,
    // Checkboxes are only present when checked.
    passive: Option<String>,
    #[serde(default)]
    icon: String,
}

async fn zone_update(
    State(state): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    axum::extract::Form(form): axum::extract::Form<ZoneUpdateForm>,
) -> Response {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return redirect("/areas");
    }
    // Empty field → don't touch the stored value (pass None = "leave unchanged").
    // Non-empty field → parse and set (Some(Some(val)) = "set to val").
    let latitude = if form.latitude.trim().is_empty() {
        None
    } else {
        Some(form.latitude.trim().parse::<f64>().ok())
    };
    let longitude = if form.longitude.trim().is_empty() {
        None
    } else {
        Some(form.longitude.trim().parse::<f64>().ok())
    };
    let radius = form.radius.trim().parse::<f64>().ok();
    let passive = Some(form.passive.is_some());
    let icon_s = form.icon.trim();
    let icon = Some(if icon_s.is_empty() { None } else { Some(icon_s.to_string()) });

    match state.zone_store.update(&zone_id, Some(name), latitude, longitude, radius, passive, icon).await {
        Ok(Some(zone)) => {
            let location_name = load_location_name(&state).await;
            let areas = load_areas(&state).await;
            let ctx = app_ctx!(state, "areas", location_name.as_str(), &areas,
                zones     => Value::from_serialize(&load_zones(&state).await),
                zone      => Value::from_serialize(&zone),
                saved     => true,
                back_url  => "/areas",
                nav_title => "Edit Zone",
            );
            render_template(&state, "zone_edit.html", ctx)
        }
        Ok(None) => page_error(StatusCode::NOT_FOUND, "Zone not found"),
        Err(err) => page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

async fn zone_delete(
    State(state): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> Response {
    // zone.home is synthetic; guard explicitly for a clear error rather than
    // silently returning false from the store.
    if zone_id == "home" {
        return page_error(StatusCode::FORBIDDEN, "zone.home cannot be deleted");
    }
    if let Err(err) = state.zone_store.delete(&zone_id).await {
        return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"));
    }
    redirect("/areas")
}

async fn area_detail_page(
    State(state): State<Arc<AppState>>,
    Path(area_id): Path<String>,
) -> Response {
    let area = match state.area_registry.list().await {
        Ok(list) => list.into_iter().find(|a| a.area_id == area_id),
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let area = match area {
        Some(a) => a,
        None => return page_error(StatusCode::NOT_FOUND, "Area not found"),
    };
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let all_views = match collect_all_entity_views(&state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let all_cards = build_area_cards(&state, &all_views).await;
    let area_cards: Vec<AreaCard> = all_cards.into_iter()
        .filter(|c| c.area_name == area.name)
        .collect();
    let active_key = format!("area:{}", area_id);
    let ctx = app_ctx!(state, active_key.as_str(), location_name.as_str(), &areas,
        area       => Value::from_serialize(&area),
        area_cards => Value::from_serialize(&area_cards),
    );
    render_template(&state, "area_detail.html", ctx)
}

async fn fragment_area_sensors(
    State(state): State<Arc<AppState>>,
    Path(area_id): Path<String>,
) -> Response {
    let area = match state.area_registry.list().await {
        Ok(list) => list.into_iter().find(|a| a.area_id == area_id),
        Err(err) => return internal_error(&err),
    };
    let area = match area {
        Some(a) => a,
        None => return (StatusCode::NOT_FOUND, Html("<p>Area not found</p>".to_string())).into_response(),
    };
    let all_views = match collect_all_entity_views(&state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let all_cards = build_area_cards(&state, &all_views).await;
    let area_cards: Vec<AreaCard> = all_cards.into_iter()
        .filter(|c| c.area_name == area.name)
        .collect();
    let ctx = context! {
        area_cards => Value::from_serialize(&area_cards),
    };
    render_template(&state, "fragments/sensors.html", ctx)
}

async fn device_detail_page(
    State(state): State<Arc<AppState>>,
    Path(webhook_id): Path<String>,
) -> Response {
    let device = match state.mobile_devices.get_by_webhook_id(&webhook_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return page_error(StatusCode::NOT_FOUND, "Device not found"),
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let raw_entities = match state.mobile_entities.list_by_webhook_id(&webhook_id).await {
        Ok(e) => e,
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let entities: Vec<_> = raw_entities
        .iter()
        .map(|e| entity_to_view(e, &state))
        .collect();
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "devices", location_name.as_str(), &areas,
        device   => Value::from_serialize(&device),
        entities => Value::from_serialize(&entities),
    );
    render_template(&state, "device_detail.html", ctx)
}

async fn entity_edit_page(
    State(state): State<Arc<AppState>>,
    Path((webhook_id, entity_id)): Path<(String, String)>,
    Query(params): Query<EntityEditQuery>,
) -> Response {
    let device = match state.mobile_devices.get_by_webhook_id(&webhook_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return page_error(StatusCode::NOT_FOUND, "Device not found"),
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let entity_record = match state.mobile_entities.get_by_entity_id(&entity_id).await {
        Ok(Some(e)) => e,
        Ok(None) => return page_error(StatusCode::NOT_FOUND, "Entity not found"),
        Err(err) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let history = state.history.last_n(&entity_id, 100).await;
    let sparkline = if history.len() >= 2 {
        Some(history_store::render_sparkline(&history, 320, 60))
    } else {
        None
    };
    let entity_view = entity_to_view(&entity_record, &state);
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let ctx = app_ctx!(state, "devices", location_name.as_str(), &areas,
        device        => Value::from_serialize(&device),
        entity        => Value::from_serialize(&entity_view),
        saved         => params.saved.unwrap_or(false),
        sparkline     => sparkline,
        history_count => history.len(),
    );
    render_template(&state, "entity_edit.html", ctx)
}

#[derive(Deserialize)]
struct EntityEditQuery {
    saved: Option<bool>,
}

#[derive(Deserialize)]
struct EntityEditForm {
    display_name: String,
    area_id: String,
    unit_override: String,
    disabled: Option<String>,
}

async fn entity_edit_save(
    State(state): State<Arc<AppState>>,
    Path((webhook_id, entity_id)): Path<(String, String)>,
    axum::extract::Form(form): axum::extract::Form<EntityEditForm>,
) -> Response {
    let update = EntityMetaUpdate {
        name_by_user: if form.display_name.trim().is_empty() {
            None
        } else {
            Some(form.display_name.trim().to_string())
        },
        user_area_id: if form.area_id.is_empty() {
            Some(None)
        } else {
            Some(Some(form.area_id.clone()))
        },
        unit_of_measurement: if form.unit_override.trim().is_empty() {
            None
        } else {
            Some(Some(form.unit_override.trim().to_string()))
        },
        disabled: Some(form.disabled.as_deref() == Some("true")),
    };
    match state.mobile_entities.update_meta(&entity_id, update).await {
        Ok(_) => {
            let location = format!("/devices/{webhook_id}/entities/{entity_id}?saved=true");
            let mut headers = HeaderMap::new();
            headers.insert(
                header::LOCATION,
                HeaderValue::try_from(location).unwrap(),
            );
            (StatusCode::SEE_OTHER, headers).into_response()
        }
        Err(err) => page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

// ---------------------------------------------------------------------------
// HTMX fragment handlers
// ---------------------------------------------------------------------------

async fn fragment_dashboard_sensors(State(state): State<Arc<AppState>>) -> Response {
    let all_views = match collect_all_entity_views(&state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let area_cards = build_area_cards(&state, &all_views).await;
    let ctx = context! { area_cards => Value::from_serialize(&area_cards) };
    render_template(&state, "fragments/sensors.html", ctx)
}

/// Resolve an entity into an `(EntityView, entity_type)` pair by querying each
/// registered backend in priority order.  Returns `Ok(None)` when no backend
/// owns the entity, `Err(Response)` on a storage error.
///
/// # Extension point
/// Add new entity backends here following the Zigbee example:
/// ```ignore
/// #[cfg(feature = "wifi_sensors")]
/// if let Some(v) = state.wifi_sensor_entities.get(entity_id).await
///     .map_err(|e| internal_error(&e))?
/// {
///     return Ok(Some((crate::wifi_sensor_store::entity_view_for(&v, &state.states), v.domain)));
/// }
/// ```
async fn fetch_entity_view(
    entity_id: &str,
    state: &AppState,
) -> Result<Option<(EntityView, String)>, Response> {
    // ── Mobile (webhook-registered) entities ─────────────────────────────
    if let Some(e) = state
        .mobile_entities
        .get_by_entity_id(entity_id)
        .await
        .map_err(|e| internal_error(&e))?
    {
        return Ok(Some((entity_to_view(&e, state), e.entity_type)));
    }

    // ── Zigbee entities ───────────────────────────────────────────────────
    #[cfg(feature = "zigbee")]
    if let Some(ze) = state
        .zigbee_entities
        .get(entity_id)
        .await
        .map_err(|e| internal_error(&e))?
    {
        return Ok(Some((
            crate::zigbee_integration::entity_view_for(&ze, &state.states),
            ze.domain,
        )));
    }

    Ok(None)
}

async fn fragment_more_info(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
) -> Response {
    let (view, entity_type) = match fetch_entity_view(&entity_id, &state).await {
        Err(resp)      => return resp,
        Ok(None)       => return (StatusCode::NOT_FOUND, Html("<p class='text-muted'>Entity not found</p>".to_string())).into_response(),
        Ok(Some(pair)) => pair,
    };
    let history = state.history.last_n(&entity_id, 20).await;
    let template_name = match entity_type.as_str() {
        "light"         => "more_info/_light.html",
        "switch"        => "more_info/_switch.html",
        "cover"         => "more_info/_cover.html",
        "lock"          => "more_info/_lock.html",
        "fan"           => "more_info/_fan.html",
        "sensor"        => "more_info/_sensor.html",
        "binary_sensor" => "more_info/_binary_sensor.html",
        "button"        => "more_info/_button.html",
        "scene"         => "more_info/_scene.html",
        "script"        => "more_info/_script.html",
        "select"        => "more_info/_select.html",
        "climate"       => "more_info/_climate.html",
        _               => "more_info/_default.html",
    };
    let sparkline: Option<String> = if entity_type == "sensor" && history.len() >= 2 {
        Some(crate::history_store::render_sparkline(&history, 300, 56))
    } else {
        None
    };
    let ctx = context! {
        entity    => Value::from_serialize(&view),
        history   => Value::from_serialize(&history),
        sparkline => sparkline,
    };
    render_template(&state, template_name, ctx)
}

/// Service variants for the `persistent_notification` domain.
/// Source: homeassistant/components/persistent_notification/__init__.py
enum PersistentNotificationService {
    Create,
    Dismiss,
}

impl std::str::FromStr for PersistentNotificationService {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "create" => Ok(Self::Create),
            "dismiss" => Ok(Self::Dismiss),
            _ => Err(()),
        }
    }
}

#[derive(Deserialize, Default)]
struct UiServiceForm {
    entity_id: String,
    #[serde(default)]
    brightness: Option<String>,
    #[serde(default)]
    color_temp_kelvin: Option<String>,
    #[serde(default)]
    option: Option<String>,
    #[serde(default)]
    position: Option<String>,
    #[serde(default)]
    hvac_mode: Option<String>,
    #[serde(default)]
    temperature: Option<String>,
    /// Fan speed percentage (0–100).
    /// Source: homeassistant/components/fan/__init__.py ATTR_PERCENTAGE
    #[serde(default)]
    percentage: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    notification_id: Option<String>,
}

async fn ui_service_call(
    State(state): State<Arc<AppState>>,
    Path((domain, service)): Path<(String, String)>,
    axum::extract::Form(form): axum::extract::Form<UiServiceForm>,
) -> Response {
    // Handle persistent_notification domain directly (needs NotificationStore)
    if domain.as_str() == "persistent_notification" {
        match service.parse::<PersistentNotificationService>() {
            Ok(PersistentNotificationService::Create) => {
                let message = form.message.clone().unwrap_or_default();
                if message.is_empty() {
                    return (StatusCode::BAD_REQUEST, "message required").into_response();
                }
                state
                    .notifications
                    .create(message, form.title.clone(), form.notification_id.clone())
                    .await;
                return StatusCode::NO_CONTENT.into_response();
            }
            Ok(PersistentNotificationService::Dismiss) => {
                let id = form
                    .notification_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or_else(|| Some(form.entity_id.as_str()).filter(|s| !s.is_empty()))
                    .unwrap_or("");
                if id.is_empty() {
                    return (StatusCode::BAD_REQUEST, "notification_id required").into_response();
                }
                state.notifications.dismiss(id).await;
                return StatusCode::NO_CONTENT.into_response();
            }
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        }
    }
    let mut data: Map<String, serde_json::Value> = Map::new();
    data.insert("entity_id".to_string(), serde_json::Value::String(form.entity_id));
    if let Some(b) = form.brightness.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(n) = b.parse::<i64>() { data.insert("brightness".into(), json!(n)); }
    }
    if let Some(c) = form.color_temp_kelvin.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(n) = c.parse::<i64>() { data.insert("color_temp_kelvin".into(), json!(n)); }
    }
    if let Some(o) = form.option.as_deref().filter(|s| !s.is_empty()) {
        data.insert("option".into(), json!(o));
    }
    if let Some(p) = form.position.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(n) = p.parse::<u8>() { data.insert("position".into(), json!(n)); }
    }
    if let Some(m) = form.hvac_mode.as_deref().filter(|s| !s.is_empty()) {
        data.insert("hvac_mode".into(), serde_json::json!(m));
    }
    if let Some(t) = form.temperature.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(f) = t.parse::<f64>() { data.insert("temperature".into(), serde_json::json!(f)); }
    }
    if let Some(pct) = form.percentage.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(n) = pct.parse::<u8>() { data.insert("percentage".into(), json!(n)); }
    }
    let target = match ServiceTarget::from_parts(None, Some(&data)) {
        Ok(t) => t,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let service_data = ServiceData::from_json(&data).unwrap_or_default();
    let meta = OperationMeta {
        request_id: 0,
        consistency: Consistency::LivePreferred,
        deadline: DeadlineClass::Interactive,
        allow_cached: true,
        allow_deferred: false,
    };
    match state.core.execute(
        CoreDeps { config: &state.config, states: &state.states, services: &state.services },
        OperationRequest::CallService {
            call: ServiceCall { domain, service, target, data: service_data, return_response: false },
            meta,
        },
    ) {
        OperationResult::ServiceCallCompleted(_) => StatusCode::NO_CONTENT.into_response(),
        OperationResult::Error(OperationError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Mutation API handlers (HTMX targets)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeviceRenameForm {
    device_name: String,
}

async fn api_device_rename(
    State(state): State<Arc<AppState>>,
    Path(webhook_id): Path<String>,
    axum::extract::Form(form): axum::extract::Form<DeviceRenameForm>,
) -> Response {
    let name = form.device_name.trim().to_string();
    if name.is_empty() {
        return Html(
            "<span style='color:#9b1c1c'>Name cannot be empty</span>".to_string(),
        )
        .into_response();
    }
    match state.mobile_devices.rename(&webhook_id, &name).await {
        Ok(true) => Html(
            "<span class='badge badge-success' style='padding:6px 12px'>✓ Saved</span>"
                .to_string(),
        )
        .into_response(),
        Ok(false) => Html(
            "<span style='color:#9b1c1c'>Device not found</span>".to_string(),
        )
        .into_response(),
        Err(err) => internal_error(&err),
    }
}

// ---------------------------------------------------------------------------
// BLE stub endpoints
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct BleDevice {
    name: String,
    rssi: i32,
}

async fn api_ble_scan(State(state): State<Arc<AppState>>) -> Response {
    let fake_devices = vec![
        BleDevice { name: "HA Sensor A3F2".into(), rssi: -62 },
        BleDevice { name: "HA Sensor B8D1".into(), rssi: -77 },
    ];
    let ctx = context! { ble_devices => Value::from_serialize(&fake_devices) };
    render_template(&state, "fragments/ble_results.html", ctx)
}

#[derive(Deserialize)]
struct BlePairForm {
    name: String,
}

async fn api_ble_pair(
    axum::extract::Form(form): axum::extract::Form<BlePairForm>,
) -> Response {
    let fake_webhook = uuid::Uuid::new_v4().simple().to_string();
    Html(format!(
        "<div class='ble-device-row' style='background:#dcfce7'>\
         <svg width='22' height='22' style='color:#16a34a;fill:currentColor'><use href='#icon-check'/></svg>\
         <span class='ble-device-name'>{} paired</span>\
         <code style='font-size:.75rem;color:#5a6778'>{}</code>\
         </div>",
        html_escape_str(&form.name),
        &fake_webhook[..8],
    ))
    .into_response()
}

fn html_escape_str(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// History endpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    last: Option<usize>,
}

async fn api_history(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
    Query(params): Query<HistoryQuery>,
) -> Response {
    let n = params.last.unwrap_or(100).min(1000);
    let entries = state.history.last_n(&entity_id, n).await;
    Json(entries).into_response()
}

// ---------------------------------------------------------------------------
// Health + onboarding REST API (unchanged business logic)
// ---------------------------------------------------------------------------

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok", milestone: "m0" })
}

/// Exit code that signals systemd to restart the process.
/// Source: homeassistant/const.py  RESTART_EXIT_CODE: Final = 100
const RESTART_EXIT_CODE: i32 = 100;

/// POST /api/system/restart — exit with code 100 so systemd restarts the process.
///
/// Exit code 100 matches HA's RESTART_EXIT_CODE. The 100ms delay lets the 204
/// response flush over TCP before the process terminates. Systemd's
/// Restart=on-failure policy catches exit code 100 and re-launches.
///
/// Note: std::process::exit skips Drop impls. All stores are in-memory only
/// (no pending async writes at shutdown), so data loss risk is minimal.
async fn api_system_restart() -> Response {
    // Spawn a task to exit after the response has flushed.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        std::process::exit(RESTART_EXIT_CODE);
    });
    StatusCode::NO_CONTENT.into_response()
}

async fn onboarding_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.core.onboarding_progress(&state.storage).await {
        Ok(progress) => Json(vec![
            json!({"step": STEP_USER,        "done": progress.user_done}),
            json!({"step": STEP_CORE_CONFIG, "done": progress.core_config_done}),
            json!({"step": STEP_ANALYTICS,   "done": progress.analytics_done}),
            json!({"step": STEP_INTEGRATION, "done": progress.integration_done}),
        ])
        .into_response(),
        Err(err) => internal_error(&err),
    }
}

async fn onboarding_installation_type(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.core.onboarding_progress(&state.storage).await {
        Ok(progress) if progress.onboarded => StatusCode::UNAUTHORIZED.into_response(),
        Ok(_) => Json(json!({"installation_type": "Home Edge"})).into_response(),
        Err(err) => internal_error(&err),
    }
}

#[derive(Debug, Deserialize)]
struct OnboardingUserRequest {
    client_id: String,
    name: String,
    username: String,
    password: String,
    language: String,
}

async fn create_onboarding_user(
    State(state): State<Arc<AppState>>,
    body: Json<OnboardingUserRequest>,
) -> impl IntoResponse {
    if [
        body.client_id.as_str(),
        body.name.as_str(),
        body.username.as_str(),
        body.password.as_str(),
        body.language.as_str(),
    ]
    .iter()
    .any(|value| value.is_empty())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "missing required onboarding user fields".into(),
            )),
        )
            .into_response();
    }
    match state
        .core
        .create_onboarding_user(
            &state.storage,
            &state.auth,
            &OnboardingUserInput {
                name: body.name.clone(),
                username: body.username.clone(),
                password: body.password.clone(),
                language: body.language.clone(),
            },
        )
        .await
    {
        Ok(CreateOnboardingUserOutcome::Created) => {
            let auth_code = state.tokens.issue_auth_code(&body.client_id).await;
            (StatusCode::OK, Json(json!({"auth_code": auth_code}))).into_response()
        }
        Ok(CreateOnboardingUserOutcome::UserStepAlreadyDone) => forbidden("User step already done"),
        Err(err) => internal_error(&err),
    }
}

#[derive(Debug, Default, Deserialize)]
struct CoreConfigRequest {
    location_name: Option<String>,
    country: Option<String>,
    language: Option<String>,
    time_zone: Option<String>,
    unit_system: Option<String>,
}

async fn complete_core_config(
    State(state): State<Arc<AppState>>,
    body: Option<Json<CoreConfigRequest>>,
) -> impl IntoResponse {
    let request = body.map(|json| json.0).unwrap_or_default();
    match state
        .core
        .complete_onboarding_core_config(
            &state.storage,
            &OnboardingCoreConfigInput {
                location_name: request.location_name,
                country: request.country,
                language: request.language,
                time_zone: request.time_zone,
                unit_system: request.unit_system,
            },
        )
        .await
    {
        Ok(CompleteCoreConfigOutcome::Completed) => {
            (StatusCode::OK, Json(json!({}))).into_response()
        }
        Ok(CompleteCoreConfigOutcome::CoreConfigStepAlreadyDone) => {
            forbidden("Core config step already done")
        }
        Ok(CompleteCoreConfigOutcome::UserStepRequired) => {
            forbidden("User step must be completed first")
        }
        Err(err) => internal_error(&err),
    }
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.to_string())
}

#[derive(Debug, Deserialize)]
struct IntegrationRequest {
    client_id: String,
    redirect_uri: String,
}

async fn complete_analytics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => return unauthorized("Missing or invalid Bearer token"),
    };
    if state.tokens.validate_access_token(&token).await.is_none() {
        return unauthorized("Invalid access token");
    }
    match state.core.complete_onboarding_analytics(&state.storage).await {
        Ok(CompleteAnalyticsOutcome::Completed) => {
            (StatusCode::OK, Json(json!({}))).into_response()
        }
        Ok(CompleteAnalyticsOutcome::AlreadyDone) => forbidden("Analytics step already done"),
        Err(err) => internal_error(&err),
    }
}

async fn complete_integration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Json<IntegrationRequest>,
) -> impl IntoResponse {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => return unauthorized("Missing or invalid Bearer token"),
    };
    if state.tokens.validate_access_token(&token).await.is_none() {
        return unauthorized("Invalid access token");
    }
    if body.client_id.is_empty() || body.redirect_uri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "client_id and redirect_uri are required".into(),
            )),
        )
            .into_response();
    }
    match state
        .core
        .complete_onboarding_integration(&state.storage)
        .await
    {
        Ok(CompleteIntegrationOutcome::Completed) => {
            let auth_code = state.tokens.issue_auth_code(&body.client_id).await;
            (StatusCode::OK, Json(json!({"auth_code": auth_code}))).into_response()
        }
        Ok(CompleteIntegrationOutcome::AlreadyDone) => forbidden("Integration step already done"),
        Err(err) => internal_error(&err),
    }
}

/// POST /api/onboarding/integration/wait
///
/// Source: homeassistant/components/onboarding/views.py  IntegrationWaitView.post
/// Polls completion of an asynchronously-loaded integration. The HA frontend calls
/// this after the integration step to block until the integration is fully loaded.
///
/// On home-edge there is no async integration loading, so we always report
/// integration_loaded: true immediately.
async fn onboarding_integration_wait(body: Option<Json<serde_json::Value>>) -> impl IntoResponse {
    let _ = body; // domain name accepted but unused — no async loading on embedded device
    (StatusCode::OK, Json(json!({"integration_loaded": true}))).into_response()
}

async fn complete_onboarding(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.core.complete_onboarding(&state.storage).await {
        Ok(next) => Json(next).into_response(),
        Err(err) => internal_error(&err),
    }
}

// ---------------------------------------------------------------------------
// Entity / device view helpers
// ---------------------------------------------------------------------------
//
// EntityView and AreaCard live in crate::entity_view (backend-agnostic).
// Each backend module owns its own `entity_view_for(record, states)` function:
//   • mobile   → entity_to_view()           (below, uses MobileEntityRecord)
//   • Zigbee   → zigbee_integration::entity_view_for()  (zigbee_integration.rs)
//   • <future> → <backend_module>::entity_view_for()    (see entity_view.rs docs)

fn entity_to_view(entity: &MobileEntityRecord, state: &AppState) -> EntityView {
    let value = state
        .states
        .get(&entity.entity_id)
        .map(|s| s.state.clone())
        .unwrap_or_else(|| "unavailable".to_string());
    let attrs = state
        .states
        .get(&entity.entity_id)
        .map(|s| s.attributes)
        .unwrap_or_default();
    let current_temperature = attrs.get("current_temperature").and_then(|v| v.as_f64());
    let target_temperature = attrs.get("temperature").and_then(|v| v.as_f64());
    let hvac_modes = attrs
        .get("hvac_modes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let brightness = attrs
        .get("brightness")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(255) as u8);
    let color_temp_kelvin = attrs
        .get("color_temp_kelvin")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16);
    // Source: homeassistant/components/light/const.py DEFAULT_MIN_KELVIN=2000, DEFAULT_MAX_KELVIN=6535
    let min_color_temp_kelvin = attrs
        .get("min_color_temp_kelvin")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .unwrap_or(2000);
    let max_color_temp_kelvin = attrs
        .get("max_color_temp_kelvin")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .unwrap_or(6535);
    let options: Vec<String> = attrs
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let current_position = attrs
        .get("current_position")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(100) as u8);
    // Source: homeassistant/components/fan/__init__.py ATTR_PERCENTAGE
    let fan_percentage = attrs
        .get("percentage")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(100) as u8);
    EntityView {
        entity_id: entity.entity_id.clone(),
        webhook_id: Some(entity.webhook_id.clone()),
        display_name: entity.display_name().to_string(),
        entity_type: entity.entity_type.clone(),
        icon_name: entity_icon_name_with_state(entity, &value).to_owned(),
        value,
        unit: entity.unit_of_measurement.clone().unwrap_or_default(),
        device_class: entity.device_class.clone().unwrap_or_default(),
        user_area_id: entity.user_area_id.clone().unwrap_or_default(),
        unit_of_measurement: entity.unit_of_measurement.clone(),
        disabled: entity.disabled,
        service_action: service_action_for(&entity.entity_type).to_owned(),
        current_temperature,
        target_temperature,
        hvac_modes,
        brightness,
        color_temp_kelvin,
        min_color_temp_kelvin,
        max_color_temp_kelvin,
        options,
        current_position,
        fan_percentage,
    }
}

/// Returns the HA service action string for a given entity type.
fn service_action_for(entity_type: &str) -> &'static str {
    match entity_type {
        "light" | "switch" | "fan" => "toggle",
        "button"                   => "press",
        "scene"                    => "activate",
        "script"                   => "trigger",
        "climate"                  => "",
        _                          => "",
    }
}

/// Returns the icon name appropriate for an entity's current state.
fn entity_icon_name_with_state(entity: &MobileEntityRecord, value: &str) -> &'static str {
    match entity.entity_type.as_str() {
        "light"   => "lightbulb",
        "switch"  => if value == "on" { "toggle-switch" } else { "toggle-switch-off" },
        "cover"   => if value == "open" { "window-shutter-open" } else { "window-shutter" },
        "lock"    => if value == "unlocked" { "lock-open" } else { "lock" },
        "fan"     => "fan",
        "button"  => "power",
        "scene"   => "palette",
        "script"  => "script-text",
        "select"  => "format-list",
        "climate" => "thermometer",
        _         => entity_icon_name(entity),
    }
}

/// Collect `EntityView`s from every registered backend.
///
/// Mobile (webhook) entities are always included.  Zigbee entities are merged
/// in when the `zigbee` feature is compiled in.  Each additional backend
/// should add one arm following the Zigbee pattern.
async fn collect_all_entity_views(state: &AppState) -> Result<Vec<EntityView>, Response> {
    let mobile = state
        .mobile_entities
        .all()
        .await
        .map_err(|e| internal_error(&e))?;
    let mut views: Vec<EntityView> = mobile.iter().map(|e| entity_to_view(e, state)).collect();

    #[cfg(feature = "zigbee")]
    {
        let zigbee = state
            .zigbee_entities
            .list()
            .await
            .unwrap_or_default();
        for ze in &zigbee {
            views.push(crate::zigbee_integration::entity_view_for(ze, &state.states));
        }
    }

    Ok(views)
}

async fn build_area_cards(state: &AppState, all_views: &[EntityView]) -> Vec<AreaCard> {
    let areas = state.area_registry.list().await.unwrap_or_default();
    let area_name_map: std::collections::HashMap<&str, &str> = areas
        .iter()
        .map(|a| (a.area_id.as_str(), a.name.as_str()))
        .collect();

    let mut area_map: std::collections::HashMap<String, Vec<EntityView>> =
        std::collections::HashMap::new();
    for entity in all_views {
        if entity.disabled {
            continue;
        }
        let area_name = if entity.user_area_id.is_empty() {
            "Unassigned".to_string()
        } else {
            area_name_map
                .get(entity.user_area_id.as_str())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "Unassigned".to_string())
        };
        area_map
            .entry(area_name)
            .or_default()
            .push(entity.clone());
    }

    let mut cards: Vec<AreaCard> = area_map
        .into_iter()
        .map(|(area_name, entities)| AreaCard { area_name, entities })
        .collect();
    // Named areas sort alphabetically; "Unassigned" goes last.
    cards.sort_by(|a, b| match (a.area_name.as_str(), b.area_name.as_str()) {
        ("Unassigned", _) => std::cmp::Ordering::Greater,
        (_, "Unassigned") => std::cmp::Ordering::Less,
        _                 => a.area_name.cmp(&b.area_name),
    });
    cards
}

#[derive(Serialize)]
struct DeviceEntityGroup {
    webhook_id: String,
    device_name: String,
    entities: Vec<EntityView>,
}

fn build_entity_groups(
    devices: &[crate::mobile_device_store::MobileDeviceRecord],
    all_entities: &[MobileEntityRecord],
    state: &AppState,
) -> Vec<DeviceEntityGroup> {
    devices
        .iter()
        .map(|d| {
            let entities: Vec<EntityView> = all_entities
                .iter()
                .filter(|e| e.webhook_id == d.webhook_id)
                .map(|e| entity_to_view(e, state))
                .collect();
            DeviceEntityGroup {
                webhook_id: d.webhook_id.clone(),
                device_name: d.display_name().to_string(),
                entities,
            }
        })
        .filter(|g| !g.entities.is_empty())
        .collect()
}

fn entity_icon_name(entity: &MobileEntityRecord) -> &'static str {
    if let Some(mdi) = entity.icon.as_deref() {
        let key = mdi.strip_prefix("mdi:").unwrap_or(mdi);
        match key {
            "battery" | "battery-high" | "battery-medium" | "battery-low"
            | "battery-charging" => return "battery",
            "thermometer" | "temperature-celsius" | "temperature-fahrenheit" => {
                return "thermometer"
            }
            "water" | "water-percent" | "water-drop" => return "water",
            "flash" | "lightning-bolt" | "power" | "power-plug" => return "lightning",
            "toggle-switch" | "toggle-switch-off" => return "toggle",
            "bluetooth" => return "bluetooth",
            _ => {}
        }
    }
    match entity.device_class.as_deref() {
        Some("battery") => "battery",
        Some("temperature") => "thermometer",
        Some("humidity") | Some("moisture") => "water",
        Some("power") | Some("energy") | Some("current") | Some("voltage") => "lightning",
        _ if entity.entity_type == "binary_sensor" => "toggle",
        _ => "sensor",
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    milestone: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

impl ErrorResponse {
    pub fn new(error: String) -> Self {
        Self { error }
    }
}

// ---------------------------------------------------------------------------
// Zigbee REST API handlers
// ---------------------------------------------------------------------------

#[cfg(feature = "zigbee")]
use crate::zigbee_device_store::ZigbeeDeviceMetaUpdate;

#[cfg(feature = "zigbee")]
#[derive(Debug, Serialize)]
struct ZigbeeDeviceResponse {
    ieee_addr: String,
    friendly_name: String,
    display_name: String,
    manufacturer: Option<String>,
    model: Option<String>,
    power_source: Option<String>,
    interview_complete: bool,
    last_seen: Option<String>,
    user_area_id: Option<String>,
}

#[cfg(feature = "zigbee")]
async fn api_zigbee_devices(State(state): State<Arc<AppState>>) -> Response {
    match state.zigbee_devices.list().await {
        Ok(devices) => {
            let resp: Vec<ZigbeeDeviceResponse> = devices
                .into_iter()
                .map(|d| ZigbeeDeviceResponse {
                    display_name: d.display_name().to_string(),
                    ieee_addr: d.ieee_addr.clone(),
                    friendly_name: d.friendly_name.clone(),
                    manufacturer: d.manufacturer,
                    model: d.model,
                    power_source: d.power_source,
                    interview_complete: d.interview_complete,
                    last_seen: d.last_seen,
                    user_area_id: d.user_area_id,
                })
                .collect();
            Json(resp).into_response()
        }
        Err(e) => internal_error(&e),
    }
}

/// Form used by `PATCH /api/zigbee/devices/{ieee}` (from both the rename card and the
/// area-picker card on the device detail page).
///
/// Both fields use `#[serde(default)]` so each form only needs to submit the field
/// it cares about; absent fields produce `None` and leave the stored value untouched.
/// An empty string for `user_area_id` is interpreted as "clear the area".
#[cfg(feature = "zigbee")]
#[derive(Debug, Deserialize)]
struct ZigbeeDeviceRenameForm {
    #[serde(default)]
    name_by_user: Option<String>,
    #[serde(default)]
    user_area_id: Option<String>,
}

#[cfg(feature = "zigbee")]
async fn api_zigbee_device_rename(
    State(state): State<Arc<AppState>>,
    Path(ieee): Path<String>,
    axum::extract::Form(form): axum::extract::Form<ZigbeeDeviceRenameForm>,
) -> Response {
    let update = ZigbeeDeviceMetaUpdate {
        // None  → field absent → don't touch stored value
        // Some(None)  → field submitted but empty → clear
        // Some(Some(s)) → field submitted with value → set
        name_by_user: form.name_by_user.map(|s| if s.trim().is_empty() { None } else { Some(s.trim().to_string()) }),
        user_area_id: form.user_area_id.map(|s| if s.is_empty() { None } else { Some(s) }),
    };
    match state.zigbee_devices.update_meta(&ieee, update).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(ErrorResponse::new("device not found".into()))).into_response(),
        Err(e) => internal_error(&e),
    }
}

#[cfg(feature = "zigbee")]
async fn api_zigbee_device_delete(
    State(state): State<Arc<AppState>>,
    Path(ieee): Path<String>,
) -> Response {
    // List entities before removing them so we can clear state store.
    let entity_ids: Vec<String> = state
        .zigbee_entities
        .list_for_device(&ieee)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.entity_id)
        .collect();
    // Remove entity records from the entity store.
    if let Err(e) = state.zigbee_entities.remove_for_device(&ieee).await {
        return internal_error(&e);
    }
    // Clear entity states from the state store (explicit deref to avoid trait ambiguity).
    for entity_id in &entity_ids {
        let _ = (&*state.states).remove(entity_id);
    }
    match state.zigbee_devices.remove(&ieee).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(ErrorResponse::new("device not found".into()))).into_response(),
        Err(e) => internal_error(&e),
    }
}

/// Form used by `PATCH /api/zigbee/entities/{entity_id}` from the area-picker
/// column in the device detail entities table.
#[cfg(feature = "zigbee")]
#[derive(Debug, Deserialize)]
struct ZigbeeEntityUpdateForm {
    #[serde(default)]
    name_by_user: Option<String>,
    #[serde(default)]
    user_area_id: Option<String>,
}

#[cfg(feature = "zigbee")]
async fn api_zigbee_entity_update(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
    axum::extract::Form(form): axum::extract::Form<ZigbeeEntityUpdateForm>,
) -> Response {
    use crate::zigbee_entity_store::ZigbeeEntityMetaUpdate;
    let update = ZigbeeEntityMetaUpdate {
        name_by_user: form.name_by_user.map(|s| if s.trim().is_empty() { None } else { Some(s.trim().to_string()) }),
        user_area_id: form.user_area_id.map(|s| if s.is_empty() { None } else { Some(s) }),
    };
    match state.zigbee_entities.update_meta(&entity_id, update).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(ErrorResponse::new("entity not found".into()))).into_response(),
        Err(e) => internal_error(&e),
    }
}

#[cfg(feature = "zigbee")]
#[derive(Debug, Deserialize)]
struct PermitJoinPayload {
    #[serde(default = "default_permit_join_duration")]
    duration: u8,
}

#[cfg(feature = "zigbee")]
fn default_permit_join_duration() -> u8 { 254 }

#[cfg(feature = "zigbee")]
async fn api_zigbee_permit_join(
    State(state): State<Arc<AppState>>,
    payload: Option<Json<PermitJoinPayload>>,
) -> Response {
    let duration = payload.map(|p| p.duration).unwrap_or(254);
    match &state.zigbee {
        Some(handle) => match handle.permit_join(duration).await {
            Ok(()) => {
                // HTMX clients follow HX-Redirect for a full client-side navigation.
                // Plain browser and API clients follow the 303 Location header.
                axum::response::Response::builder()
                    .status(StatusCode::SEE_OTHER)
                    .header("Location", "/zigbee")
                    .header("HX-Redirect", "/zigbee")
                    .body(axum::body::Body::empty())
                    .unwrap()
                    .into_response()
            }
            Err(e) => internal_error(&e),
        },
        None => (StatusCode::SERVICE_UNAVAILABLE, Json(ErrorResponse::new("zigbee bridge not running".into()))).into_response(),
    }
}

#[cfg(feature = "zigbee")]
async fn api_zigbee_permit_join_stop(State(state): State<Arc<AppState>>) -> Response {
    match &state.zigbee {
        Some(handle) => match handle.permit_join(0).await {
            Ok(()) => {
                axum::response::Response::builder()
                    .status(StatusCode::SEE_OTHER)
                    .header("Location", "/zigbee")
                    .header("HX-Redirect", "/zigbee")
                    .body(axum::body::Body::empty())
                    .unwrap()
                    .into_response()
            }
            Err(e) => internal_error(&e),
        },
        None => (StatusCode::SERVICE_UNAVAILABLE, Json(ErrorResponse::new("zigbee bridge not running".into()))).into_response(),
    }
}

#[cfg(feature = "zigbee")]
async fn api_zigbee_permit_join_status(State(state): State<Arc<AppState>>) -> Response {
    let remaining = state.zigbee
        .as_ref()
        .map(|h| h.permit_join_remaining_secs())
        .unwrap_or(0);
    Json(json!({ "active": remaining > 0, "remaining_secs": remaining })).into_response()
}

// ---------------------------------------------------------------------------
// Zigbee UI page handlers
// ---------------------------------------------------------------------------

#[cfg(feature = "zigbee")]
async fn zigbee_devices_page(State(state): State<Arc<AppState>>) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let devices = match state.zigbee_devices.list().await {
        Ok(d) => d,
        Err(e) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    };
    let bridge_running = state.zigbee.is_some();
    let pairing_remaining_secs: u8 = state.zigbee
        .as_ref()
        .map(|h| h.permit_join_remaining_secs())
        .unwrap_or(0);
    let ctx = app_ctx!(&state, "zigbee", location_name.as_str(), &areas,
        devices               => Value::from_serialize(&devices),
        bridge_running        => bridge_running,
        pairing_remaining_secs => pairing_remaining_secs,
    );
    render_template(&state, "zigbee_devices.html", ctx)
}

/// HTMX fragment that returns just the device list rows for live polling.
#[cfg(feature = "zigbee")]
async fn fragment_zigbee_devices(State(state): State<Arc<AppState>>) -> Response {
    let devices = match state.zigbee_devices.list().await {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Html(format!("{e:#}"))).into_response(),
    };
    let pairing_remaining_secs: u8 = state.zigbee
        .as_ref()
        .map(|h| h.permit_join_remaining_secs())
        .unwrap_or(0);
    let ctx = context! {
        devices               => Value::from_serialize(&devices),
        pairing_remaining_secs => pairing_remaining_secs,
    };
    render_template(&state, "fragments/zigbee_devices.html", ctx)
}

#[cfg(feature = "zigbee")]
async fn zigbee_device_detail_page(
    State(state): State<Arc<AppState>>,
    Path(ieee): Path<String>,
) -> Response {
    let location_name = load_location_name(&state).await;
    let areas = load_areas(&state).await;
    let device = match state.zigbee_devices.get_by_ieee(&ieee).await {
        Ok(Some(d)) => d,
        Ok(None) => return page_error(StatusCode::NOT_FOUND, "Zigbee device not found"),
        Err(e) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    };
    let entity_records = match state.zigbee_entities.list_for_device(&ieee).await {
        Ok(e) => e,
        Err(e) => return page_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    };
    let entities: Vec<EntityView> = entity_records
        .iter()
        .map(|e| crate::zigbee_integration::entity_view_for(e, &state.states))
        .collect();
    let ctx = app_ctx!(&state, "zigbee", location_name.as_str(), &areas,
        device   => Value::from_serialize(&device),
        entities => Value::from_serialize(&entities),
    );
    render_template(&state, "zigbee_device_detail.html", ctx)
}
