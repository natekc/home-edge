use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::Serialize;

use crate::app::AppState;
use crate::ha_api;
use crate::ha_auth;
use crate::ha_mobile;
use crate::ha_ws;
use crate::storage::OnboardingState;

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
        // Original shell routes
        .route("/", get(index))
        .route("/onboarding", get(onboarding_page))
        .route("/api/health", get(health))
        .route("/api/onboarding", get(onboarding_status))
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
        Ok(status) => Json(status).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(format!("failed to load state: {err:#}"))),
        )
            .into_response(),
    }
}

async fn complete_onboarding(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let current = match state.storage.load_onboarding().await {
        Ok(status) => status,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(format!("failed to load state: {err:#}"))),
            )
                .into_response();
        }
    };

    let next = OnboardingState {
        onboarded: true,
        updated_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        ..current
    };

    match state.storage.save_onboarding(&next).await {
        Ok(()) => Json(next).into_response(),
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
        "Milestone 0 onboarding shell is online. This page is intentionally small and server-rendered to keep first-load cost low on Pi Zero W class devices."
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
