use crate::{
    AgentRecord, Binding, Controller, Job, ResolvedBinding,
    api::{ApiResult, bad, conflict, missing},
    catalog::{Blueprint, Catalog},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path, State},
};
use pier_pkg::{Architecture, PortProtocol, Ports};
use pier_protocol::{
    Message,
    forward::{Incoming, Listener, ListenerStatus, Request, Target},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exposure {
    pub tag: String,
    pub port: u16,
}
pub type ExposureMap = BTreeMap<String, BTreeMap<String, Exposure>>;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Deployment {
    #[serde(default)]
    pub ports: BTreeMap<String, Ports>,
    #[serde(default)]
    pub exposures: ExposureMap,
}
#[derive(Clone, Serialize, Deserialize)]
struct Active {
    agent: String,
    blueprint: String,
    deployment: String,
    network: Deployment,
}

#[derive(Clone, Serialize)]
struct RouteView {
    id: String,
    agent_id: String,
    blueprint: String,
    deployment: String,
    app: String,
    name: String,
    tag: String,
    protocol: PortProtocol,
    port: u16,
    target_port: u16,
    ingress_id: Option<String>,
    ingress_name: Option<String>,
    state: String,
    reason: Option<String>,
}
struct Route {
    view: RouteView,
    target: Target,
    ready: bool,
    cancelled: CancellationToken,
    slots: Arc<Semaphore>,
}
#[derive(Default)]
struct StateData {
    routes: BTreeMap<String, Route>,
    configured: BTreeMap<String, (String, Vec<Listener>)>,
    statuses: BTreeMap<(String, String), ListenerStatus>,
    errors: BTreeMap<String, (Instant, String)>,
}
pub(crate) struct Registry {
    data: Mutex<StateData>,
    started: AtomicBool,
    wake: Arc<Notify>,
    slots: Arc<Semaphore>,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            data: Mutex::new(StateData::default()),
            started: AtomicBool::new(false),
            wake: Arc::new(Notify::new()),
            slots: Arc::new(Semaphore::new(1024)),
        }
    }
}

pub(crate) fn resolve(
    binding: &Binding,
    blueprint: &Blueprint,
    resolved: &ResolvedBinding,
    catalog: &Catalog,
    architecture: Architecture,
) -> Result<Deployment> {
    let mut ports = BTreeMap::new();
    for app in &blueprint.apps {
        let metadata = catalog
            .apps
            .get(&app.app)
            .context("app metadata missing from catalog")?;
        let mut variables: BTreeMap<String, String> = metadata
            .variables
            .iter()
            .filter_map(|(name, def)| def.default.clone().map(|value| (name.clone(), value)))
            .collect();
        variables.extend(resolved.variables[&app.id].clone());
        variables.insert("PIER_ARCH".into(), architecture.to_string());
        ports.insert(
            app.id.clone(),
            pier_pkg::resolve_ports(&metadata.ports, &variables)?,
        );
    }
    let network = Deployment {
        ports,
        exposures: binding.exposures.clone(),
    };
    for (app, exposures) in &network.exposures {
        let declared = network
            .ports
            .get(app)
            .context("exposure references unknown app")?;
        for (name, exposure) in exposures {
            ensure!(
                declared.contains_key(name),
                "exposure references unknown port: {app}/{name}"
            );
            ensure!(
                exposure.port > 0 && valid_tag(&exposure.tag),
                "invalid exposure port or tag"
            );
        }
    }
    Ok(network)
}
fn valid_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.len() <= 80 && tag.trim() == tag && !tag.chars().any(char::is_control)
}

/// Called under mutation_lock, including the candidate and all in-flight reservations.
pub(crate) fn check_conflicts(
    state: &Controller,
    candidate: Option<(&str, &str, &Deployment)>,
    tags: Option<(&str, &[String])>,
) -> Result<()> {
    let mut specs: Vec<Active> = state.store.list("port_deployments")?;
    for job in state
        .store
        .list::<Job>("jobs")?
        .into_iter()
        .filter(Job::active)
    {
        if job.action == pier_protocol::DeploymentAction::Deploy {
            specs.push(Active {
                agent: job.agent_id,
                blueprint: job.blueprint,
                deployment: job.id,
                network: job.network,
            });
        }
    }
    if let Some((agent, blueprint, network)) = candidate {
        specs.push(Active {
            agent: agent.into(),
            blueprint: blueprint.into(),
            deployment: String::new(),
            network: network.clone(),
        });
    }
    let mut agents = state.store.list::<AgentRecord>("agents")?;
    if let Some((id, tags)) = tags {
        if let Some(agent) = agents.iter_mut().find(|a| a.id == id) {
            agent.tags = tags.to_vec();
        }
    }
    let mut claimed = BTreeMap::new();
    for spec in &specs {
        for (app, exposures) in &spec.network.exposures {
            for (name, exposure) in exposures {
                let port = spec
                    .network
                    .ports
                    .get(app)
                    .and_then(|ports| ports.get(name))
                    .context("invalid saved exposure")?;
                let owner = format!("{}/{}/{}/{}", spec.agent, spec.blueprint, app, name);
                for ingress in agents
                    .iter()
                    .filter(|agent| agent.tags.contains(&exposure.tag))
                {
                    let key = (ingress.id.clone(), port.protocol, exposure.port);
                    if let Some(previous) = claimed.insert(key, owner.clone()) {
                        ensure!(
                            previous == owner,
                            "port {} {:?} on agent {} is reserved by {}",
                            exposure.port,
                            port.protocol,
                            ingress.id,
                            previous
                        );
                    }
                    for local in specs.iter().filter(|local| local.agent == ingress.id) {
                        ensure!(
                            !local
                                .network
                                .ports
                                .values()
                                .flat_map(|ports| ports.values())
                                .any(|target| target.port == exposure.port
                                    && target.protocol == port.protocol),
                            "port {} {:?} on agent {} conflicts with a declared local app port",
                            exposure.port,
                            port.protocol,
                            ingress.id
                        );
                    }
                }
            }
        }
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for (agent, _, _) in claimed.keys() {
        *counts.entry(agent.clone()).or_default() += 1;
    }
    ensure!(
        counts.values().all(|count| *count <= 1024),
        "at most 1024 exposed ports per ingress agent"
    );
    Ok(())
}

/// The installed deployment ID is authoritative, including recovery and rollback.
pub(crate) fn report(
    state: &Controller,
    agent: &str,
    report: &pier_protocol::AgentReport,
) -> Result<()> {
    for blueprint in &report.blueprints {
        let key = format!("{agent}:{}", blueprint.id);
        let Some(id) = &blueprint.deployment_id else {
            continue;
        };
        let Some(job) = state.store.get::<Job>("jobs", id)? else {
            continue;
        };
        ensure!(
            job.agent_id == agent && job.blueprint == blueprint.blueprint,
            "reported deployment belongs to another blueprint"
        );
        if blueprint.state == "stopped" && job.action == pier_protocol::DeploymentAction::Stop {
            state.store.delete("port_deployments", &key)?;
        } else if job.action == pier_protocol::DeploymentAction::Deploy {
            state.store.put(
                "port_deployments",
                &key,
                &Active {
                    agent: agent.into(),
                    blueprint: job.blueprint,
                    deployment: id.clone(),
                    network: job.network,
                },
            )?;
        }
    }
    state.forwarding.wake.notify_one();
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tags {
    tags: Vec<String>,
}
pub(crate) async fn update_tags(
    State(state): State<Arc<Controller>>,
    Path(id): Path<String>,
    Json(request): Json<Tags>,
) -> ApiResult<Value> {
    let _guard = state.mutation_lock.lock().unwrap();
    let mut record: AgentRecord = state.store.get("agents", &id)?.ok_or_else(missing)?;
    let tags: Vec<String> = request
        .tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if tags.len() > 64 || tags.iter().any(|tag| !valid_tag(tag)) {
        return Err(bad(
            "tags must be nonempty, at most 80 bytes each and at most 64 per agent",
        ));
    }
    check_conflicts(&state, None, Some((&id, &tags))).map_err(|e| conflict(&e.to_string()))?;
    record.tags = tags;
    state.store.put("agents", &id, &record)?;
    state.forwarding.wake.notify_one();
    Ok(Json(json!({"tags":record.tags})))
}
pub(crate) async fn list(
    State(state): State<Arc<Controller>>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    if state.store.get::<AgentRecord>("agents", &id)?.is_none() {
        return Err(missing());
    }
    reconcile(&state)?;
    let data = state.forwarding.data.lock().unwrap();
    let rows: Vec<_> = data
        .routes
        .values()
        .filter(|r| r.view.agent_id == id || r.view.ingress_id.as_deref() == Some(&id))
        .map(|r| {
            let mut row = r.view.clone();
            if r.ready {
                if let Some(status) = row
                    .ingress_id
                    .as_ref()
                    .and_then(|ingress| data.statuses.get(&(ingress.clone(), row.id.clone())))
                {
                    row.state = status.state.clone();
                    row.reason = status.reason.clone();
                }
                if let Some((time, error)) = data
                    .errors
                    .get(&row.id)
                    .filter(|(time, _)| time.elapsed() < Duration::from_secs(10))
                {
                    let _ = time;
                    row.state = "error".into();
                    row.reason = Some(error.clone());
                }
            }
            row
        })
        .collect();
    Ok(Json(json!({"exposures":rows})))
}

pub(crate) fn start(state: &Arc<Controller>) {
    state.forwarding.wake.notify_one();
    if state.forwarding.started.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else {
                break;
            };
            if reconcile(&state).is_err() {
                tracing::warn!("port exposure reconciliation failed");
            }
            let wake = state.forwarding.wake.clone();
            drop(state);
            tokio::select! {
                _ = wake.notified() => (),
                _ = tokio::time::sleep(Duration::from_secs(2)) => (),
            }
        }
    });
}
fn reconcile(state: &Controller) -> Result<()> {
    let _guard = state.mutation_lock.lock().unwrap();
    let active = state.store.list::<Active>("port_deployments")?;
    let agents: BTreeMap<_, _> = state
        .store
        .list::<AgentRecord>("agents")?
        .into_iter()
        .map(|a| (a.id.clone(), a))
        .collect();
    let jobs = state.store.list::<Job>("jobs")?;
    let sessions = state.sessions.lock().unwrap();
    let mut data = state.forwarding.data.lock().unwrap();
    let mut next = BTreeMap::new();
    let mut configs: BTreeMap<String, Vec<Listener>> = BTreeMap::new();
    for spec in active {
        for (app, exposures) in &spec.network.exposures {
            for (name, exposure) in exposures {
                let Some(port) = spec
                    .network
                    .ports
                    .get(app)
                    .and_then(|ports| ports.get(name))
                else {
                    continue;
                };
                let instance = pier_protocol::hash(format!("{}\0{app}", spec.blueprint));
                let target = Target {
                    blueprint: spec.blueprint.clone(),
                    deployment: spec.deployment.clone(),
                    instance: instance.clone(),
                    name: name.clone(),
                    protocol: port.protocol,
                    port: port.port,
                };
                let backend_reason = if sessions
                    .get(&spec.agent)
                    .is_none_or(|s| s.forward.is_none())
                {
                    Some("应用 agent 离线或不支持端口转发")
                } else if state.upgrade_busy(&spec.agent)? {
                    Some("应用 agent 正在升级")
                } else if jobs.iter().any(|j| {
                    j.agent_id == spec.agent
                        && j.blueprint == spec.blueprint
                        && matches!(j.state.as_str(), "applying" | "rolling_back")
                }) {
                    Some("应用正在切换部署")
                } else if !agents.get(&spec.agent).is_some_and(|a| {
                    a.report.blueprints.iter().any(|b| {
                        b.deployment_id.as_ref() == Some(&spec.deployment)
                            && b.apps
                                .iter()
                                .any(|app| app.instance == instance && app.state == "running")
                    })
                }) {
                    Some("目标应用尚未运行")
                } else {
                    None
                };
                let matching: Vec<_> = agents
                    .values()
                    .filter(|agent| agent.tags.contains(&exposure.tag))
                    .map(Some)
                    .collect();
                let matching = if matching.is_empty() {
                    vec![None]
                } else {
                    matching
                };
                for ingress in matching {
                    let id = pier_protocol::hash(format!(
                        "{}:{}:{app}:{name}:{}",
                        spec.agent,
                        spec.deployment,
                        ingress.map_or("", |agent| agent.id.as_str())
                    ));
                    let reason = if let Some(ingress) = ingress {
                        if !sessions.contains_key(&ingress.id) {
                            Some("入口 agent 离线")
                        } else if sessions[&ingress.id].forward.is_none() {
                            Some("入口 agent 不支持 port_forward_v1，请升级")
                        } else if state.upgrade_busy(&ingress.id)? {
                            Some("入口 agent 正在升级")
                        } else {
                            backend_reason
                        }
                    } else {
                        Some("没有匹配标签的 agent")
                    };
                    let ready = reason.is_none();
                    let view = RouteView {
                        id: id.clone(),
                        agent_id: spec.agent.clone(),
                        blueprint: spec.blueprint.clone(),
                        deployment: spec.deployment.clone(),
                        app: app.clone(),
                        name: name.clone(),
                        tag: exposure.tag.clone(),
                        protocol: port.protocol,
                        port: exposure.port,
                        target_port: port.port,
                        ingress_id: ingress.map(|a| a.id.clone()),
                        ingress_name: ingress.map(|a| a.name.clone()),
                        state: "pending".into(),
                        reason: reason.map(str::to_string),
                    };
                    let old = data.routes.remove(&id);
                    let (cancelled, slots) = match old {
                        Some(old) if old.ready == ready && !old.cancelled.is_cancelled() => {
                            (old.cancelled, old.slots)
                        }
                        Some(old) => {
                            old.cancelled.cancel();
                            (CancellationToken::new(), Arc::new(Semaphore::new(64)))
                        }
                        None => (CancellationToken::new(), Arc::new(Semaphore::new(64))),
                    };
                    if ready {
                        configs
                            .entry(ingress.unwrap().id.clone())
                            .or_default()
                            .push(Listener {
                                id: id.clone(),
                                protocol: port.protocol,
                                port: exposure.port,
                            });
                    }
                    next.insert(
                        id,
                        Route {
                            view,
                            target: target.clone(),
                            ready,
                            cancelled,
                            slots,
                        },
                    );
                }
            }
        }
    }
    for old in data.routes.values() {
        old.cancelled.cancel();
    }
    data.routes = next;
    let ids: BTreeSet<_> = data.routes.keys().cloned().collect();
    data.statuses.retain(|(_, id), _| ids.contains(id));
    data.errors
        .retain(|id, (time, _)| ids.contains(id) && time.elapsed() < Duration::from_secs(10));
    for (agent, session) in sessions.iter().filter(|(_, s)| s.forward.is_some()) {
        let config = configs.remove(agent).unwrap_or_default();
        if data
            .configured
            .get(agent)
            .is_some_and(|(id, old)| id == &session.id && old == &config)
        {
            continue;
        }
        if session
            .sender
            .try_send(Message::PortConfig {
                listeners: config.clone(),
            })
            .is_ok()
        {
            data.statuses.retain(|(id, _), _| id != agent);
            data.configured
                .insert(agent.clone(), (session.id.clone(), config));
        }
    }
    data.configured.retain(|id, _| sessions.contains_key(id));
    Ok(())
}
pub(crate) fn status(state: &Controller, agent: &str, statuses: Vec<ListenerStatus>) -> Result<()> {
    ensure!(statuses.len() <= 1024, "too many listener statuses");
    let mut data = state.forwarding.data.lock().unwrap();
    for mut status in statuses {
        ensure!(
            matches!(status.state.as_str(), "pending" | "ready" | "error"),
            "invalid listener state"
        );
        if data
            .routes
            .get(&status.id)
            .is_none_or(|r| r.view.ingress_id.as_deref() != Some(agent))
        {
            continue;
        }
        status.reason = status
            .reason
            .map(|reason| reason.chars().take(256).collect());
        data.statuses
            .insert((agent.into(), status.id.clone()), status);
    }
    Ok(())
}
pub(crate) fn disconnect(state: &Controller, agent: &str) {
    let mut data = state.forwarding.data.lock().unwrap();
    for route in data
        .routes
        .values()
        .filter(|r| r.view.agent_id == agent || r.view.ingress_id.as_deref() == Some(agent))
    {
        route.cancelled.cancel();
    }
    data.configured.remove(agent);
    data.statuses.retain(|(id, _), _| id != agent);
    state.forwarding.wake.notify_one();
}
pub(crate) fn serve(
    state: Arc<Controller>,
    agent: String,
    mut incoming: tokio::sync::mpsc::Receiver<Incoming>,
    cancelled: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let request = tokio::select! { _ = cancelled.cancelled() => break, value = incoming.recv() => match value { Some(value) => value, None => break } };
            let state = state.clone();
            let agent = agent.clone();
            let cancelled = cancelled.clone();
            tokio::spawn(async move {
                if let Err(error) = relay(&state, &agent, request, cancelled).await {
                    tracing::debug!(%error, "port relay closed");
                }
            });
        }
    });
}
async fn relay(
    state: &Controller,
    agent: &str,
    mut incoming: Incoming,
    disconnected: CancellationToken,
) -> Result<()> {
    let setup = (|| -> Result<_> {
        let Request::Ingress { route } = &incoming.request else {
            anyhow::bail!("ingress request required");
        };
        let sessions = state.sessions.lock().unwrap();
        let data = state.forwarding.data.lock().unwrap();
        let entry = data
            .routes
            .get(route)
            .context("exposure no longer active")?;
        ensure!(
            entry.ready
                && !entry.cancelled.is_cancelled()
                && entry.view.ingress_id.as_deref() == Some(agent),
            "exposure unavailable or not assigned to agent"
        );
        let target = sessions
            .get(&entry.view.agent_id)
            .and_then(|s| s.forward.clone())
            .context("target disconnected")?;
        Ok((
            route.clone(),
            target,
            entry.target.clone(),
            entry.cancelled.clone(),
            entry
                .slots
                .clone()
                .try_acquire_owned()
                .context("entrance connection limit")?,
            state
                .forwarding
                .slots
                .clone()
                .try_acquire_owned()
                .context("controller forwarding connection limit")?,
        ))
    })();
    let (route, mux, target, cancelled, _route_slot, _slot) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            incoming.reject(&error.to_string());
            return Err(error);
        }
    };
    tokio::select! {
        _ = disconnected.cancelled() => Ok(()),
        _ = cancelled.cancelled() => Ok(()),
        result = async {
            let mut backend = match mux.open(Request::Target { target }).await {
                Ok(backend) => backend,
                Err(error) => {
                    let reason = format!("后端连接失败：{error}");
                    incoming.reject(&reason);
                    state.forwarding.data.lock().unwrap().errors.insert(route.clone(), (Instant::now(), reason));
                    return Err(error);
                }
            };
            state.forwarding.data.lock().unwrap().errors.remove(&route);
            incoming.accept().await?;
            tokio::io::copy_bidirectional(&mut incoming.stream, &mut backend).await?;
            Ok(())
        } => result,
    }
}

#[cfg(test)]
mod tests;
