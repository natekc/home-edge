use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppState;
use crate::core::{
    CompleteAnalyticsOutcome, CompleteIntegrationOutcome, CompleteCoreConfigOutcome,
    CreateOnboardingUserOutcome, OnboardingCoreConfigInput, OnboardingUserInput,
};
use crate::ha_api;
use crate::ha_auth;
use crate::ha_mobile;
use crate::ha_webhook;
use crate::ha_ws;

const STEP_USER: &str = "user";
const STEP_CORE_CONFIG: &str = "core_config";
const STEP_ANALYTICS: &str = "analytics";
const STEP_INTEGRATION: &str = "integration";

fn internal_error(err: &anyhow::Error) -> Response {
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

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // HA-compatible REST API surface
        .merge(ha_api::router())
        // HA-compatible auth surface
        .merge(ha_auth::router())
        // HA-compatible WebSocket surface
        .merge(ha_ws::router())
        // HA mobile app registration
        .merge(ha_mobile::router())
        // HA webhook endpoint + MQTT discovery
        .merge(ha_webhook::router())
        // Original shell routes
        .route("/", get(index))
        .route("/onboarding", get(onboarding_page))
        .route("/api/health", get(health))
        .route("/api/onboarding", get(onboarding_status))
        .route(
            "/api/onboarding/installation_type",
            get(onboarding_installation_type),
        )
        .route("/api/onboarding/users", post(create_onboarding_user))
        .route("/api/onboarding/core_config", post(complete_core_config))
        .route("/api/onboarding/analytics", post(complete_analytics))
        .route("/api/onboarding/integration", post(complete_integration))
        .route("/api/onboarding/complete", post(complete_onboarding))
        .with_state(state)
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    match state.core.onboarding_progress(&state.storage).await {
        Ok(progress) if !progress.onboarded => {
            let mut headers = HeaderMap::new();
            headers.insert(header::LOCATION, HeaderValue::from_static("/onboarding"));
            (StatusCode::TEMPORARY_REDIRECT, headers).into_response()
        }
        Ok(_) => Html(render_shell(&state.config.ui.product_name, true)).into_response(),
        Err(err) => internal_error(&err),
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        milestone: "m0",
    })
}

async fn onboarding_page(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.core.onboarding_progress(&state.storage).await {
        Ok(progress) => Html(render_shell(&state.config.ui.product_name, progress.onboarded))
        .into_response(),
        Err(err) => internal_error(&err),
    }
}

async fn onboarding_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.core.onboarding_progress(&state.storage).await {
        Ok(progress) => Json(vec![
            json!({"step": STEP_USER, "done": progress.user_done}),
            json!({"step": STEP_CORE_CONFIG, "done": progress.core_config_done}),
            json!({"step": STEP_ANALYTICS, "done": progress.analytics_done}),
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
        Ok(CompleteCoreConfigOutcome::CoreConfigStepAlreadyDone) => forbidden("Core config step already done"),
        Ok(CompleteCoreConfigOutcome::UserStepRequired) => forbidden("User step must be completed first"),
        Err(err) => internal_error(&err),
    }
}

/// Extract a Bearer token from the Authorization header.
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

/// POST /api/onboarding/analytics
/// Source: homeassistant/components/onboarding/views.py  AnalyticsOnboardingView
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
    match state
        .core
        .complete_onboarding_analytics(&state.storage)
        .await
    {
        Ok(CompleteAnalyticsOutcome::Completed) => {
            (StatusCode::OK, Json(json!({}))).into_response()
        }
        Ok(CompleteAnalyticsOutcome::AlreadyDone) => forbidden("Analytics step already done"),
        Err(err) => internal_error(&err),
    }
}

/// POST /api/onboarding/integration
/// Source: homeassistant/components/onboarding/views.py  IntegrationOnboardingView
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
            Json(ErrorResponse::new("client_id and redirect_uri are required".into())),
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

async fn complete_onboarding(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.core.complete_onboarding(&state.storage).await {
        Ok(next) => Json(next).into_response(),
        Err(err) => internal_error(&err),
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    milestone: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

impl ErrorResponse {
    fn new(error: String) -> Self {
        Self { error }
    }
}

fn render_shell(product_name: &str, onboarded: bool) -> String {
    let title = if onboarded { "Home" } else { "Onboarding" };
    let body = if onboarded {
        "Milestone 0 runtime shell is online. Onboarding has been marked complete, and the system is ready for the next slice of implementation."
    } else {
        "Milestone 0 onboarding shell is online. This page is intentionally small and server-rendered to keep first-load cost low on low-power embedded Linux devices, with Raspberry Pi Zero W as the benchmark baseline."
    };
    let action = if onboarded {
        "<a href=\"/\" style=\"display:inline-block;background:#204030;color:#fff;text-decoration:none;border-radius:10px;padding:.9rem 1.1rem;font-weight:700\">Go to home</a>"
    } else {
        "<form method=\"post\" action=\"/api/onboarding/complete\"><button type=\"submit\">Mark onboarding complete</button></form>"
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{product_name}</title><style>body{{font-family:-apple-system,BlinkMacSystemFont,Segoe UI,sans-serif;margin:0;background:#f2efe8;color:#1d2a2a}}main{{max-width:42rem;margin:5rem auto;padding:2rem}}.card{{background:#fff;border-radius:16px;padding:2rem;box-shadow:0 10px 30px rgba(0,0,0,.08)}}.pill{{display:inline-block;background:#dce9de;color:#204030;padding:.3rem .7rem;border-radius:999px;font-size:.85rem}}h1{{font-size:2.4rem;line-height:1.1;margin:.8rem 0}}p{{font-size:1.05rem;line-height:1.6}}button{{background:#204030;color:#fff;border:0;border-radius:10px;padding:.9rem 1.1rem;font-weight:700;cursor:pointer}}</style></head><body><main><section class=\"card\"><span class=\"pill\">Milestone 0</span><h1>{title}</h1><p>{body}</p>{action}</section></main></body></html>"
    )
}
