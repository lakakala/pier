use crate::{AgentRecord, Binding, Controller, Job};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use pier_protocol::AgentReport;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;
#[derive(Debug)]
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(_: anyhow::Error) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal operation failed".into(),
        )
    }
}
pub(crate) fn bad(message: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "resource not found".into())
}
pub(crate) fn conflict(message: &str) -> ApiError {
    ApiError(StatusCode::CONFLICT, message.into())
}
fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn admin(State(state): State<Arc<Controller>>, request: Request, next: Next) -> Response {
    if let Err(error) = state.authenticate(request.headers()).and_then(|session| {
        if !matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD
        ) {
            crate::auth::check_write(&state, request.headers(), &session)?;
        }
        Ok(())
    }) {
        return error.into_response();
    }
    next.run(request).await
}
pub fn router(state: Arc<Controller>) -> Router {
    let management = Router::new()
        .route(
            "/v1/enrollments",
            post(enroll).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/v1/enrollments/{id}", get(enrollment))
        .route(
            "/v1/settings",
            get(runtime_settings).put(update_runtime_settings),
        )
        .route("/v1/repository", get(repository).put(update_repository))
        .route("/v1/repository/sync", post(sync))
        .route("/v1/apps", get(apps))
        .route("/v1/blueprints", get(blueprints))
        .route("/v1/agents", post(register).get(agents))
        .route("/v1/agents/{id}", get(agent))
        .route(
            "/v1/agents/{id}/binding",
            put(bind).get(binding).patch(patch_binding),
        )
        .route("/v1/deployments", post(deploy).get(jobs))
        .route("/v1/deployments/{id}", get(job))
        .route("/v1/auth/session", get(crate::auth::session))
        .route("/v1/auth/logout", post(crate::auth::logout))
        .route("/v1/auth/password", post(crate::auth::password))
        .route_layer(middleware::from_fn_with_state(state.clone(), admin));
    Router::new()
        .merge(management)
        .merge(crate::auth::public_routes())
        .route("/v1/artifacts/{deployment}/{app}", get(artifact))
        .fallback(crate::web::serve)
        .with_state(state)
        .layer(middleware::from_fn(no_cache))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingPatch {
    blueprint: String,
    #[serde(deserialize_with = "pier_protocol::unique_map")]
    variables: BTreeMap<String, Option<String>>,
}
async fn patch_binding(
    State(state): State<Arc<Controller>>,
    Path(id): Path<String>,
    Json(patch): Json<BindingPatch>,
) -> ApiResult<Value> {
    let _guard = state.mutation_lock.lock().unwrap();
    let mut binding: Binding = state.store.get("bindings", &id)?.ok_or_else(missing)?;
    if binding.blueprint != patch.blueprint {
        return Err(conflict("binding changed; reload before editing"));
    }
    let catalog = state.catalog.read().unwrap();
    let catalog = catalog
        .as_ref()
        .ok_or_else(|| conflict("catalog not available"))?;
    let blueprint = catalog
        .blueprints
        .get(&binding.blueprint)
        .ok_or_else(missing)?;
    for (key, value) in patch.variables {
        if !blueprint.variables.contains_key(&key) {
            return Err(bad("unknown blueprint variable"));
        }
        if let Some(value) = value {
            binding.variables.insert(key, value);
        } else {
            binding.variables.remove(&key);
        }
    }
    blueprint
        .resolve(&binding.variables)
        .map_err(|e| bad(&e.to_string()))?;
    state.store.put("bindings", &id, &binding)?;
    Ok(Json(
        json!({"agent_id":id,"blueprint":binding.blueprint,"variable_names":binding.variables.keys().collect::<Vec<_>>()}),
    ))
}
async fn no_cache(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
async fn enroll(
    State(state): State<Arc<Controller>>,
    headers: axum::http::HeaderMap,
    Json(request): Json<pier_protocol::enrollment::InitRequest>,
) -> ApiResult<Value> {
    crate::auth::check_origin(&state, &headers)?;
    if !state.agent_ready() {
        return Err(conflict(
            "agent listener unavailable; correct settings and restart controller",
        ));
    }
    let pairing = state
        .approve_enrollment(request)
        .map_err(|_| bad("invalid or already completed enrollment; restart init if necessary"))?;
    Ok(Json(
        json!({"pairing":pairing.encode()?,"id":pairing.grant_id,"expires_at":pairing.expires_at}),
    ))
}
async fn enrollment(
    State(state): State<Arc<Controller>>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    Ok(Json(state.enrollment_status(&id)?.ok_or_else(missing)?))
}
async fn runtime_settings(State(state): State<Arc<Controller>>) -> Json<Value> {
    Json(state.runtime_view())
}
async fn update_runtime_settings(
    State(state): State<Arc<Controller>>,
    Json(patch): Json<crate::runtime::RuntimePatch>,
) -> ApiResult<Value> {
    state.save_runtime(patch)?;
    Ok(Json(state.runtime_view()))
}
async fn repository(State(state): State<Arc<Controller>>) -> Json<Value> {
    Json(state.repository_view())
}
async fn update_repository(
    State(state): State<Arc<Controller>>,
    Json(repository): Json<crate::settings::Repository>,
) -> ApiResult<Value> {
    repository.validate().map_err(|e| bad(&e.to_string()))?;
    let worker = state.clone();
    tokio::task::spawn_blocking(move || worker.save_repository(repository))
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "repository worker failed".into(),
            )
        })??;
    Ok(Json(state.repository_view()))
}
async fn sync(State(state): State<Arc<Controller>>) -> ApiResult<Value> {
    let result = tokio::task::spawn_blocking(move || state.sync())
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "sync worker failed".into(),
            )
        })?;
    let commit = result.map_err(|_| {
        bad("repository sync or catalog validation failed; previous catalog retained")
    })?;
    Ok(Json(json!({"commit": commit})))
}
async fn apps(State(state): State<Arc<Controller>>) -> ApiResult<Value> {
    let catalog = state.catalog.read().unwrap();
    let catalog = catalog
        .as_ref()
        .ok_or_else(|| conflict("catalog not available"))?;
    Ok(Json(json!({"commit":catalog.commit, "apps":catalog.apps})))
}
async fn blueprints(State(state): State<Arc<Controller>>) -> ApiResult<Value> {
    let catalog = state.catalog.read().unwrap();
    let catalog = catalog
        .as_ref()
        .ok_or_else(|| conflict("catalog not available"))?;
    // Return app declarations alongside mappings so clients can inspect every variable.
    Ok(Json(
        json!({"commit":catalog.commit, "blueprints":catalog.blueprints, "apps":catalog.apps}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    name: String,
}
async fn register(
    State(state): State<Arc<Controller>>,
    Json(request): Json<Register>,
) -> ApiResult<Value> {
    if request.name.is_empty() || request.name.len() > 256 {
        return Err(bad("invalid agent name"));
    }
    let id = pier_protocol::new_id();
    let token = pier_protocol::new_token();
    state.store.put(
        "agents",
        &id,
        &AgentRecord {
            id: id.clone(),
            name: request.name,
            token_hash: pier_protocol::hash(&token),
            info: None,
            last_seen: None,
            report: AgentReport::default(),
        },
    )?;
    Ok(Json(json!({"id":id,"token":token})))
}
fn public_agent(state: &Controller, agent: AgentRecord) -> Value {
    let mut value = json!({"id":agent.id,"name":agent.name,"info":agent.info,"last_seen":agent.last_seen,"report":agent.report,"online":state.sessions.lock().unwrap().contains_key(&agent.id)});
    match state.upgrade_view(&agent.id) {
        Ok(upgrade) => value
            .as_object_mut()
            .unwrap()
            .extend(upgrade.as_object().unwrap().clone()),
        Err(_) => {
            value["upgrade"] =
                json!({"target":null,"status":null,"reason":"upgrade status unavailable"});
            value["software"] = Value::Null;
        }
    }
    value
}
async fn agents(State(state): State<Arc<Controller>>) -> ApiResult<Value> {
    Ok(Json(
        json!({"agents":state.store.list::<AgentRecord>("agents")?.into_iter().map(|agent| public_agent(&state,agent)).collect::<Vec<_>>()}),
    ))
}
async fn agent(State(state): State<Arc<Controller>>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(public_agent(
        &state,
        state.store.get("agents", &id)?.ok_or_else(missing)?,
    )))
}
async fn bind(
    State(state): State<Arc<Controller>>,
    Path(id): Path<String>,
    Json(binding): Json<Binding>,
) -> ApiResult<Value> {
    let _guard = state.mutation_lock.lock().unwrap();
    if state.store.get::<AgentRecord>("agents", &id)?.is_none() {
        return Err(missing());
    }
    let catalog = state.catalog.read().unwrap();
    let catalog = catalog
        .as_ref()
        .ok_or_else(|| conflict("catalog not available"))?;
    let blueprint = catalog
        .blueprints
        .get(&binding.blueprint)
        .ok_or_else(missing)?;
    blueprint
        .resolve(&binding.variables)
        .map_err(|e| bad(&e.to_string()))?;
    state.store.put("bindings", &id, &binding)?;
    Ok(Json(
        json!({"agent_id":id,"blueprint":binding.blueprint,"variable_names":binding.variables.keys().collect::<Vec<_>>()}),
    ))
}
async fn binding(State(state): State<Arc<Controller>>, Path(id): Path<String>) -> ApiResult<Value> {
    let binding: Binding = state.store.get("bindings", &id)?.ok_or_else(missing)?;
    Ok(Json(
        json!({"agent_id":id,"blueprint":binding.blueprint,"variable_names":binding.variables.keys().collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Deploy {
    agent_id: String,
    commit: String,
    #[serde(default, deserialize_with = "pier_protocol::unique_map")]
    images: BTreeMap<String, String>,
}
async fn deploy(
    State(state): State<Arc<Controller>>,
    Json(request): Json<Deploy>,
) -> ApiResult<Value> {
    let id;
    {
        let _guard = state.mutation_lock.lock().unwrap();
        if state.upgrade_busy(&request.agent_id)? {
            return Err(conflict("agent is upgrading; wait for it to reconnect"));
        }
        if state.repository_needs_sync() {
            return Err(conflict(
                "repository settings changed or catalog unavailable; sync before deploying",
            ));
        }
        if !state.agent_ready() {
            return Err(conflict(
                "agent listener unavailable; correct settings and restart controller",
            ));
        }
        let runtime = state.active_runtime()?;
        let record: AgentRecord = state
            .store
            .get("agents", &request.agent_id)?
            .ok_or_else(missing)?;
        if !state
            .sessions
            .lock()
            .unwrap()
            .contains_key(&request.agent_id)
        {
            return Err(conflict("agent must be online to start deployment"));
        }
        let info = record
            .info
            .ok_or_else(|| conflict("agent architecture unknown"))?;
        if state
            .store
            .list::<Job>("jobs")?
            .iter()
            .any(|j| j.agent_id == request.agent_id && j.active())
        {
            return Err(conflict("agent already has an active deployment"));
        }
        let catalog = state
            .catalog
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| conflict("catalog not available"))?;
        if catalog.commit != request.commit {
            return Err(conflict(
                "catalog changed; reload and supply current commit",
            ));
        }
        let binding: Binding = state
            .store
            .get("bindings", &request.agent_id)?
            .ok_or_else(|| conflict("agent has no blueprint binding"))?;
        let blueprint = catalog
            .blueprints
            .get(&binding.blueprint)
            .ok_or_else(|| conflict("bound blueprint absent from current catalog"))?;
        blueprint
            .resolve(&binding.variables)
            .map_err(|e| bad(&e.to_string()))?;
        if request
            .images
            .keys()
            .any(|id| !blueprint.apps.iter().any(|a| &a.id == id))
        {
            return Err(bad("image supplied for unknown app instance"));
        }
        for app in &blueprint.apps {
            if (catalog.apps[&app.app].source == pier_pkg::SourceKind::Git)
                != request.images.contains_key(&app.id)
            {
                return Err(bad(
                    "each source app requires an image; binary apps forbid images",
                ));
            }
        }
        id = pier_protocol::new_id();
        let job = Job {
            id: id.clone(),
            agent_id: request.agent_id,
            blueprint: binding.blueprint.clone(),
            commit: catalog.commit.clone(),
            state: "building".into(),
            error: None,
            created_at: pier_protocol::now(),
            plan: None,
            artifacts: BTreeMap::new(),
        };
        state.store.put("jobs", &id, &job)?;
        let worker = state.clone();
        let job_id = id.clone();
        tokio::spawn(async move {
            let _permit = match runtime.build_slots.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            let builder = worker.clone();
            let build_id = job_id.clone();
            let result = tokio::task::spawn_blocking(move || {
                builder.build(
                    &build_id,
                    catalog,
                    binding,
                    request.images,
                    info.architecture,
                    runtime,
                )
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                if let Ok(Some(mut job)) = worker.store.get::<Job>("jobs", &job_id) {
                    job.state = "failed".into();
                    job.error = Some(
                        "package validation or build failed; no server changes applied".into(),
                    );
                    let _ = worker.store.put("jobs", &job_id, &job);
                }
            } else {
                let _ = worker.dispatch(&job_id).await;
            }
        });
    }
    Ok(Json(json!({"id":id,"state":"building"})))
}
fn public_job(job: Job) -> Value {
    json!({"id":job.id,"agent_id":job.agent_id,"blueprint":job.blueprint,"commit":job.commit,"state":job.state,"error":job.error,"created_at":job.created_at,"plan":job.plan})
}
async fn jobs(
    State(state): State<Arc<Controller>>,
    Query(query): Query<BTreeMap<String, String>>,
) -> ApiResult<Value> {
    let jobs = state
        .store
        .list::<Job>("jobs")?
        .into_iter()
        .filter(|j| query.get("agent_id").is_none_or(|id| &j.agent_id == id))
        .map(public_job)
        .collect::<Vec<_>>();
    Ok(Json(json!({"deployments": jobs})))
}
async fn job(State(state): State<Arc<Controller>>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(public_job(
        state.store.get("jobs", &id)?.ok_or_else(missing)?,
    )))
}
async fn artifact(
    State(state): State<Arc<Controller>>,
    Path((deployment, app)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let job: Job = state.store.get("jobs", &deployment)?.ok_or_else(missing)?;
    let agent: AgentRecord = state
        .store
        .get("agents", &job.agent_id)?
        .ok_or_else(missing)?;
    if !bearer(&headers).is_some_and(|t| pier_protocol::token_matches(t, &agent.token_hash)) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()));
    }
    let path = job.artifacts.get(&app).ok_or_else(missing)?;
    let file = tokio::fs::File::open(path).await.map_err(|_| missing())?;
    let size = file.metadata().await.map_err(|_| missing())?.len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn management_authentication_and_artifact_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::Config {
            state_dir: directory.path().join("controller"),
            repository: crate::RepositoryConfig {
                url: "unused".into(),
                reference: "main".into(),
                sync_interval_seconds: 60,
            },
            http_listen: "127.0.0.1:0".parse().unwrap(),
            tcp_listen: crate::auth::tests::free_address(),
            public_url: "https://controller.example.com".into(),
            agent_endpoint: "controller.example.com:7443".into(),
            build_proxy: crate::BuildProxy::default(),
            max_concurrent_builds: 2,
        };
        let state = Controller::open(config).unwrap();
        let owner = AgentRecord {
            id: "owner".into(),
            name: "owner".into(),
            token_hash: pier_protocol::hash("owner-token"),
            info: None,
            last_seen: None,
            report: AgentReport::default(),
        };
        state.store.put("agents", "owner", &owner).unwrap();
        let artifact_path = directory.path().join("package.tar.gz");
        std::fs::write(&artifact_path, b"artifact").unwrap();
        state
            .store
            .put(
                "jobs",
                "job",
                &Job {
                    id: "job".into(),
                    agent_id: "owner".into(),
                    blueprint: "blueprints/web".into(),
                    commit: "commit".into(),
                    state: "ready".into(),
                    error: None,
                    created_at: 0,
                    plan: None,
                    artifacts: BTreeMap::from([("app".into(), artifact_path)]),
                },
            )
            .unwrap();
        let router = router(state);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/artifacts/job/app")
                    .header(header::AUTHORIZATION, "Bearer another-agent-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v1/artifacts/job/app")
                    .header(header::AUTHORIZATION, "Bearer owner-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            "artifact"
        );
    }
}
