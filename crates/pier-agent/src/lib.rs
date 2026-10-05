//! Linux deployment agent and process supervisor.
#[cfg(not(target_os = "linux"))]
compile_error!("pier-agent currently requires Linux");

mod account;
pub mod init;
mod network;
mod supervisor;
mod systemd;
mod terminal;
mod transport;
pub mod upgrade;

use account::Account;
use anyhow::{Result, ensure};
use pier_pkg::{Architecture, PackageManifest};
use pier_protocol::{
    AgentReport, BlueprintStatus, DeploymentAction, DeploymentPlan, DeploymentResult, Message,
    store::Store,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use supervisor::Supervisor;

fn startup_grace() -> u64 {
    10
}
fn stop_timeout() -> u64 {
    30
}
fn heartbeat() -> u64 {
    15
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOptions {
    #[serde(default = "startup_grace")]
    pub startup_grace_seconds: u64,
    #[serde(default = "stop_timeout")]
    pub stop_timeout_seconds: u64,
}
impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            startup_grace_seconds: 10,
            stop_timeout_seconds: 30,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub agent_id: String,
    pub token_file: PathBuf,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub controller_tcp: String,
    #[serde(
        default,
        skip_serializing_if = "pier_protocol::connection::ConnectionMode::is_default"
    )]
    pub connection_mode: pier_protocol::connection::ConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<std::net::SocketAddr>,
    pub state_dir: PathBuf,
    #[serde(default = "heartbeat")]
    pub heartbeat_seconds: u64,
    #[serde(default)]
    pub runtime: RuntimeOptions,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installed {
    pub instance: String,
    pub id: String,
    pub sha256: String,
    pub release: PathBuf,
    pub account: Account,
    pub logs: PathBuf,
    pub manifest: PackageManifest,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Snapshot {
    blueprint: String,
    name: String,
    result: Option<DeploymentResult>,
    deployment_id: Option<String>,
    apps: Vec<Installed>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    id: String,
    fingerprint: String,
    blueprint_id: String,
    before: Snapshot,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DurableState {
    #[serde(default)]
    blueprints: BTreeMap<String, Snapshot>,
    pending: Option<Pending>,
    result: Option<DeploymentResult>,
}
#[derive(Serialize, Deserialize)]
struct Completed {
    fingerprint: String,
    result: DeploymentResult,
}

pub struct Runtime {
    transport: Arc<transport::Transport>,
    terminals: Arc<terminal::Manager>,
    _lock: fs::File,
    config: Config,
    store: Store,
    active: Mutex<BTreeMap<String, Supervisor>>,
    operation: Mutex<()>,
    busy_blueprint: Mutex<Option<String>>,
    shutting_down: AtomicBool,
    maintenance: AtomicBool,
    events: tokio::sync::mpsc::UnboundedSender<Message>,
}
struct BusyBlueprint<'a>(&'a Mutex<Option<String>>);
impl Drop for BusyBlueprint<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}
pub fn architecture() -> Result<Architecture> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(Architecture::Amd64),
        "aarch64" => Ok(Architecture::Arm64),
        _ => anyhow::bail!("unsupported host architecture"),
    }
}
impl Runtime {
    pub fn open(
        config: Config,
        events: tokio::sync::mpsc::UnboundedSender<Message>,
    ) -> Result<Arc<Self>> {
        // SAFETY: geteuid has no preconditions.
        ensure!(
            unsafe { libc::geteuid() } == 0,
            "pier-agent requires root to create app users"
        );
        ensure!(pier_protocol::safe_id(&config.agent_id), "invalid agent id");
        ensure!(
            config.runtime.startup_grace_seconds > 0 && config.runtime.startup_grace_seconds <= 300,
            "startup grace must be 1..300 seconds"
        );
        ensure!(
            config.runtime.stop_timeout_seconds <= 300,
            "stop timeout must be at most 300 seconds"
        );
        ensure!(
            (1..=30).contains(&config.heartbeat_seconds),
            "heartbeat interval must be 1..30 seconds"
        );
        match config.connection_mode {
            pier_protocol::connection::ConnectionMode::AgentToController => {
                pier_protocol::enrollment::endpoint(&config.controller_tcp)?;
                ensure!(config.listen.is_none(), "active agent cannot listen");
            }
            pier_protocol::connection::ConnectionMode::ControllerToAgent => {
                ensure!(
                    config.controller_tcp.is_empty() && config.listen.is_some_and(|a| a.port() > 0),
                    "passive agent requires listen and forbids controller_tcp"
                );
            }
        }
        ensure!(
            fs::read_to_string(&config.token_file)?.trim().len() >= 32,
            "agent token must contain at least 32 characters"
        );
        architecture()?;
        fs::create_dir_all(&config.state_dir)?;
        let lock = pier_protocol::state_lock(&config.state_dir)?;
        // App users need traversal to their own protected app directory.
        fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o711))?;
        account::directory(&config.state_dir.join("blueprints"), 0, 0, 0o711)?;
        account::directory(&config.state_dir.join("downloads"), 0, 0, 0o700)?;
        let store = Store::open(&config.state_dir.join("agent.db"))?;
        let legacy = store.get::<serde_json::Value>("runtime", "state")?;
        ensure!(
            store.list::<serde_json::Value>("accounts")?.is_empty()
                && legacy.as_ref().is_none_or(|value| {
                    value.get("snapshot").is_none()
                        || (value["snapshot"]["apps"]
                            .as_array()
                            .is_some_and(Vec::is_empty)
                            && value["pending"].is_null())
                }),
            "legacy per-app deployment state requires manual backup and removal; automatic account/data migration is not supported"
        );
        let identity: Option<String> = store.get("identity", "agent_id")?;
        ensure!(
            identity.as_ref().is_none_or(|id| id == &config.agent_id),
            "state directory belongs to another agent identity"
        );
        store.put("identity", "agent_id", &config.agent_id)?;
        let state = Arc::new(Self {
            transport: transport::Transport::new(config.clone(), events.clone()),
            terminals: Arc::new(terminal::Manager::default()),
            _lock: lock,
            config,
            store,
            active: Mutex::new(BTreeMap::new()),
            operation: Mutex::new(()),
            busy_blueprint: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            maintenance: AtomicBool::new(false),
            events,
        });
        for account in state.store.list::<Account>("blueprint_accounts")? {
            if account::cleanup(&account).is_err() {
                tracing::warn!(user=%account.name, "blueprint account verification or cleanup failed");
            }
        }
        let mut durable: DurableState = state.store.get("runtime", "state")?.unwrap_or_default();
        let interrupted = durable.pending.clone();
        if let Some(pending) = &interrupted {
            durable
                .blueprints
                .insert(pending.blueprint_id.clone(), pending.before.clone());
        }
        let mut interrupted_restored = true;
        for (id, snapshot) in &durable.blueprints {
            let observe = interrupted.as_ref().is_some_and(|p| p.blueprint_id == *id);
            let restored = state.restore(id, snapshot, observe);
            if observe {
                interrupted_restored = restored.is_ok();
            }
        }
        if let Some(pending) = interrupted {
            let result = DeploymentResult {
                id: pending.id.clone(),
                state: if interrupted_restored {
                    "rolled_back"
                } else {
                    "rollback_failed"
                }
                .into(),
                error: Some(
                    "agent restarted during deployment; restored previous blueprint snapshot"
                        .into(),
                ),
            };
            durable.pending = None;
            durable.result = Some(result.clone());
            durable
                .blueprints
                .get_mut(&pending.blueprint_id)
                .unwrap()
                .result = Some(result.clone());
            state.store.put_pair(
                ("runtime", "state", &durable),
                (
                    "completed",
                    &pending.id,
                    &Completed {
                        fingerprint: pending.fingerprint,
                        result,
                    },
                ),
            )?;
        } else {
            state.store.put("runtime", "state", &durable)?;
        }
        Ok(state)
    }
    fn stopped(&self) -> Result<()> {
        ensure!(
            !self.shutting_down.load(Ordering::SeqCst),
            "agent shutting down"
        );
        Ok(())
    }
    fn stop_blueprint(&self, id: &str) -> Result<()> {
        let supervisor = self.active.lock().unwrap().remove(id);
        if let Some(mut supervisor) = supervisor {
            supervisor.stop();
        }
        if let Some(account) = self.store.get::<Account>("blueprint_accounts", id)? {
            account::cleanup(&account)?;
        }
        Ok(())
    }
    fn stop_all(&self) {
        let active = std::mem::take(&mut *self.active.lock().unwrap());
        for supervisor in active.values() {
            supervisor.request_stop();
        }
        for (_, mut supervisor) in active {
            supervisor.stop();
        }
    }
    fn start(&self, id: &str, snapshot: &Snapshot, observe: bool) -> Result<()> {
        self.stopped()?;
        if snapshot.apps.is_empty() {
            return Ok(());
        }
        let supervisor = Supervisor::start(
            snapshot.apps.clone(),
            self.config.runtime.clone(),
            observe,
            self.terminals.clone(),
        );
        let status = supervisor.status.clone();
        self.active.lock().unwrap().insert(id.into(), supervisor);
        if observe {
            let deadline = std::time::Instant::now()
                + Duration::from_secs(self.config.runtime.startup_grace_seconds + 10);
            loop {
                self.stopped()?;
                let statuses = status.lock().unwrap();
                if statuses.iter().all(|s| s.state == "running") {
                    return Ok(());
                }
                ensure!(
                    statuses.iter().all(|s| s.state != "failed")
                        && std::time::Instant::now() < deadline,
                    "blueprint failed startup observation"
                );
                drop(statuses);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    }
    fn restore(&self, id: &str, snapshot: &Snapshot, observe: bool) -> Result<()> {
        self.stop_blueprint(id)?;
        let result = self.start(id, snapshot, observe);
        if result.is_err() {
            let _ = self.stop_blueprint(id);
            if !self.shutting_down.load(Ordering::SeqCst) {
                let _ = self.start(id, snapshot, false);
            }
        }
        result
    }
    pub fn report(&self) -> Result<AgentReport> {
        let durable: DurableState = self.store.get("runtime", "state")?.unwrap_or_default();
        let active = self.active.lock().unwrap();
        let mut blueprints = Vec::new();
        for (id, snapshot) in &durable.blueprints {
            let apps = active
                .get(id)
                .map(|s| s.status.lock().unwrap().clone())
                .unwrap_or_default();
            let state = if snapshot.apps.is_empty() {
                "stopped"
            } else if apps.is_empty() {
                "failed"
            } else if apps.iter().all(|app| app.state == "running") {
                "running"
            } else if apps.iter().any(|app| app.state == "failed") {
                "failed"
            } else if apps.iter().any(|app| app.state == "backoff") {
                "backoff"
            } else {
                "starting"
            };
            let account = self.store.get::<Account>("blueprint_accounts", id)?;
            blueprints.push(BlueprintStatus {
                account_reserved: account.is_some(),
                id: id.clone(),
                blueprint: snapshot.blueprint.clone(),
                name: account.map_or_else(|| snapshot.name.clone(), |account| account.name),
                deployment_id: snapshot.deployment_id.clone(),
                state: state.into(),
                apps,
                result: snapshot.result.clone(),
            });
        }
        Ok(AgentReport {
            capabilities: vec![
                pier_protocol::terminal::CAPABILITY.into(),
                pier_protocol::MULTI_BLUEPRINT_CAPABILITY.into(),
            ],
            deployment_id: if blueprints.len() == 1 {
                blueprints[0].deployment_id.clone()
            } else {
                None
            },
            apps: blueprints.iter().flat_map(|b| b.apps.clone()).collect(),
            blueprints,
            result: durable.result,
        })
    }
    pub fn shutdown(&self) {
        {
            let _busy = self.busy_blueprint.lock().unwrap();
            self.shutting_down.store(true, Ordering::SeqCst);
            self.terminals.close_all("agent_stopping");
        }
        // Stop running apps even if a deployment is waiting on a network read.
        self.stop_all();
        let _guard = self.operation.lock().unwrap();
        self.stop_all();
        if let Ok(accounts) = self.store.list::<Account>("blueprint_accounts") {
            for account in accounts {
                let _ = account::cleanup(&account);
            }
        }
    }
    pub(crate) fn enter_upgrade(&self) -> Result<bool> {
        let Ok(_guard) = self.operation.try_lock() else {
            return Ok(false);
        };
        if self.shutting_down.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let durable: DurableState = self.store.get("runtime", "state")?.unwrap_or_default();
        if durable.pending.is_some() {
            return Ok(false);
        }
        let _busy = self.busy_blueprint.lock().unwrap();
        self.maintenance.store(true, Ordering::SeqCst);
        self.terminals.close_all("agent_upgrading");
        Ok(true)
    }

    pub fn apply(&self, plan: DeploymentPlan) -> Result<DeploymentResult> {
        let _guard = self.operation.lock().unwrap();
        self.stopped()?;
        ensure!(
            !self.maintenance.load(Ordering::SeqCst),
            "agent upgrade in progress"
        );
        ensure!(
            pier_protocol::safe_id(&plan.id)
                && plan.agent_id == self.config.agent_id
                && plan.architecture == architecture()?,
            "deployment identity or architecture mismatch"
        );
        pier_protocol::relative(&plan.blueprint)?;
        ensure!(plan.apps.len() <= 1024, "too many app instances");
        let fingerprint = pier_protocol::hash(serde_json::to_vec(&plan)?);
        if let Some(completed) = self.store.get::<Completed>("completed", &plan.id)? {
            ensure!(
                completed.fingerprint == fingerprint,
                "deployment id reused with different content"
            );
            return Ok(completed.result);
        }
        let mut durable: DurableState = self.store.get("runtime", "state")?.unwrap_or_default();
        ensure!(
            durable.pending.is_none(),
            "unfinished local transaction; restart agent to recover before deploying"
        );
        let mut ids = BTreeSet::new();
        for app in &plan.apps {
            ensure!(
                pier_protocol::safe_id(&app.id) && ids.insert(&app.id),
                "invalid or duplicate app id"
            );
            ensure!(
                app.instance == pier_protocol::hash(format!("{}\0{}", plan.blueprint, app.id)),
                "invalid instance identity"
            );
            ensure!(
                app.sha256.len() == 64
                    && app.sha256.bytes().all(|c| c.is_ascii_hexdigit())
                    && app.size <= 10 * 1024 * 1024 * 1024,
                "invalid artifact metadata"
            );
        }
        let blueprint_id = pier_protocol::hash(&plan.blueprint);
        ensure!(
            !plan.blueprint_name.is_empty(),
            "controller must support blueprint accounts"
        );
        ensure!(
            plan.action != DeploymentAction::Stop || plan.apps.is_empty(),
            "stop plan cannot contain apps"
        );
        *self.busy_blueprint.lock().unwrap() = Some(blueprint_id.clone());
        let _busy = BusyBlueprint(&self.busy_blueprint);
        let before = durable
            .blueprints
            .get(&blueprint_id)
            .cloned()
            .unwrap_or_else(|| Snapshot {
                blueprint: plan.blueprint.clone(),
                name: plan.blueprint_name.clone(),
                ..Snapshot::default()
            });
        let _ = self.events.send(Message::Progress {
            id: plan.id.clone(),
            phase: "downloading".into(),
        });
        let result = match self.prepare(&plan) {
            Err(error) => DeploymentResult {
                id: plan.id.clone(),
                state: "failed".into(),
                error: Some(
                    match error.downcast_ref::<pier_protocol::BlueprintAccountError>() {
                        Some(reason) => reason.deployment_error(),
                        None => "artifact preparation failed; existing deployment retained".into(),
                    },
                ),
            },
            Ok(candidate) => {
                self.stopped()?;
                self.terminals
                    .close_blueprint(&blueprint_id, "deployment_started");
                durable.pending = Some(Pending {
                    id: plan.id.clone(),
                    fingerprint: fingerprint.clone(),
                    blueprint_id: blueprint_id.clone(),
                    before: before.clone(),
                });
                self.store.put("runtime", "state", &durable)?;
                let _ = self.events.send(Message::Progress {
                    id: plan.id.clone(),
                    phase: "applying".into(),
                });
                if self
                    .stop_blueprint(&blueprint_id)
                    .and_then(|()| self.start(&blueprint_id, &candidate, true))
                    .is_ok()
                {
                    durable.blueprints.insert(blueprint_id.clone(), candidate);
                    DeploymentResult {
                        id: plan.id.clone(),
                        state: "succeeded".into(),
                        error: None,
                    }
                } else {
                    let _ = self.events.send(Message::Progress {
                        id: plan.id.clone(),
                        phase: "rolling_back".into(),
                    });
                    let restored = self.restore(&blueprint_id, &before, true);
                    DeploymentResult {
                        id: plan.id.clone(),
                        state: if restored.is_ok() {
                            "rolled_back"
                        } else {
                            "rollback_failed"
                        }
                        .into(),
                        error: Some(
                            "app startup failed; restoring the previous blueprint deployment"
                                .into(),
                        ),
                    }
                }
            }
        };
        let pending = durable.pending.take();
        durable.result = Some(result.clone());
        durable
            .blueprints
            .entry(blueprint_id.clone())
            .or_insert(before.clone())
            .result = Some(result.clone());
        if let Err(error) = self.store.put_pair(
            ("runtime", "state", &durable),
            (
                "completed",
                &plan.id,
                &Completed {
                    fingerprint,
                    result: result.clone(),
                },
            ),
        ) {
            if let Some(pending) = pending {
                let _ = self.restore(&blueprint_id, &pending.before, false);
            }
            return Err(error);
        }
        Ok(result)
    }
    fn prepare(&self, plan: &DeploymentPlan) -> Result<Snapshot> {
        let workspace = tempfile::tempdir_in(self.config.state_dir.join("downloads"))?;
        let mut downloaded = Vec::new();
        // All archives are verified before creating users or stopping any service.
        for app in &plan.apps {
            self.stopped()?;
            let package = workspace.path().join(format!("{}.tar.gz", app.id));
            network::download(
                &self.transport,
                &plan.id,
                app,
                &package,
                &self.shutting_down,
            )?;
            let extracted = workspace.path().join(&app.id);
            let manifest = pier_pkg::unpack(&package, &extracted, &app.sha256, plan.architecture)?;
            ensure!(
                !manifest.service.env.contains_key("PIER_DATA_DIR")
                    && !manifest.service.env.contains_key("PIER_LOG_DIR"),
                "service overrides reserved agent environment variable"
            );
            downloaded.push((app, extracted, manifest));
        }
        let id = pier_protocol::hash(&plan.blueprint);
        let mut snapshot = Snapshot {
            blueprint: plan.blueprint.clone(),
            name: plan.blueprint_name.clone(),
            deployment_id: Some(plan.id.clone()),
            result: None,
            apps: Vec::new(),
        };
        ensure!(
            pier_protocol::valid_system_username(&plan.blueprint_name),
            pier_protocol::BlueprintAccountError::InvalidName
        );
        if let Some(account) = self.store.get::<Account>("blueprint_accounts", &id)? {
            ensure!(
                account.name == plan.blueprint_name,
                pier_protocol::BlueprintAccountError::NameChanged(plan.blueprint_name.clone())
            );
        }
        // Stopping never creates an account and must work without a catalog.
        if downloaded.is_empty() {
            return Ok(snapshot);
        }
        let root = self.config.state_dir.join("blueprints").join(&id);
        let data = root.join("data");
        let saved = self.store.get::<Account>("blueprint_accounts", &id)?;
        let checked = account::check(
            &self.config.agent_id,
            &id,
            &plan.blueprint_name,
            &data,
            saved.as_ref(),
        )?;
        let registered = self.store.list::<Account>("blueprint_accounts")?;
        ensure!(
            !registered.iter().any(|a| a.name == plan.blueprint_name
                && a.marker != account::marker(&self.config.agent_id, &id)),
            pier_protocol::BlueprintAccountError::ReservedName(plan.blueprint_name.clone())
        );
        let account = account::create(
            &self.config.agent_id,
            &id,
            &plan.blueprint_name,
            &data,
            checked.as_ref(),
        )?;
        self.store.put("blueprint_accounts", &id, &account)?;
        account::directory(&root, 0, account.gid, 0o750)?;
        account::directory(&data, account.uid, account.gid, 0o750)?;
        let apps = root.join("apps");
        account::directory(&apps, 0, account.gid, 0o750)?;
        for (app, extracted, manifest) in downloaded {
            self.stopped()?;
            let app_root = apps.join(&app.id);
            let logs = app_root.join("logs");
            let releases = app_root.join("releases");
            for path in [&app_root, &logs, &releases] {
                account::directory(path, 0, account.gid, 0o750)?;
            }
            let release = releases.join(format!("{}-{}", plan.id, app.sha256));
            ensure!(!release.exists(), "release destination already exists");
            account::release_permissions(&extracted, account.gid)?;
            fs::rename(extracted, &release)?;
            snapshot.apps.push(Installed {
                instance: app.instance.clone(),
                id: app.id.clone(),
                sha256: app.sha256.clone(),
                release,
                account: account.clone(),
                logs,
                manifest,
            });
        }
        Ok(snapshot)
    }
}

pub async fn run(config: Config) -> Result<()> {
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime_config = config.clone();
    let runtime =
        tokio::task::spawn_blocking(move || Runtime::open(runtime_config, events_tx)).await??;
    let accepted = runtime.transport.bind().await?;
    let upgrade_config = config.clone();
    let upgrade_runtime = runtime.clone();
    // The upgrade worker may acknowledge readiness only after local apps are restored.
    let upgrades = tokio::task::spawn_blocking(move || {
        upgrade::Manager::open(upgrade_config, upgrade_runtime)
    })
    .await??;
    let network = network::connect(config, runtime.clone(), events_rx, upgrades, accepted);
    systemd::notify_ready()?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        result = network => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
    };
    runtime.transport.stop();
    tokio::task::spawn_blocking(move || runtime.shutdown()).await?;
    result
}
