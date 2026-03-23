use std::sync::Arc;

use anyhow::anyhow;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppState;
use crate::auth_store::AuthUser;
use crate::ha_api;
use crate::ha_auth;
use crate::ha_mobile;
use crate::ha_webhook;
use crate::ha_ws;
use crate::storage::StoredUser;

const STEP_USER: &str = "user";
const STEP_CORE_CONFIG: &str = "core_config";
const ONBOARDING_STEPS: [&str; 2] = [STEP_USER, STEP_CORE_CONFIG];

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
        .route("/api/onboarding/installation_type", get(onboarding_installation_type))
        .route("/api/onboarding/users", post(create_onboarding_user))
        .route("/api/onboarding/core_config", post(complete_core_config))
        .route("/api/onboarding/complete", post(complete_onboarding))
        .with_state(state)
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    match state.storage.load_onboarding().await {
        Ok(status) if !status.onboarded => {
            let mut headers = HeaderMap::new();
            headers.insert(header::LOCATION, HeaderValue::from_static("/onboarding"));
            (StatusCode::TEMPORARY_REDIRECT, headers).into_response()
        }
        Ok(_) => Html(render_shell(&state.config.ui.product_name, true)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load onboarding state: {err:#}"),
        )
            .into_response(),
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        milestone: "m0",
    })
}

async fn onboarding_page(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.storage.load_onboarding().await {
        Ok(status) => Html(render_shell(
            &state.config.ui.product_name,
            status.onboarded,
        ))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load onboarding state: {err:#}"),
        )
            .into_response(),
    }
}

async fn onboarding_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.storage.load_onboarding().await {
        Ok(status) => Json(
            ONBOARDING_STEPS
                .into_iter()
                .map(|step| json!({"step": step, "done": status.step_done(step)}))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(format!("failed to load state: {err:#}"))),
        )
            .into_response(),
    }
}

async fn onboarding_installation_type(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.storage.load_onboarding().await {
        Ok(status) if status.onboarded => StatusCode::UNAUTHORIZED.into_response(),
        Ok(_) => Json(json!({"installation_type": "Home Edge"})).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(format!("failed to load state: {err:#}"))),
        )
            .into_response(),
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
            Json(ErrorResponse::new("missing required onboarding user fields".into())),
        )
            .into_response();
    }

    match state
        .storage
        .update_onboarding(|current| {
            if current.step_done(STEP_USER) || current.user.is_some() {
                return Err(anyhow!("user step already done"));
            }
            current.user = Some(StoredUser {
                name: body.name.clone(),
                username: body.username.clone(),
                password: body.password.clone(),
                language: body.language.clone(),
            });
            current.language = Some(body.language.clone());
            current.done.push(STEP_USER.into());
            Ok(())
        })
        .await
    {
        Ok(_) => {
            if let Err(err) = state
                .auth
                .save_user(&AuthUser {
                    name: body.name.clone(),
                    username: body.username.clone(),
                    password: body.password.clone(),
                    language: body.language.clone(),
                })
                .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new(format!("failed to persist auth user: {err:#}"))),
                )
                    .into_response();
            }
            let auth_code = state.tokens.issue_auth_code(&body.client_id).await;
            (StatusCode::OK, Json(json!({"auth_code": auth_code}))).into_response()
        }
        Err(err) if err.to_string() == "user step already done" => (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new("User step already done".into())),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(format!("failed to persist user: {err:#}"))),
        )
            .into_response(),
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
        .storage
        .update_onboarding(|current| {
            if current.step_done(STEP_CORE_CONFIG) {
                return Err(anyhow!("core config step already done"));
            }
            if !current.step_done(STEP_USER) {
                return Err(anyhow!("user step required"));
            }
            current.location_name = request.location_name.clone();
            current.country = request.country.clone();
            current.language = request.language.clone().or_else(|| current.language.clone());
            current.time_zone = request.time_zone.clone();
            current.unit_system = request.unit_system.clone();
            current.done.push(STEP_CORE_CONFIG.into());
            current.onboarded = ONBOARDING_STEPS.iter().all(|step| current.step_done(step));
            Ok(())
        })
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({}))).into_response(),
        Err(err) if err.to_string() == "core config step already done" => (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new("Core config step already done".into())),
        )
            .into_response(),
        Err(err) if err.to_string() == "user step required" => (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new("User step must be completed first".into())),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(format!("failed to persist core config: {err:#}"))),
        )
            .into_response(),
    }
}

async fn complete_onboarding(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state
        .storage
        .update_onboarding(|current| {
            current.onboarded = true;
            current.done = ONBOARDING_STEPS.iter().map(|step| step.to_string()).collect();
            Ok(())
        })
        .await
    {
        Ok(next) => Json(next).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(format!(
                "failed to persist state: {err:#}"
            ))),
        )
            .into_response(),
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
