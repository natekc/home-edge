//! HA-compatible authentication endpoints.
//!
//! Implements the OAuth2 / IndieAuth authentication flow used by the official
//! Home Assistant app and other HA clients.  Reference Python sources:
//!
//!   GET  /auth/providers            → homeassistant/components/auth/login_flow.py  AuthProvidersView
//!   POST /auth/login_flow           → homeassistant/components/auth/login_flow.py  LoginFlowIndexView
//!   POST /auth/login_flow/{flow_id} → homeassistant/components/auth/login_flow.py  LoginFlowResourceView
//!   POST /auth/token                → homeassistant/components/auth/__init__.py      TokenView
//!   POST /auth/revoke               → homeassistant/components/auth/__init__.py      RevokeTokenView
//!
//! This implementation uses stateless signed tokens (UUID-based for simplicity),
//! suitable for single-user embedded devices.  The protocol surface (error keys,
//! status codes, field names) exactly mirrors the HA Python backend.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::app::AppState;
use crate::core::AuthorizeBootstrapInput;
use crate::storage::save_json_atomic;

const TOKENS_FILE: &str = "tokens.json";

/// Return `opt` trimmed if non-empty, otherwise `fallback`.
fn non_empty<'a>(opt: Option<&'a str>, fallback: &'a str) -> &'a str {
    opt.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(fallback)
}

fn onboarding_required_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "message": "Onboarding not finished",
            "code": "onboarding_required"
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Token store — holds short-lived access tokens and refresh tokens
// ---------------------------------------------------------------------------

/// How long an access token is considered valid (matches `expires_in` returned
/// to clients).  Source: homeassistant/auth/__init__.py ACCESS_TOKEN_EXPIRATION
const ACCESS_TOKEN_TTL_SECS: u64 = 1800;

/// Source: homeassistant/components/auth/__init__.py  AUTH_CODE_EXPIRATION_TIME
const AUTH_CODE_TTL_SECS: u64 = 60;

/// Returns the current UTC time as Unix seconds.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Per-token metadata stored in the persisted access-token map.
#[derive(Clone, Serialize, Deserialize)]
struct PersistedAccessToken {
    client_id: String,
    /// UTC Unix seconds at which the token was issued.
    issued_at: u64,
}

/// On-disk representation — refresh tokens and access tokens survive restarts.
/// Auth codes are intentionally ephemeral (one-time use).
///
/// **Schema migration note:** changing `access_tokens` values from bare `String`
/// (< 0.2.0) to `PersistedAccessToken` objects causes serde to fail the
/// *entire* `PersistedTokens` deserialization when an old file is read;
/// `unwrap_or_default()` then returns empty maps, forcing all sessions to
/// re-authenticate. This is the intended behavior — upgrade = re-login.
#[derive(Default, Serialize, Deserialize)]
struct PersistedTokens {
    /// refresh_token → client_id
    refresh_tokens: HashMap<String, String>,
    /// access_token → metadata (new format; old `String`-valued files are
    /// treated as empty on load, forcing re-login once after upgrade).
    #[serde(default)]
    access_tokens: HashMap<String, PersistedAccessToken>,
}

/// Opaque token registry kept in AppState.
pub struct TokenStore {
    root: PathBuf,
    inner: Mutex<TokenStoreInner>,
}

/// In-memory access token entry.
#[derive(Clone)]
struct AccessTokenEntry {
    client_id: String,
    issued_at: u64,
}

/// One-time authorization code with monotonic issue time.
///
/// Uses `Instant` rather than wall-clock time so the TTL is correct even when
/// the server is not synced to NTP.  Auth codes are never persisted, so
/// process-local monotonic time is sufficient.
struct AuthCodeEntry {
    client_id: String,
    issued_at: std::time::Instant,
}

struct TokenStoreInner {
    /// auth_code → entry (one-time use, expires after AUTH_CODE_TTL_SECS)
    auth_codes: HashMap<String, AuthCodeEntry>,
    /// refresh_token → client_id
    refresh_tokens: HashMap<String, String>,
    /// access_token → entry (client_id + issue timestamp)
    access_tokens: HashMap<String, AccessTokenEntry>,
}

impl TokenStore {
    pub fn new(root: PathBuf) -> Self {
        TokenStore {
            root,
            inner: Mutex::new(TokenStoreInner {
                auth_codes: HashMap::new(),
                refresh_tokens: HashMap::new(),
                access_tokens: HashMap::new(),
            }),
        }
    }

    /// Load persisted tokens from disk into the in-memory store.
    /// Called once at startup in `AppState::new_initialized`.
    pub async fn load_persisted(&self) -> Result<()> {
        let path = self.root.join(TOKENS_FILE);
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let persisted: PersistedTokens =
                    serde_json::from_str(&contents).unwrap_or_default();
                let mut inner = self.inner.lock().await;
                inner.refresh_tokens = persisted.refresh_tokens;
                inner.access_tokens = persisted
                    .access_tokens
                    .into_iter()
                    .map(|(token, entry)| {
                        (token, AccessTokenEntry { client_id: entry.client_id, issued_at: entry.issued_at })
                    })
                    .collect();
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("Token file not found: {}", path.display());
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Persist the current token tables to disk.
    /// Errors are logged but not propagated — callers should not fail on persistence error.
    async fn save_persisted(&self) {
        let persisted = {
            let inner = self.inner.lock().await;
            PersistedTokens {
                refresh_tokens: inner.refresh_tokens.clone(),
                access_tokens: inner
                    .access_tokens
                    .iter()
                    .map(|(token, entry)| {
                        (token.clone(), PersistedAccessToken {
                            client_id: entry.client_id.clone(),
                            issued_at: entry.issued_at,
                        })
                    })
                    .collect(),
            }
        };
        let path = self.root.join(TOKENS_FILE);
        if let Err(e) = save_json_atomic(&path, &persisted).await {
            tracing::warn!("Failed to persist token store: {e:#}");
        }
    }

    /// Issue a one-time authorization code for the given client.
    pub async fn issue_auth_code(&self, client_id: &str) -> String {
        let code = new_token();
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_secs(AUTH_CODE_TTL_SECS);
        let mut inner = self.inner.lock().await;
        // Lazy sweep: remove codes whose age exceeds AUTH_CODE_TTL_SECS.
        inner.auth_codes.retain(|_, entry| now.duration_since(entry.issued_at) <= ttl);
        inner.auth_codes.insert(code.clone(), AuthCodeEntry {
            client_id: client_id.to_string(),
            issued_at: now,
        });
        code
    }

    /// Consume an auth code and, if valid, issue refresh + access tokens.
    /// Returns (access_token, refresh_token) or None if code is invalid.
    pub async fn exchange_code(&self, client_id: &str, code: &str) -> Option<(String, String)> {
        let (access_token, refresh_token) = {
            let mut inner = self.inner.lock().await;
            let entry = inner.auth_codes.remove(code)?;
            // Reject expired codes (HA: invalid_grant).
            let age = entry.issued_at.elapsed();
            if age > std::time::Duration::from_secs(AUTH_CODE_TTL_SECS) {
                tracing::debug!("Auth code expired (age={age:.1?}, ttl={AUTH_CODE_TTL_SECS}s)");
                return None;
            }
            if entry.client_id != client_id {
                tracing::warn!(
                    expected = %entry.client_id,
                    got = %client_id,
                    "client_id mismatch during code exchange"
                );
                return None;
            }
            let refresh_token = new_token();
            let access_token = new_token();
            inner.refresh_tokens.insert(refresh_token.clone(), client_id.to_string());
            inner.access_tokens.insert(access_token.clone(), AccessTokenEntry {
                client_id: client_id.to_string(),
                issued_at: now_unix_secs(),
            });
            (access_token, refresh_token)
        };
        self.save_persisted().await;
        Some((access_token, refresh_token))
    }

    /// Refresh an access token using a refresh token.
    /// Returns a new access_token or None if refresh_token is unknown.
    pub async fn refresh_access_token(
        &self,
        client_id: Option<&str>,
        refresh_token: &str,
    ) -> Option<String> {
        let access_token = {
            let mut inner = self.inner.lock().await;
            let stored_client = inner.refresh_tokens.get(refresh_token)?.clone();
            // Source: TokenView._async_handle_refresh_token
            //   if refresh_token.client_id != client_id: → invalid_request
            // client_id is optional for refresh; only check when provided.
            if let Some(cid) = client_id {
                if stored_client != cid {
                    tracing::warn!("client_id mismatch during token refresh");
                    return None;
                }
            }
            let access_token = new_token();
            inner.access_tokens.insert(access_token.clone(), AccessTokenEntry {
                client_id: stored_client,
                issued_at: now_unix_secs(),
            });
            access_token
        };
        self.save_persisted().await;
        Some(access_token)
    }

    /// Revoke a refresh token and all associated access tokens for its client.
    /// Source: RevokeTokenView — returns 200 regardless of whether token existed.
    pub async fn revoke_refresh_token(&self, token: &str) {
        {
            let mut inner = self.inner.lock().await;
            if let Some(client_id) = inner.refresh_tokens.remove(token) {
                inner.access_tokens.retain(|_, entry| entry.client_id != client_id);
            }
        }
        self.save_persisted().await;
    }

    /// Returns `Some(())` if the token is valid and not yet expired, `None` otherwise.
    ///
    /// Tokens older than `ACCESS_TOKEN_TTL_SECS` (1800 s) are treated as expired and
    /// removed — the client must use its refresh token or re-authenticate.
    ///
    /// Expired entries are removed from memory but not persisted immediately; the
    /// age check (`now - issued_at`) re-applies correctly if the entry is reloaded
    /// from disk after a restart, so no security gap results.
    pub async fn validate_access_token(&self, token: &str) -> Option<()> {
        let mut inner = self.inner.lock().await;
        let entry = inner.access_tokens.get(token)?;
        let age = now_unix_secs().saturating_sub(entry.issued_at);
        if age > ACCESS_TOKEN_TTL_SECS {
            tracing::debug!("Access token expired (age={age}s, ttl={ACCESS_TOKEN_TTL_SECS}s)");
            inner.access_tokens.remove(token);
            return None;
        }
        Some(())
    }

    #[cfg(test)]
    async fn backdate_auth_code(&self, code: &str, age_secs: u64) {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.auth_codes.get_mut(code) {
            // Shift issued_at back in time by subtracting a duration from now.
            entry.issued_at = std::time::Instant::now()
                - std::time::Duration::from_secs(age_secs);
        }
    }

    #[cfg(test)]
    async fn auth_code_count(&self) -> usize {
        self.inner.lock().await.auth_codes.len()
    }

}

// ---------------------------------------------------------------------------
// Login flow state — tracks in-progress login flows
// ---------------------------------------------------------------------------

/// In-progress login flow entry.
#[derive(Clone)]
struct LoginFlow {
    client_id: String,
    /// Whether the flow has been submitted (pending credential step).
    /// None = not started, Some = credentials submitted.
    step: FlowStep,
}

#[derive(Clone)]
enum FlowStep {
    /// Waiting for username/password.
    Form,
    /// Completed successfully.
    Done,
}

pub struct LoginFlowStore {
    inner: RwLock<HashMap<String, LoginFlow>>,
}

impl LoginFlowStore {
    pub fn new() -> Self {
        LoginFlowStore {
            inner: RwLock::new(HashMap::new()),
        }
    }

    async fn create(&self, client_id: String) -> String {
        let flow_id = Uuid::new_v4().to_string();
        let mut inner = self.inner.write().await;
        inner.insert(
            flow_id.clone(),
            LoginFlow {
                client_id,
                step: FlowStep::Form,
            },
        );
        flow_id
    }

    async fn get(&self, flow_id: &str) -> Option<LoginFlow> {
        let inner = self.inner.read().await;
        inner.get(flow_id).cloned()
    }

    async fn mark_done(&self, flow_id: &str) {
        let mut inner = self.inner.write().await;
        if let Some(flow) = inner.get_mut(flow_id) {
            flow.step = FlowStep::Done;
        }
    }

    async fn remove(&self, flow_id: &str) -> Option<LoginFlow> {
        let mut inner = self.inner.write().await;
        inner.remove(flow_id)
    }
}

fn new_token() -> String {
    Uuid::new_v4().simple().to_string()
}

// ---------------------------------------------------------------------------
// Request / Response shapes — exactly matching HA protocol
// ---------------------------------------------------------------------------

/// Response body from GET /auth/providers.
///
/// Source: AuthProvidersView.get
///   return self.json({"providers": [...], "preselect_remember_me": bool})
#[derive(Serialize)]
struct ProvidersResponse {
    providers: Vec<ProviderEntry>,
    preselect_remember_me: bool,
}

#[derive(Serialize)]
struct ProviderEntry {
    name: String,
    id: Option<String>,
    #[serde(rename = "type")]
    provider_type: String,
}

/// POST /auth/login_flow request body.
///
/// Source: LoginFlowIndexView.post schema:
///   vol.Required("client_id"): str,
///   vol.Required("handler"): [str|None, str|None]   (length 2)
///   vol.Required("redirect_uri"): str,
#[derive(Deserialize)]
struct LoginFlowRequest {
    client_id: String,
}

/// POST /auth/login_flow/{flow_id} request body.
///
/// Source: LoginFlowResourceView.post schema:
///   vol.Required("client_id"): str   (+ extra fields for the step)
#[derive(Deserialize)]
struct LoginFlowStepRequest {
    client_id: String,
    username: Option<String>,
    password: Option<String>,
}

/// POST /auth/token request body — URL-encoded form.
///
/// Source: TokenView.post — reads `await request.post()` (form body).
#[derive(Deserialize)]
struct TokenRequest {
    grant_type: Option<String>,
    /// For authorization_code grant.
    code: Option<String>,
    /// For refresh_token grant.
    refresh_token: Option<String>,
    client_id: Option<String>,
    /// For revoke action (IndieAuth backwards compat).
    action: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct AuthorizeRequest {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
}

#[derive(Deserialize)]
struct AuthorizeForm {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    name: Option<String>,
    username: String,
    password: String,
    location_name: Option<String>,
    language: Option<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Return a router for all HA auth endpoints (no state applied yet).
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(well_known_oauth_info),
        )
        .route(
            "/auth/authorize",
            get(auth_authorize).post(auth_authorize_submit),
        )
        .route("/auth/providers", get(auth_providers))
        .route("/auth/login_flow", post(login_flow_init))
        .route("/auth/login_flow/{flow_id}", post(login_flow_step))
        .route("/auth/token", post(auth_token))
        .route("/auth/revoke", post(auth_revoke))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn well_known_oauth_info() -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "authorization_endpoint": "/auth/authorize",
            "token_endpoint": "/auth/token",
            "revocation_endpoint": "/auth/revoke",
            "response_types_supported": ["code"],
            "service_documentation": "https://developers.home-assistant.io/docs/auth_api"
        })),
    )
        .into_response()
}

async fn auth_authorize(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    let onboarding = match state.core.onboarding_progress(&state.storage).await {
        Ok(status) => status,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load onboarding state: {err:#}")})),
            )
                .into_response();
        }
    };

    let request = parse_authorize_request(&uri);
    if authorize_request_error(&request).is_some() {
        return Html(render_authorize_page(
            &state,
            &request,
            onboarding.onboarded,
            Some("Invalid authorization request."),
        ))
        .into_response();
    }

    Html(render_authorize_page(
        &state,
        &request,
        onboarding.onboarded,
        None,
    ))
    .into_response()
}

async fn auth_authorize_submit(
    State(state): State<Arc<AppState>>,
    axum::extract::Form(form): axum::extract::Form<AuthorizeForm>,
) -> Response {
    let request = AuthorizeRequest {
        response_type: Some(form.response_type.clone()),
        client_id: Some(form.client_id.clone()),
        redirect_uri: Some(form.redirect_uri.clone()),
        state: form.state.clone(),
    };
    if let Some(error) = authorize_request_error(&request) {
        return Html(render_authorize_page(
            &state,
            &request,
            false,
            Some(error),
        ))
        .into_response();
    }

    let onboarding = match state.core.onboarding_progress(&state.storage).await {
        Ok(status) => status,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load onboarding state: {err:#}")})),
            )
                .into_response();
        }
    };

    if !onboarding.onboarded {
        let username = form.username.trim();
        let password = form.password.trim();
        if username.is_empty() || password.is_empty() {
            return Html(render_authorize_page(
                &state,
                &request,
                false,
                Some("Username and password are required."),
            ))
            .into_response();
        }

        let display_name = non_empty(form.name.as_deref(), username).to_string();
        let location_name = non_empty(form.location_name.as_deref(), state.config.ui.product_name.as_str()).to_string();
        let language = non_empty(form.language.as_deref(), "en").to_string();

        if let Err(err) = state
            .core
            .bootstrap_authorized_owner(
                &state.storage,
                &state.auth,
                &AuthorizeBootstrapInput {
                    display_name,
                    username: username.to_string(),
                    password: password.to_string(),
                    location_name,
                    language,
                },
            )
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to complete onboarding: {err:#}")})),
            )
                .into_response();
        }
    }

    let auth_user = match state.core.auth_user(&state.auth, &state.storage).await {
        Ok(user) => user,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load auth user: {err:#}")})),
            )
                .into_response();
        }
    };

    let valid = auth_user.as_ref().is_some_and(|user| {
        user.username == form.username
            && crate::auth_store::verify_password(&form.password, &user.password)
    });
    // Migrate legacy plaintext password to argon2 hash on first successful login.
    if valid {
        if let Some(user) = &auth_user {
            if !user.password.starts_with("$argon2") {
                match crate::auth_store::hash_password(&form.password) {
                    Ok(hashed) => {
                        let mut upgraded = user.clone();
                        upgraded.password = hashed.clone();
                        if let Err(e) = state.auth.save_user(&upgraded).await {
                            tracing::warn!("Password migration failed (auth_user.json): {e:#}");
                        }
                        // Also upgrade the copy stored in onboarding.json.
                        let h = hashed.clone();
                        if let Err(e) = state
                            .storage
                            .update_onboarding(|ob| {
                                if let Some(u) = &mut ob.user {
                                    u.password = h.clone();
                                }
                                Ok(())
                            })
                            .await
                        {
                            tracing::warn!("Password migration failed (onboarding.json): {e:#}");
                        }
                    }
                    Err(e) => tracing::warn!("Password migration hash failed: {e:#}"),
                }
            }
        }
    }
    if !valid {
        return Html(render_authorize_page(
            &state,
            &request,
            true,
            Some("Invalid username or password."),
        ))
        .into_response();
    }

    let auth_code = state.tokens.issue_auth_code(&form.client_id).await;
    let location = build_authorize_redirect(&form.redirect_uri, &auth_code, form.state.as_deref());
    let mut headers = HeaderMap::new();
    match HeaderValue::from_str(&location) {
        Ok(value) => {
            headers.insert(header::LOCATION, value);
            (StatusCode::FOUND, headers).into_response()
        }
        Err(_) => Html(render_authorize_page(
            &state,
            &request,
            true,
            Some("Invalid redirect URI."),
        ))
        .into_response(),
    }
}

/// GET /auth/providers
///
/// Source: AuthProvidersView.get
/// Returns the list of configured authentication providers.
/// The official app uses this to decide which login form to show.
///
/// We expose a single "homeassistant" (username/password) provider.
async fn auth_providers(State(state): State<Arc<AppState>>) -> Response {
    match state.core.is_onboarded(&state.storage).await {
        Ok(false) => return onboarding_required_response(),
        Ok(_) => {}
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load onboarding state: {err:#}")})),
            )
                .into_response();
        }
    }

    let resp = ProvidersResponse {
        providers: vec![ProviderEntry {
            name: "Home Assistant Local".into(),
            id: None,
            provider_type: "homeassistant".into(),
        }],
        // Source: preselect_remember_me = not cloud_connection and is_local(remote_address)
        preselect_remember_me: true,
    };
    (StatusCode::OK, Json(resp)).into_response()
}

/// POST /auth/login_flow
///
/// Source: LoginFlowIndexView.post
/// Creates a new login flow and returns the first form step.
///
/// Expected response shape (flow result type = "form"):
/// ```json
/// {
///   "flow_id": "...",
///   "type": "form",
///   "step_id": "init",
///   "data_schema": [{"name": "username"}, {"name": "password", "type": "string"}],
///   "errors": {}
/// }
/// ```
async fn login_flow_init(
    State(state): State<Arc<AppState>>,
    body: axum::extract::Json<LoginFlowRequest>,
) -> Response {
    match state.core.is_onboarded(&state.storage).await {
        Ok(false) => return onboarding_required_response(),
        Ok(_) => {}
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load onboarding state: {err:#}")})),
            )
                .into_response();
        }
    }

    // Source: indieauth.verify_client_id(client_id) — must be a URL
    // We accept any non-empty string for embedded use.
    if body.client_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Invalid client id"})),
        )
            .into_response();
    }

    let flow_id = state.flows.create(body.client_id.clone()).await;

    // Source: _prepare_result_json for FlowResultType.FORM
    let resp = json!({
        "flow_id": flow_id,
        "type": "form",
        "step_id": "init",
        "data_schema": [
            {"name": "username"},
            {"name": "password", "type": "string"}
        ],
        "errors": {}
    });
    (StatusCode::OK, Json(resp)).into_response()
}

/// POST /auth/login_flow/{flow_id}
///
/// Source: LoginFlowResourceView.post
/// Submits credentials for the current step of the login flow.
///
/// On success returns flow result type = "create_entry" with a `result` field
/// containing the authorization code (which the client then exchanges at /auth/token).
async fn login_flow_step(
    State(state): State<Arc<AppState>>,
    Path(flow_id): Path<String>,
    body: axum::extract::Json<LoginFlowStepRequest>,
) -> Response {
    if body.client_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Invalid client id"})),
        )
            .into_response();
    }

    let flow = match state.flows.get(&flow_id).await {
        Some(f) => f,
        None => {
            // Source: LoginFlowResourceView.post — UnknownFlow
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Invalid flow specified"})),
            )
                .into_response();
        }
    };

    if flow.client_id != body.client_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Invalid client id"})),
        )
            .into_response();
    }

    let auth_user = match state.core.auth_user(&state.auth, &state.storage).await {
        Ok(user) => user,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("failed to load auth user: {err:#}")})),
            )
                .into_response();
        }
    };

    let valid = auth_user.as_ref().is_some_and(|user| {
        body.username.as_deref() == Some(user.username.as_str())
            && body.password.as_deref().is_some_and(|p| {
                crate::auth_store::verify_password(p, &user.password)
            })
    });
    // Migrate legacy plaintext password to argon2 hash on first successful login.
    if valid {
        if let Some(user) = &auth_user {
            if !user.password.starts_with("$argon2") {
                if let Some(p) = body.password.as_deref() {
                    match crate::auth_store::hash_password(p) {
                        Ok(hashed) => {
                            let mut upgraded = user.clone();
                            upgraded.password = hashed.clone();
                            if let Err(e) = state.auth.save_user(&upgraded).await {
                                tracing::warn!("Password migration failed (auth_user.json): {e:#}");
                            }
                            // Also upgrade the copy stored in onboarding.json.
                            let h = hashed.clone();
                            if let Err(e) = state
                                .storage
                                .update_onboarding(|ob| {
                                    if let Some(u) = &mut ob.user {
                                        u.password = h.clone();
                                    }
                                    Ok(())
                                })
                                .await
                            {
                                tracing::warn!(
                                    "Password migration failed (onboarding.json): {e:#}"
                                );
                            }
                        }
                        Err(e) => tracing::warn!("Password migration hash failed: {e:#}"),
                    }
                }
            }
        }
    }

    if !valid {
        // Source: DataEntryFlow.async_configure — invalid_auth error
        return (
            StatusCode::OK, // HA returns 200 even for errors in flow steps
            Json(json!({
                "flow_id": flow_id,
                "type": "form",
                "step_id": "init",
                "data_schema": [
                    {"name": "username"},
                    {"name": "password", "type": "string"}
                ],
                "errors": {"base": "invalid_auth"}
            })),
        )
            .into_response();
    }

    // Issue an auth code and mark flow as done.
    let auth_code = state.tokens.issue_auth_code(&body.client_id).await;
    state.flows.mark_done(&flow_id).await;
    state.flows.remove(&flow_id).await;

    // Source: _async_flow_result_to_response for FlowResultType.CREATE_ENTRY
    // result["result"] = auth_code (stored under the key used later in /auth/token)
    (
        StatusCode::OK,
        Json(json!({
            "flow_id": flow_id,
            "type": "create_entry",
            "result": auth_code
        })),
    )
        .into_response()
}

/// POST /auth/token
///
/// Source: TokenView.post
/// OAuth2 token endpoint. Accepts URL-encoded form body.
///
/// Supported grant types:
///   - "authorization_code": exchange auth code for access + refresh tokens
///   - "refresh_token": exchange refresh token for a new access token
///
/// Error responses (RFC 6749 §5.2):
///   {"error": "unsupported_grant_type"}         HTTP 400
///   {"error": "invalid_request", ...}            HTTP 400
///   {"error": "invalid_grant"}                   HTTP 400
async fn auth_token(
    State(state): State<Arc<AppState>>,
    body: axum::extract::Form<TokenRequest>,
) -> Response {
    // IndieAuth backwards compat: POST /auth/token?action=revoke → revoke
    // Source: TokenView.post — if data.get("action") == "revoke": ...
    if body.action.as_deref() == Some("revoke") {
        if let Some(token) = &body.token {
            state.tokens.revoke_refresh_token(token).await;
        }
        return (StatusCode::OK, Json(json!({}))).into_response();
    }

    match body.grant_type.as_deref() {
        Some("authorization_code") => {
            let client_id = match &body.client_id {
                Some(c) if !c.is_empty() => c.as_str(),
                _ => {
                    // Source: "Invalid client id"
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "invalid_request", "error_description": "Invalid client id"})),
                    )
                        .into_response();
                }
            };

            let code = match &body.code {
                Some(c) if !c.is_empty() => c.as_str(),
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "invalid_request", "error_description": "Invalid code"})),
                    )
                        .into_response();
                }
            };

            match state.tokens.exchange_code(client_id, code).await {
                Some((access_token, refresh_token)) => {
                    // Source: TokenView._async_handle_auth_code return value
                    (
                        StatusCode::OK,
                        Json(json!({
                            "access_token": access_token,
                            "token_type": "Bearer",
                            "refresh_token": refresh_token,
                            "expires_in": 1800,
                            "ha_auth_provider": "homeassistant"
                        })),
                    )
                        .into_response()
                }
                None => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid_request", "error_description": "Invalid code"})),
                )
                    .into_response(),
            }
        }

        Some("refresh_token") => {
            let refresh_token = match &body.refresh_token {
                Some(t) if !t.is_empty() => t.as_str(),
                _ => {
                    // Source: TokenView._async_handle_refresh_token — no token → invalid_request
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "invalid_request"})),
                    )
                        .into_response();
                }
            };

            let client_id = body.client_id.as_deref();

            match state
                .tokens
                .refresh_access_token(client_id, refresh_token)
                .await
            {
                Some(access_token) => {
                    // Source: TokenView._async_handle_refresh_token return value
                    (
                        StatusCode::OK,
                        Json(json!({
                            "access_token": access_token,
                            "token_type": "Bearer",
                            "expires_in": 1800
                        })),
                    )
                        .into_response()
                }
                None => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid_grant"})),
                )
                    .into_response(),
            }
        }

        _ => {
            // Source: TokenView.post — unsupported grant → 400 unsupported_grant_type
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "unsupported_grant_type"})),
            )
                .into_response()
        }
    }
}

/// POST /auth/revoke
///
/// Source: RevokeTokenView.post
/// Revokes a refresh token.  Response is ALWAYS 200 regardless of whether
/// the token existed — this is specified by the HA source:
///   "The response code will ALWAYS be 200"
async fn auth_revoke(
    State(state): State<Arc<AppState>>,
    body: axum::extract::Form<HashMap<String, String>>,
) -> Response {
    if let Some(token) = body.get("token") {
        state.tokens.revoke_refresh_token(token).await;
    }
    // Source: RevokeTokenView — return self.json({})   HTTP 200 always
    (StatusCode::OK, Json(json!({}))).into_response()
}

fn parse_authorize_request(uri: &Uri) -> AuthorizeRequest {
    let mut request = AuthorizeRequest::default();
    let Some(query) = uri.query() else {
        return request;
    };

    for pair in query.split('&') {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(raw_value);
        match raw_key {
            "response_type" => request.response_type = Some(value),
            "client_id" => request.client_id = Some(value),
            "redirect_uri" => request.redirect_uri = Some(value),
            "state" => request.state = Some(value),
            _ => {}
        }
    }

    request
}

fn authorize_request_error(request: &AuthorizeRequest) -> Option<&'static str> {
    if request.response_type.as_deref() != Some("code") {
        return Some("Unsupported response_type.");
    }
    if request.client_id.as_deref().is_none_or(str::is_empty) {
        return Some("Invalid client_id.");
    }
    if request.redirect_uri.as_deref().is_none_or(str::is_empty) {
        return Some("Invalid redirect_uri.");
    }
    None
}

fn build_authorize_redirect(redirect_uri: &str, code: &str, state: Option<&str>) -> String {
    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    let mut location = format!("{redirect_uri}{separator}code={code}");
    if let Some(state) = state {
        location.push_str("&state=");
        location.push_str(&percent_encode(state));
    }
    location
}

fn render_authorize_page(
    state: &AppState,
    request: &AuthorizeRequest,
    onboarded: bool,
    error: Option<&str>,
) -> String {
    use minijinja::{context, value::Value};
    // Manually escape URL values with html_escape (which does NOT encode /)
    // and mark them safe so minijinja's auto-escape doesn't re-encode / as &#x2f;.
    let ctx = context! {
        product_name  => state.config.ui.product_name.as_str(),
        response_type => Value::from_safe_string(html_escape(request.response_type.as_deref().unwrap_or("code"))),
        client_id     => Value::from_safe_string(html_escape(request.client_id.as_deref().unwrap_or(""))),
        redirect_uri  => Value::from_safe_string(html_escape(request.redirect_uri.as_deref().unwrap_or(""))),
        state_param   => Value::from_safe_string(html_escape(request.state.as_deref().unwrap_or(""))),
        onboarded     => onboarded,
        error         => error,
    };
    state
        .templates
        .get_template("authorize.html")
        .and_then(|t| t.render(ctx))
        .unwrap_or_else(|e| format!("Template error: {e}"))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    decoded.push((high << 4) | low);
                    index += 3;
                } else {
                    decoded.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push_str("%20"),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum_test::TestServer;
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
                data_dir: PathBuf::from("/tmp/ha-auth-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
            areas: crate::config::AreasConfig::default(),
            history: crate::config::HistoryConfig::default(),
        };
        let storage = Storage::new_in_memory();
        storage
            .save_onboarding(&completed_onboarding())
            .await
            .expect("save onboarding state");
        let state = Arc::new(AppState::new(config, storage));
        let app = super::router().with_state(state);
        TestServer::new(app).unwrap()
    }

    async fn make_unboarded_server() -> TestServer {
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
                data_dir: PathBuf::from("/tmp/ha-auth-test"),
            },
            ui: UiConfig {
                product_name: "Test Home".into(),
            },
            areas: crate::config::AreasConfig::default(),
            history: crate::config::HistoryConfig::default(),
        };
        let storage = Storage::new_in_memory();
        let state = Arc::new(AppState::new(config, storage));
        let app = super::router().with_state(state);
        TestServer::new(app).unwrap()
    }

    // -----------------------------------------------------------------------
    // GET /auth/providers
    ///   Returns {"providers": [...], "preselect_remember_me": bool}
    #[tokio::test]
    async fn get_providers_returns_200_with_provider_list() {
        let server = make_server().await;
        let resp = server.get("/auth/providers").await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert!(json.get("providers").is_some(), "must have providers array");
        assert!(
            json.get("preselect_remember_me").is_some(),
            "must have preselect_remember_me"
        );
        let providers = json["providers"].as_array().unwrap();
        assert!(!providers.is_empty(), "must expose at least one provider");
        // Each provider must have name, type (+ optional id).
        for p in providers {
            assert!(p.get("name").is_some(), "provider missing name");
            assert!(p.get("type").is_some(), "provider missing type");
        }
    }

    #[tokio::test]
    async fn well_known_oauth_info_returns_authorization_metadata() {
        let server = make_server().await;

        let resp = server.get("/.well-known/oauth-authorization-server").await;

        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(json["authorization_endpoint"], "/auth/authorize");
        assert_eq!(json["token_endpoint"], "/auth/token");
        assert_eq!(json["revocation_endpoint"], "/auth/revoke");
        assert_eq!(
            json["response_types_supported"],
            serde_json::json!(["code"])
        );
    }

    #[tokio::test]
    async fn get_authorize_returns_html_login_page() {
        let server = make_server().await;

        let resp = server
            .get("/auth/authorize?response_type=code&client_id=https%3A%2F%2Fhome-assistant.io%2FiOS&redirect_uri=homeassistant%3A%2F%2Fauth-callback&state=abc123")
            .await;

        resp.assert_status_ok();
        let body = resp.text();
        assert!(body.contains("<form method=\"post\" action=\"/auth/authorize\">"));
        assert!(body.contains("Sign in to Test Home"));
        assert!(body.contains("name=\"client_id\" value=\"https://home-assistant.io/iOS\""));
    }

    #[tokio::test]
    async fn get_authorize_returns_onboarding_page_when_not_onboarded() {
        let server = make_unboarded_server().await;

        let resp = server
            .get("/auth/authorize?response_type=code&client_id=https%3A%2F%2Fhome-assistant.io%2FiOS&redirect_uri=homeassistant%3A%2F%2Fauth-callback")
            .await;

        resp.assert_status_ok();
        let body = resp.text();
        assert!(body.contains("Set up Test Home"));
        assert!(body.contains("Create account and authorize"));
    }

    #[tokio::test]
    async fn post_authorize_redirects_with_code_and_state() {
        let server = make_server().await;

        let resp = server
            .post("/auth/authorize")
            .form(&[
                ("response_type", "code"),
                ("client_id", "https://home-assistant.io/iOS"),
                ("redirect_uri", "homeassistant://auth-callback"),
                ("state", "abc#123"),
                ("username", "admin"),
                ("password", "secret"),
            ])
            .await;

        resp.assert_status(StatusCode::FOUND);
        let location = resp
            .headers()
            .get("location")
            .expect("location header")
            .to_str()
            .expect("location string");
        assert!(location.starts_with("homeassistant://auth-callback?code="));
        assert!(location.contains("&state=abc%23123"));
    }

    #[tokio::test]
    async fn post_authorize_bootstraps_onboarding_when_not_onboarded() {
        let server = make_unboarded_server().await;

        let resp = server
            .post("/auth/authorize")
            .form(&[
                ("response_type", "code"),
                ("client_id", "https://home-assistant.io/iOS"),
                ("redirect_uri", "homeassistant://auth-callback"),
                ("name", "Owner"),
                ("username", "owner"),
                ("password", "secret"),
                ("location_name", "My Home"),
            ])
            .await;

        resp.assert_status(StatusCode::FOUND);
        let location = resp
            .headers()
            .get("location")
            .expect("location header")
            .to_str()
            .expect("location string");
        assert!(location.starts_with("homeassistant://auth-callback?code="));

        let providers = server.get("/auth/providers").await;
        providers.assert_status_ok();
    }

    // -----------------------------------------------------------------------
    // POST /auth/login_flow
    // -----------------------------------------------------------------------

    /// Source: LoginFlowIndexView.post — returns flow_id and form step on success.
    #[tokio::test]
    async fn post_login_flow_init_returns_form_step() {
        let server = make_server().await;
        let resp = server
            .post("/auth/login_flow")
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "handler": ["homeassistant", null],
                "redirect_uri": "homeassistant://auth-callback"
            }))
            .await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(json["type"], "form");
        assert!(json.get("flow_id").is_some(), "must return flow_id");
        assert_eq!(json["step_id"], "init");
        assert!(
            json.get("data_schema").is_some(),
            "form step must have data_schema"
        );
    }

    /// Empty client_id must be rejected.
    ///
    /// Source: indieauth.verify_client_id(client_id) check.
    #[tokio::test]
    async fn post_login_flow_empty_client_id_returns_400() {
        let server = make_server().await;
        let resp = server
            .post("/auth/login_flow")
            .json(&serde_json::json!({
                "client_id": "",
                "handler": ["homeassistant", null],
                "redirect_uri": "homeassistant://auth-callback"
            }))
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // POST /auth/login_flow/{flow_id}
    // -----------------------------------------------------------------------

    async fn init_flow(server: &TestServer) -> String {
        let resp = server
            .post("/auth/login_flow")
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "handler": ["homeassistant", null],
                "redirect_uri": "homeassistant://auth-callback"
            }))
            .await;
        resp.json::<Value>()["flow_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Valid credentials → create_entry result with auth code.
    ///
    /// Source: LoginFlowResourceView.post → _async_flow_result_to_response
    ///   result["type"] == "create_entry"  and  result["result"] == auth_code
    #[tokio::test]
    async fn post_login_flow_step_valid_creds_returns_create_entry() {
        let server = make_server().await;
        let flow_id = init_flow(&server).await;
        let resp = server
            .post(&format!("/auth/login_flow/{flow_id}"))
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "username": "admin",
                "password": "secret"
            }))
            .await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(
            json["type"], "create_entry",
            "must be create_entry on success"
        );
        assert!(
            json.get("result").is_some(),
            "create_entry must contain result (auth code)"
        );
    }

    /// Invalid credentials → form step with {"base": "invalid_auth"} error.
    ///
    /// Source: LoginFlowResourceView when DataEntryFlow returns form with errors.
    #[tokio::test]
    async fn post_login_flow_step_bad_creds_returns_invalid_auth() {
        let server = make_server().await;
        let flow_id = init_flow(&server).await;
        let resp = server
            .post(&format!("/auth/login_flow/{flow_id}"))
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "username": "",
                "password": ""
            }))
            .await;
        resp.assert_status_ok(); // HA returns 200 even on auth failure
        let json: Value = resp.json();
        assert_eq!(json["type"], "form");
        assert_eq!(json["errors"]["base"], "invalid_auth");
    }

    /// Unknown flow_id → 404.
    ///
    /// Source: LoginFlowResourceView.post — UnknownFlow → 404
    #[tokio::test]
    async fn post_login_flow_step_unknown_flow_returns_404() {
        let server = make_server().await;
        let resp = server
            .post("/auth/login_flow/nonexistent-flow-id")
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "username": "admin",
                "password": "secret"
            }))
            .await;
        resp.assert_status(StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------
    // POST /auth/token
    // -----------------------------------------------------------------------

    async fn get_auth_code(server: &TestServer) -> (String, String) {
        let flow_id = init_flow(server).await;
        let resp = server
            .post(&format!("/auth/login_flow/{flow_id}"))
            .json(&serde_json::json!({
                "client_id": "https://home-assistant.io/iOS",
                "username": "admin",
                "password": "secret"
            }))
            .await;
        let json: Value = resp.json();
        let code = json["result"].as_str().unwrap().to_string();
        (code, "https://home-assistant.io/iOS".to_string())
    }

    /// authorization_code grant → access_token + refresh_token + Bearer type.
    ///
    /// Source: TokenView._async_handle_auth_code return shape:
    ///   {"access_token": ..., "token_type": "Bearer", "refresh_token": ...,
    ///    "expires_in": int, "ha_auth_provider": ...}
    #[tokio::test]
    async fn post_token_auth_code_grant_returns_tokens() {
        let server = make_server().await;
        let (code, client_id) = get_auth_code(&server).await;

        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client_id),
            ])
            .await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(json["token_type"], "Bearer");
        assert!(json.get("access_token").is_some());
        assert!(json.get("refresh_token").is_some());
        assert!(json.get("expires_in").is_some());
        assert_eq!(json["ha_auth_provider"], "homeassistant");
    }

    /// Invalid auth code → HTTP 400 invalid_request.
    #[tokio::test]
    async fn post_token_invalid_code_returns_400() {
        let server = make_server().await;
        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", "not-a-real-code"),
                ("client_id", "https://home-assistant.io/iOS"),
            ])
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let json: Value = resp.json();
        assert_eq!(json["error"], "invalid_request");
    }

    /// refresh_token grant → new access_token + Bearer type (no refresh_token in response).
    ///
    /// Source: TokenView._async_handle_refresh_token return shape:
    ///   {"access_token": ..., "token_type": "Bearer", "expires_in": int}
    #[tokio::test]
    async fn post_token_refresh_grant_returns_new_access_token() {
        let server = make_server().await;
        let (code, client_id) = get_auth_code(&server).await;

        // First exchange for refresh token.
        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client_id),
            ])
            .await;
        let first: Value = resp.json();
        let refresh_token = first["refresh_token"].as_str().unwrap();

        // Then use it to get a new access token.
        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &client_id),
            ])
            .await;
        resp.assert_status_ok();
        let json: Value = resp.json();
        assert_eq!(json["token_type"], "Bearer");
        assert!(json.get("access_token").is_some());
        assert!(json.get("expires_in").is_some());
        // refresh grant does NOT return a new refresh_token
        assert!(json.get("refresh_token").is_none());
    }

    /// Missing refresh_token → HTTP 400 invalid_request.
    ///
    /// Source: TokenView._async_handle_refresh_token:
    ///   if (token := data.get("refresh_token")) is None:
    ///       return self.json({"error": "invalid_request"}, status_code=400)
    #[tokio::test]
    async fn post_token_refresh_missing_token_returns_400() {
        let server = make_server().await;
        let resp = server
            .post("/auth/token")
            .form(&[("grant_type", "refresh_token"), ("client_id", "test")])
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let json: Value = resp.json();
        assert_eq!(json["error"], "invalid_request");
    }

    /// Bad refresh token → HTTP 400 invalid_grant.
    ///
    /// Source: TokenView._async_handle_refresh_token:
    ///   if refresh_token is None: return {"error": "invalid_grant"}
    #[tokio::test]
    async fn post_token_refresh_bad_token_returns_invalid_grant() {
        let server = make_server().await;
        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", "not-a-real-refresh-token"),
            ])
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let json: Value = resp.json();
        assert_eq!(json["error"], "invalid_grant");
    }

    /// Unsupported grant type → HTTP 400 unsupported_grant_type.
    ///
    /// Source: TokenView.post — fallthrough case.
    #[tokio::test]
    async fn post_token_unsupported_grant_returns_400() {
        let server = make_server().await;
        let resp = server
            .post("/auth/token")
            .form(&[("grant_type", "client_credentials")])
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let json: Value = resp.json();
        assert_eq!(json["error"], "unsupported_grant_type");
    }

    // -----------------------------------------------------------------------
    // POST /auth/revoke
    // -----------------------------------------------------------------------

    /// Revoke valid token → HTTP 200.
    ///
    /// Source: RevokeTokenView.post — "response code ALWAYS 200"
    #[tokio::test]
    async fn post_revoke_returns_200_always() {
        let server = make_server().await;
        // Revoke a non-existent token — must still return 200
        let resp = server
            .post("/auth/revoke")
            .form(&[("token", "fake-token-that-does-not-exist")])
            .await;
        resp.assert_status_ok();
    }

    /// Revoke a real refresh token, then try to use it → invalid_grant.
    #[tokio::test]
    async fn post_revoke_invalidates_refresh_token() {
        let server = make_server().await;
        let (code, client_id) = get_auth_code(&server).await;

        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client_id),
            ])
            .await;
        let tokens: Value = resp.json();
        let refresh_token = tokens["refresh_token"].as_str().unwrap();

        // Revoke it.
        let resp = server
            .post("/auth/revoke")
            .form(&[("token", refresh_token)])
            .await;
        resp.assert_status_ok();

        // Try to use the revoked refresh token.
        let resp = server
            .post("/auth/token")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let json: Value = resp.json();
        assert_eq!(json["error"], "invalid_grant");
    }
}

// ---------------------------------------------------------------------------
// TokenStore unit tests — cover expiry logic that the HTTP-level tests cannot
// ---------------------------------------------------------------------------

#[cfg(test)]
mod token_store_tests {
    use super::*;

    fn temp_token_store() -> TokenStore {
        let dir = std::env::temp_dir().join(format!("token-store-{}", uuid::Uuid::new_v4()));
        TokenStore::new(dir)
    }

    /// A token issued at epoch (Jan 1 1970) must be expired immediately.
    #[tokio::test]
    async fn expired_token_is_rejected() {
        let store = temp_token_store();
        {
            let mut inner = store.inner.lock().await;
            inner.access_tokens.insert(
                "expired-token".into(),
                AccessTokenEntry { client_id: "c1".into(), issued_at: 0 },
            );
        }
        assert!(
            store.validate_access_token("expired-token").await.is_none(),
            "epoch-issued token must be rejected"
        );
        // Also verify the entry was evicted from memory.
        let inner = store.inner.lock().await;
        assert!(!inner.access_tokens.contains_key("expired-token"), "expired entry must be removed");
    }

    /// A token issued right now must be accepted.
    #[tokio::test]
    async fn fresh_token_is_accepted() {
        let store = temp_token_store();
        {
            let mut inner = store.inner.lock().await;
            inner.access_tokens.insert(
                "fresh-token".into(),
                AccessTokenEntry { client_id: "c1".into(), issued_at: now_unix_secs() },
            );
        }
        assert!(
            store.validate_access_token("fresh-token").await.is_some(),
            "just-issued token must be accepted"
        );
    }

    /// An unknown token must return None.
    #[tokio::test]
    async fn unknown_token_is_rejected() {
        let store = temp_token_store();
        assert!(store.validate_access_token("unknown").await.is_none());
    }

    #[tokio::test]
    async fn exchange_code_rejects_expired_code() {
        let store = temp_token_store();
        let code = store.issue_auth_code("client1").await;
        store.backdate_auth_code(&code, AUTH_CODE_TTL_SECS + 1).await;
        assert!(
            store.exchange_code("client1", &code).await.is_none(),
            "expired code must be rejected"
        );
    }

    #[tokio::test]
    async fn exchange_code_rejects_wrong_client_id() {
        let store = temp_token_store();
        let code = store.issue_auth_code("client1").await;
        assert!(
            store.exchange_code("wrong_client", &code).await.is_none(),
            "wrong client_id must be rejected"
        );
    }

    #[tokio::test]
    async fn issue_auth_code_sweeps_expired_codes() {
        let store = temp_token_store();
        // Issue a code and make it appear expired.
        let old_code = store.issue_auth_code("client1").await;
        store.backdate_auth_code(&old_code, AUTH_CODE_TTL_SECS + 1).await;
        // Issue a new code — the lazy sweep should remove old_code.
        let _new_code = store.issue_auth_code("client2").await;
        assert_eq!(
            store.auth_code_count().await,
            1,
            "expired code should have been swept, leaving only the new code"
        );
    }
}
