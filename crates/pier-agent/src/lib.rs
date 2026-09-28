//! Linux deployment agent and process supervisor.
#[cfg(not(target_os = "linux"))]
compile_error!("pier-agent currently requires Linux");

mod account;
pub mod init;
mod network;
mod supervisor;
mod systemd;
pub mod upgrade;

use account::Account;
use anyhow::{Result, ensure};
use pier_pkg::{Architecture, PackageManifest};
use pier_protocol::{AgentReport, DeploymentPlan, DeploymentResult, Message, store::Store};
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
    pub controller_tcp: String,
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
    deployment_id: Option<String>,
    apps: Vec<Installed>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    id: String,
    fingerprint: String,
    before: Snapshot,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DurableState {
    snapshot: Snapshot,
    pending: Option<Pending>,
    result: Option<DeploymentResult>,
}
#[derive(Serialize, Deserialize)]
struct Completed {
    fingerprint: String,
    result: DeploymentResult,
}

pub struct Runtime {
    _lock: fs::File,
    config: Config,
    store: Store,
    active: Mutex<BTreeMap<String, Supervisor>>,
    operation: Mutex<()>,
    shutting_down: AtomicBool,
    maintenance: AtomicBool,
    events: tokio::sync::mpsc::UnboundedSender<Message>,
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
        pier_protocol::enrollment::endpoint(&config.controller_tcp)?;
        ensure!(
            fs::read_to_string(&config.token_file)?.trim().len() >= 32,
            "agent token must contain at least 32 characters"
        );
        architecture()?;
        fs::create_dir_all(&config.state_dir)?;
        let lock = pier_protocol::state_lock(&config.state_dir)?;
        // App users need traversal to their own protected app directory.
        fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o711))?;
        account::directory(&config.state_dir.join("apps"), 0, 0, 0o711)?;
        account::directory(&config.state_dir.join("downloads"), 0, 0, 0o700)?;
        let store = Store::open(&config.state_dir.join("agent.db"))?;
        let identity: Option<String> = store.get("identity", "agent_id")?;
        ensure!(
            identity.as_ref().is_none_or(|id| id == &config.agent_id),
            "state directory belongs to another agent identity"
        );
        store.put("identity", "agent_id", &config.agent_id)?;
        let state = Arc::new(Self {
            _lock: lock,
            config,
            store,
            active: Mutex::new(BTreeMap::new()),
            operation: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            maintenance: AtomicBool::new(false),
            events,
        });
        for account in state.store.list::<Account>("accounts")? {
            account::cleanup(&account)?;
        }
        let mut durable: DurableState = state.store.get("runtime", "state")?.unwrap_or_default();
        let interrupted = durable.pending.take();
        if let Some(pending) = &interrupted {
            durable.snapshot = pending.before.clone();
        }
        // Keep the pending journal until the prior snapshot is restored.
        let restored = state.restore(&durable.snapshot, interrupted.is_some());
        if let Some(pending) = interrupted {
            let result = DeploymentResult {
                id: pending.id.clone(),
                state: if restored.is_ok() {
                    "rolled_back"
                } else {
                    "rollback_failed"
                }
                .into(),
                error: Some("agent restarted during deployment; restored previous snapshot".into()),
            };
            durable.result = Some(result.clone());
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
    fn stop_instance(&self, instance: &str) {
        let supervisor = self.active.lock().unwrap().remove(instance);
        if let Some(mut supervisor) = supervisor {
            supervisor.stop();
        }
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
    fn start(&self, installed: Installed, observe: bool) -> Result<()> {
        self.stopped()?;
        let instance = installed.instance.clone();
        let supervisor = Supervisor::start(installed, self.config.runtime.clone(), observe);
        self.active
            .lock()
            .unwrap()
            .insert(instance.clone(), supervisor);
        if observe {
            // Only operation worker writes the map, but report readers remain unblocked
            // during startup observation via a separate status/ready handle.
            let deadline = std::time::Instant::now()
                + Duration::from_secs(self.config.runtime.startup_grace_seconds + 10);
            loop {
                self.stopped()?;
                let state = self
                    .active
                    .lock()
                    .unwrap()
                    .get(&instance)
                    .unwrap()
                    .status
                    .lock()
                    .unwrap()
                    .state
                    .clone();
                if state == "running" {
                    return Ok(());
                }
                ensure!(
                    state != "failed" && std::time::Instant::now() < deadline,
                    "app failed startup observation"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    }
    fn restore(&self, snapshot: &Snapshot, observe: bool) -> Result<()> {
        self.stop_all();
        let mut failed = false;
        for installed in &snapshot.apps {
            if self.start(installed.clone(), observe).is_err() {
                failed = true;
                self.stop_instance(&installed.instance);
                // Failed rollback services continue retrying with backoff.
                if !self.shutting_down.load(Ordering::SeqCst) {
                    let _ = self.start(installed.clone(), false);
                }
            }
        }
        ensure!(!failed, "one or more previous services failed to restart");
        Ok(())
    }
    pub fn report(&self) -> Result<AgentReport> {
        let durable: DurableState = self.store.get("runtime", "state")?.unwrap_or_default();
        let apps = self
            .active
            .lock()
            .unwrap()
            .values()
            .map(|s| s.status.lock().unwrap().clone())
            .collect();
        Ok(AgentReport {
            deployment_id: durable.snapshot.deployment_id,
            apps,
            result: durable.result,
        })
    }
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Stop running apps even if a deployment is waiting on a network read.
        self.stop_all();
        let _guard = self.operation.lock().unwrap();
        self.stop_all();
        if let Ok(accounts) = self.store.list::<Account>("accounts") {
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
        self.maintenance.store(true, Ordering::SeqCst);
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
        let _ = self.events.send(Message::Progress {
            id: plan.id.clone(),
            phase: "downloading".into(),
        });
        let result = match self.prepare(&plan) {
            Err(_) => DeploymentResult {
                id: plan.id.clone(),
                state: "failed".into(),
                error: Some("artifact preparation failed; existing deployment retained".into()),
            },
            Ok(candidate) => {
                self.stopped()?;
                durable.pending = Some(Pending {
                    id: plan.id.clone(),
                    fingerprint: fingerprint.clone(),
                    before: durable.snapshot.clone(),
                });
                self.store.put("runtime", "state", &durable)?;
                let _ = self.events.send(Message::Progress {
                    id: plan.id.clone(),
                    phase: "applying".into(),
                });
                let applied = self.activate(&candidate, &durable.snapshot);
                if applied.is_ok() {
                    durable.snapshot = candidate;
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
                    let restored = self.restore(&durable.snapshot, true);
                    DeploymentResult {
                        id: plan.id.clone(),
                        state: if restored.is_ok() {
                            "rolled_back"
                        } else {
                            "rollback_failed"
                        }
                        .into(),
                        error: Some(
                            "app startup failed; restoring the entire previous deployment".into(),
                        ),
                    }
                }
            }
        };
        let pending = durable.pending.take();
        durable.result = Some(result.clone());
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
                let _ = self.restore(&pending.before, false);
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
            network::download(&self.config, &plan.id, app, &package, &self.shutting_down)?;
            let extracted = workspace.path().join(&app.id);
            let manifest = pier_pkg::unpack(&package, &extracted, &app.sha256, plan.architecture)?;
            ensure!(
                !manifest.service.env.contains_key("PIER_DATA_DIR")
                    && !manifest.service.env.contains_key("PIER_LOG_DIR"),
                "service overrides reserved agent environment variable"
            );
            downloaded.push((app, extracted, manifest));
        }
        let mut snapshot = Snapshot {
            deployment_id: Some(plan.id.clone()),
            apps: Vec::new(),
        };
        for (app, extracted, manifest) in downloaded {
            self.stopped()?;
            let root = self.config.state_dir.join("apps").join(&app.instance);
            let data = root.join("data");
            let logs = root.join("logs");
            let account = account::create(&self.config.agent_id, &app.instance, &data)?;
            self.store.put("accounts", &app.instance, &account)?;
            account::directory(&root, 0, account.gid, 0o750)?;
            account::directory(&data, account.uid, account.gid, 0o750)?;
            account::directory(&logs, 0, account.gid, 0o750)?;
            let releases = root.join("releases");
            account::directory(&releases, 0, account.gid, 0o750)?;
            // Include deployment ID so reinstalling an identical artifact never trusts
            // potentially modified contents of an older installation.
            let release = releases.join(format!("{}-{}", plan.id, app.sha256));
            ensure!(!release.exists(), "release destination already exists");
            account::release_permissions(&extracted, account.gid)?;
            fs::rename(extracted, &release)?;
            snapshot.apps.push(Installed {
                instance: app.instance.clone(),
                id: app.id.clone(),
                sha256: app.sha256.clone(),
                release,
                account,
                logs,
                manifest,
            });
        }
        Ok(snapshot)
    }
    fn activate(&self, candidate: &Snapshot, before: &Snapshot) -> Result<()> {
        // Stop removed instances first so replacements may reuse their listening ports.
        for old in &before.apps {
            if !candidate
                .apps
                .iter()
                .any(|new| new.instance == old.instance)
            {
                self.stop_instance(&old.instance);
            }
        }
        for installed in &candidate.apps {
            self.stopped()?;
            self.stop_instance(&installed.instance);
            self.start(installed.clone(), true)?;
        }
        Ok(())
    }
}

pub async fn run(config: Config) -> Result<()> {
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime_config = config.clone();
    let runtime =
        tokio::task::spawn_blocking(move || Runtime::open(runtime_config, events_tx)).await??;
    let upgrade_config = config.clone();
    let upgrade_runtime = runtime.clone();
    // The upgrade worker may acknowledge readiness only after local apps are restored.
    let upgrades = tokio::task::spawn_blocking(move || {
        upgrade::Manager::open(upgrade_config, upgrade_runtime)
    })
    .await??;
    let network = network::connect(config, runtime.clone(), events_rx, upgrades);
    systemd::notify_ready()?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        result = network => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
    };
    tokio::task::spawn_blocking(move || runtime.shutdown()).await?;
    result
}
