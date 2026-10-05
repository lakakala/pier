use crate::{
    Controller,
    api::{ApiError, ApiResult, bad, conflict},
};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const LIFETIME: u64 = 8 * 60 * 60;

#[derive(Clone, Copy)]
struct SessionCookie {
    name: &'static str,
    secure: bool,
}
impl SessionCookie {
    fn for_origin(origin: &str) -> Result<Self, ApiError> {
        match origin.split_once("://").map(|(scheme, _)| scheme) {
            Some("http") => Ok(Self {
                name: "pier_session",
                secure: false,
            }),
            Some("https") => Ok(Self {
                name: "__Host-pier_session",
                secure: true,
            }),
            _ => Err(unauthorized()),
        }
    }
    fn header(self, token: &str, max_age: u64) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!(
            "{}={token}; Path=/; HttpOnly{secure}; SameSite=Strict; Max-Age={max_age}",
            self.name
        )
    }
}
pub(crate) struct Auth {
    lock: Mutex<()>,
    attempts: Mutex<(Instant, u32)>,
    work: Arc<tokio::sync::Semaphore>,
}
impl Default for Auth {
    fn default() -> Self {
        Self {
            lock: Mutex::new(()),
            attempts: Mutex::new((Instant::now(), 0)),
            work: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}
impl Auth {
    fn limit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
        let mut attempts = self.attempts.lock().unwrap();
        if attempts.0.elapsed() >= Duration::from_secs(60) {
            *attempts = (Instant::now(), 0);
        }
        if attempts.1 >= 60 {
            return Err(limited());
        }
        attempts.1 += 1;
        self.work.clone().try_acquire_owned().map_err(|_| limited())
    }
}
fn limited() -> ApiError {
    ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "too many authentication attempts; retry later".into(),
    )
}
fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "authentication required".into())
}
fn internal() -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "authentication operation failed".into(),
    )
}

#[derive(Clone, Serialize, Deserialize)]
struct Admin {
    username: String,
    password_hash: String,
    generation: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct WebSession {
    pub(crate) digest: String,
    pub(crate) username: String,
    generation: String,
    csrf_token: String,
    pub(crate) expires_at: u64,
}
impl WebSession {
    fn public(&self) -> Value {
        json!({"username":self.username,"expires_at":self.expires_at,"csrf_token":self.csrf_token})
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    username: String,
    password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialization {
    username: String,
    password: String,
    repository: Option<crate::settings::Repository>,
    #[serde(default)]
    settings: crate::runtime::RuntimePatch,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PasswordChange {
    current_password: String,
    new_password: String,
}

pub(crate) fn public_routes() -> Router<Arc<Controller>> {
    Router::new()
        .route("/v1/auth/status", get(status))
        .route("/v1/auth/init", post(initialize))
        .route("/v1/auth/login", post(login))
        .layer(DefaultBodyLimit::max(16 * 1024))
}
fn validate_username(value: &str) -> Result<(), ApiError> {
    if value.is_empty()
        || value.len() > 64
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(bad(
            "username must be 1..64 bytes without surrounding whitespace or control characters",
        ));
    }
    Ok(())
}
fn validate_password(value: &str) -> Result<(), ApiError> {
    if value.chars().count() < 12 || value.len() > 1024 {
        return Err(bad(
            "password must contain at least 12 characters and at most 1024 bytes",
        ));
    }
    Ok(())
}
fn hash_password(value: &str) -> Result<String, ApiError> {
    let salt =
        pier_protocol::secure::decode_key(&pier_protocol::new_token()).map_err(|_| internal())?;
    let salt = SaltString::encode_b64(&salt[..16]).map_err(|_| internal())?;
    Argon2::default()
        .hash_password(value.as_bytes(), &salt)
        .map(|v| v.to_string())
        .map_err(|_| internal())
}
fn verify(password: &str, encoded: &str) -> bool {
    PasswordHash::new(encoded).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}
pub(crate) fn check_origin(state: &Controller, headers: &HeaderMap) -> Result<(), ApiError> {
    let expected = state.public_url();
    if expected.is_none()
        || headers
            .get(header::ORIGIN)
            .is_none_or(|v| Some(v.as_bytes()) != expected.as_ref().map(|s| s.as_bytes()))
    {
        return Err(ApiError(StatusCode::FORBIDDEN, "invalid origin".into()));
    }
    Ok(())
}
fn initialization_origin(state: &Controller, headers: &HeaderMap) -> Result<String, ApiError> {
    if let Some(origin) = state.public_url() {
        check_origin(state, headers)?;
        return Ok(origin);
    }
    let invalid = || {
        ApiError(
            StatusCode::FORBIDDEN,
            "initialization requires matching HTTP(S) Origin and Host".into(),
        )
    };
    if headers.get_all(header::ORIGIN).iter().count() != 1
        || headers.get_all(header::HOST).iter().count() != 1
    {
        return Err(invalid());
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(invalid)?;
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(invalid)?;
    let canonical = pier_protocol::enrollment::origin(origin).map_err(|_| invalid())?;
    let url = url::Url::parse(&canonical).map_err(|_| invalid())?;
    let host_origin = pier_protocol::enrollment::origin(&format!("{}://{host}", url.scheme()))
        .map_err(|_| invalid())?;
    if origin != canonical || canonical != host_origin {
        return Err(invalid());
    }
    Ok(canonical)
}
pub(crate) fn check_write(
    state: &Controller,
    headers: &HeaderMap,
    session: &WebSession,
) -> Result<(), ApiError> {
    check_origin(state, headers)?;
    if !headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            pier_protocol::token_matches(value, &pier_protocol::hash(&session.csrf_token))
        })
    {
        return Err(ApiError(StatusCode::FORBIDDEN, "invalid csrf token".into()));
    }
    Ok(())
}
fn cookie_value(headers: &HeaderMap, policy: SessionCookie) -> Option<&str> {
    let mut found = None;
    for line in headers.get_all(header::COOKIE) {
        for part in line.to_str().ok()?.split(';') {
            if let Some((key, value)) = part.trim().split_once('=') {
                if key == policy.name {
                    if found.is_some()
                        || value.len() != 64
                        || !value.bytes().all(|c| c.is_ascii_hexdigit())
                    {
                        return None;
                    }
                    found = Some(value);
                }
            }
        }
    }
    found
}
fn fresh_session(admin: &Admin) -> (String, WebSession) {
    let token = pier_protocol::new_token();
    let session = WebSession {
        digest: pier_protocol::hash(&token),
        username: admin.username.clone(),
        generation: admin.generation.clone(),
        csrf_token: pier_protocol::new_token(),
        expires_at: pier_protocol::now() + LIFETIME,
    };
    (token, session)
}
fn session_response(token: &str, session: &WebSession, policy: SessionCookie) -> Response {
    let cookie = policy.header(token, LIFETIME);
    ([(header::SET_COOKIE, cookie)], Json(session.public())).into_response()
}
fn cleared(policy: SessionCookie) -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, policy.header("", 0))],
    )
        .into_response()
}
impl Controller {
    fn session_cookie(&self) -> Result<SessionCookie, ApiError> {
        SessionCookie::for_origin(&self.public_url().ok_or_else(unauthorized)?)
    }
    pub(crate) fn authenticate(&self, headers: &HeaderMap) -> Result<WebSession, ApiError> {
        let token = cookie_value(headers, self.session_cookie()?).ok_or_else(unauthorized)?;
        let session: WebSession = self
            .store
            .get("web_sessions", &pier_protocol::hash(token))?
            .ok_or_else(unauthorized)?;
        let admin: Admin = self.store.get("auth", "admin")?.ok_or_else(unauthorized)?;
        if session.expires_at <= pier_protocol::now() || session.generation != admin.generation {
            return Err(unauthorized());
        }
        Ok(session)
    }
    pub(crate) fn expire_web_sessions(&self) -> anyhow::Result<()> {
        let _lock = self.auth.lock.lock().unwrap();
        let generation = self
            .store
            .get::<Admin>("auth", "admin")?
            .map(|v| v.generation);
        for session in self.store.list::<WebSession>("web_sessions")? {
            if session.expires_at <= pier_protocol::now()
                || generation.as_ref() != Some(&session.generation)
            {
                self.store.delete("web_sessions", &session.digest)?;
            }
        }
        Ok(())
    }
}
async fn status(State(state): State<Arc<Controller>>) -> ApiResult<Value> {
    let initialized = state.store.get::<Admin>("auth", "admin")?.is_some();
    let settings = state.settings.read().unwrap();
    Ok(Json(
        json!({"initialized": initialized, "repository_configured": settings.repository.is_some(),
        "setup_defaults": if initialized { None } else { settings.runtime.as_ref().map(|v| v.setup_view()) }}),
    ))
}
async fn initialize(
    State(state): State<Arc<Controller>>,
    headers: HeaderMap,
    Json(input): Json<Initialization>,
) -> Result<Response, ApiError> {
    let origin = initialization_origin(&state, &headers)?;
    // Capture the policy before taking settings/runtime write locks below.
    let cookie = SessionCookie::for_origin(&origin)?;
    if state.store.get::<Admin>("auth", "admin")?.is_some() {
        return Err(conflict("already initialized"));
    }
    let repository = input
        .repository
        .or_else(|| state.settings.read().unwrap().repository.clone())
        .ok_or_else(|| bad("repository is required"))?;
    repository.validate().map_err(|e| bad(&e.to_string()))?;
    validate_username(&input.username)?;
    validate_password(&input.password)?;
    let permit = state.auth.limit()?;
    let password_hash = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_password(&input.password)
    })
    .await
    .map_err(|_| internal())??;
    let _lock = state.auth.lock.lock().unwrap();
    if state.store.get::<Admin>("auth", "admin")?.is_some() {
        return Err(conflict("already initialized"));
    }
    let admin = Admin {
        username: input.username,
        password_hash,
        generation: pier_protocol::new_token(),
    };
    let (token, session) = fresh_session(&admin);
    let _sync = state.sync_lock.lock().unwrap();
    let _mutation = state.mutation_lock.lock().unwrap();
    let mut settings = state.settings.write().unwrap();
    let mut updated = settings.clone();
    updated.repository = Some(repository);
    let runtime = input
        .settings
        .apply(updated.runtime.clone().unwrap(), Some(&origin))
        .map_err(|e| bad(&e.to_string()))?;
    let listener = Controller::prepare_listener(&runtime).map_err(|_| {
        conflict("agent listener unavailable; check its address, port and permissions")
    })?;
    let active = std::sync::Arc::new(crate::runtime::Runtime::new(runtime.clone())?);
    updated.runtime = Some(runtime);
    state.store.put_batch(&[
        (
            "auth",
            "admin",
            serde_json::to_value(&admin).map_err(|_| internal())?,
        ),
        (
            "web_sessions",
            &session.digest,
            serde_json::to_value(&session).map_err(|_| internal())?,
        ),
        (
            "settings",
            "controller",
            serde_json::to_value(&updated).map_err(|_| internal())?,
        ),
    ])?;
    *settings = updated;
    *state.runtime.write().unwrap() = Some(active);
    state.install_listener(listener);
    Ok(session_response(&token, &session, cookie))
}
async fn login(
    State(state): State<Arc<Controller>>,
    headers: HeaderMap,
    Json(input): Json<Credentials>,
) -> Result<Response, ApiError> {
    check_origin(&state, &headers)?;
    let cookie = state.session_cookie()?;
    if input.password.len() > 1024 || input.username.len() > 64 {
        return Err(unauthorized());
    }
    let admin: Admin = state
        .store
        .get("auth", "admin")?
        .ok_or_else(|| conflict("initialization required"))?;
    let permit = state.auth.limit()?;
    let expected = admin.clone();
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify(&input.password, &expected.password_hash) && input.username == expected.username
    })
    .await
    .map_err(|_| internal())?;
    if !valid {
        return Err(unauthorized());
    }
    let _lock = state.auth.lock.lock().unwrap();
    if state
        .store
        .get::<Admin>("auth", "admin")?
        .is_none_or(|v| v.generation != admin.generation)
    {
        return Err(unauthorized());
    }
    // A successful login rotates the browser's existing session.
    if let Some(token) = cookie_value(&headers, cookie) {
        state.terminals.revoke_owner(&pier_protocol::hash(token));
        state
            .store
            .delete("web_sessions", &pier_protocol::hash(token))?;
    }
    let (token, session) = fresh_session(&admin);
    state.store.put("web_sessions", &session.digest, &session)?;
    Ok(session_response(&token, &session, cookie))
}
pub(crate) async fn session(
    State(state): State<Arc<Controller>>,
    headers: HeaderMap,
) -> ApiResult<Value> {
    Ok(Json(state.authenticate(&headers)?.public()))
}
pub(crate) async fn logout(
    State(state): State<Arc<Controller>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let cookie = state.session_cookie()?;
    let _lock = state.auth.lock.lock().unwrap();
    let session = state.authenticate(&headers)?;
    state.store.delete("web_sessions", &session.digest)?;
    state.terminals.revoke_owner(&session.digest);
    Ok(cleared(cookie))
}
pub(crate) async fn password(
    State(state): State<Arc<Controller>>,
    headers: HeaderMap,
    Json(input): Json<PasswordChange>,
) -> Result<Response, ApiError> {
    let cookie = state.session_cookie()?;
    validate_password(&input.new_password)?;
    if input.current_password.len() > 1024 {
        return Err(unauthorized());
    }
    let session = state.authenticate(&headers)?;
    let admin: Admin = state.store.get("auth", "admin")?.ok_or_else(unauthorized)?;
    let permit = state.auth.limit()?;
    let expected = admin.clone();
    let password_hash = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if !verify(&input.current_password, &expected.password_hash) {
            return Err(unauthorized());
        }
        hash_password(&input.new_password)
    })
    .await
    .map_err(|_| internal())??;
    let _lock = state.auth.lock.lock().unwrap();
    let current = state.authenticate(&headers)?;
    if current.generation != session.generation {
        return Err(unauthorized());
    }
    state.store.put(
        "auth",
        "admin",
        &Admin {
            password_hash,
            generation: pier_protocol::new_token(),
            ..admin
        },
    )?;
    state.terminals.revoke_all();
    Ok(cleared(cookie))
}

#[cfg(test)]
pub(crate) mod tests;
