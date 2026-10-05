//! Controller service and HTTP API.
mod api;
mod auth;
#[cfg(test)]
mod build_tests;
pub mod catalog;
mod connection;
mod dialer;
mod enrollment;
mod proxy;
mod runtime;
mod settings;
mod terminal;
mod upgrades;
mod web;
pub use runtime::RuntimeSettings;

use anyhow::{Context, Result, ensure};
use catalog::Catalog;
use pier_pkg::{PackOptions, ProxyOptions, SourceKind};
use pier_protocol::{
    AgentInfo, AgentReport, DeploymentApp, DeploymentPlan, Message, Variables, store::Store,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::mpsc;

fn default_state_dir() -> PathBuf {
    "/var/lib/pier-controller".into()
}
fn default_http() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}
fn default_tcp() -> SocketAddr {
    "0.0.0.0:7443".parse().unwrap()
}
fn default_ref() -> String {
    "main".into()
}
fn default_sync() -> u64 {
    60
}
fn default_builds() -> usize {
    2
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_ref")]
    pub reference: String,
    #[serde(default = "default_sync")]
    pub sync_interval_seconds: u64,
}
impl Default for RepositoryConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            reference: default_ref(),
            sync_interval_seconds: default_sync(),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildProxy {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_http")]
    pub http_listen: SocketAddr,
    /// Legacy settings below are imported once. Runtime changes use the web API.
    #[serde(default)]
    pub repository: RepositoryConfig,
    #[serde(default = "default_tcp")]
    pub tcp_listen: SocketAddr,
    #[serde(default)]
    pub public_url: String,
    #[serde(default)]
    pub agent_endpoint: String,
    #[serde(default)]
    pub build_proxy: BuildProxy,
    #[serde(default = "default_builds")]
    pub max_concurrent_builds: usize,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proxy: Option<proxy::Proxy>,
    #[serde(default)]
    pub connection: pier_protocol::connection::Connection,
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub token_hash: String,
    pub info: Option<AgentInfo>,
    pub last_seen: Option<u64>,
    pub report: AgentReport,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub blueprint: String,
    #[serde(default, deserialize_with = "pier_protocol::unique_map")]
    pub variables: Variables,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    #[serde(default)]
    pub action: pier_protocol::DeploymentAction,
    pub id: String,
    pub agent_id: String,
    pub blueprint: String,
    pub commit: String,
    pub state: String,
    pub error: Option<String>,
    pub created_at: u64,
    pub plan: Option<DeploymentPlan>,
    pub artifacts: BTreeMap<String, PathBuf>,
}
impl Job {
    fn active(&self) -> bool {
        !matches!(
            self.state.as_str(),
            "succeeded" | "failed" | "rolled_back" | "rollback_failed"
        )
    }
}
struct Session {
    id: String,
    sender: mpsc::Sender<Message>,
    terminal: bool,
    multi_blueprint: bool,
    cancelled: tokio_util::sync::CancellationToken,
}
pub struct Controller {
    dialer: dialer::Registry,
    terminals: terminal::Registry,
    _lock: fs::File,
    config: Config,
    store: Store,
    catalog: RwLock<Option<Catalog>>,
    settings: RwLock<settings::Settings>,
    auth: auth::Auth,
    sessions: Mutex<BTreeMap<String, Session>>,
    sync_lock: Mutex<()>,
    mutation_lock: Mutex<()>,
    runtime: RwLock<Option<Arc<runtime::Runtime>>>,
    listener: Mutex<Option<std::net::TcpListener>>,
    listener_ready: tokio::sync::Notify,
    listener_status: RwLock<runtime::ListenerStatus>,
    upgrades: upgrades::Catalog,
}
impl Controller {
    pub fn open(config: Config) -> Result<Arc<Self>> {
        fs::create_dir_all(&config.state_dir)?;
        let lock = pier_protocol::state_lock(&config.state_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o700))?;
        }
        let store = Store::open(&config.state_dir.join("controller.db"))?;
        let catalog = store.get("catalog", "current")?;
        let settings = settings::Settings::load(&store, &config, catalog.is_some())?;
        let runtime = if store.get::<serde_json::Value>("auth", "admin")?.is_some() {
            Some(Arc::new(runtime::Runtime::new(
                settings.runtime.clone().unwrap(),
            )?))
        } else {
            None
        };
        for mut job in store.list::<Job>("jobs")? {
            if job.state == "building" {
                job.state = "failed".into();
                job.error =
                    Some("controller restarted during packaging; submit a new deployment".into());
                store.put("jobs", &job.id, &job)?;
            }
        }
        Ok(Arc::new(Self {
            dialer: dialer::Registry::default(),
            terminals: terminal::Registry::default(),
            _lock: lock,
            upgrades: upgrades::Catalog::open(&config.state_dir),
            config,
            store,
            catalog: RwLock::new(catalog),
            settings: RwLock::new(settings),
            auth: auth::Auth::default(),
            sessions: Mutex::new(BTreeMap::new()),
            sync_lock: Mutex::new(()),
            mutation_lock: Mutex::new(()),
            runtime: RwLock::new(runtime),
            listener: Mutex::new(None),
            listener_ready: tokio::sync::Notify::new(),
            listener_status: RwLock::new(runtime::ListenerStatus::default()),
        }))
    }
    fn bindings(&self, agent: &str) -> Result<BTreeMap<String, Binding>> {
        Ok(self
            .store
            .get("blueprint_bindings", agent)?
            .unwrap_or_default())
    }
    fn check_blueprint_name(&self, agent: &str, path: &str, name: &str) -> Result<()> {
        use pier_protocol::BlueprintAccountError as Error;
        ensure!(
            pier_protocol::valid_system_username(name),
            Error::InvalidName
        );
        let catalog = self.catalog.read().unwrap();
        if let Some(catalog) = catalog.as_ref() {
            for binding in self.bindings(agent)?.values() {
                ensure!(
                    binding.blueprint == path
                        || catalog
                            .blueprints
                            .get(&binding.blueprint)
                            .is_none_or(|b| b.name != name),
                    Error::DuplicateName(name.into())
                );
            }
        }
        if let Some(record) = self.store.get::<AgentRecord>("agents", agent)? {
            for blueprint in record
                .report
                .blueprints
                .iter()
                .filter(|b| b.account_reserved)
            {
                ensure!(
                    blueprint.blueprint == path || blueprint.name != name,
                    Error::ReservedName(name.into())
                );
                // A successful deployment, including a later stop, reserves the name.
                if blueprint.blueprint == path {
                    ensure!(blueprint.name == name, Error::NameChanged(name.into()));
                }
            }
        }
        Ok(())
    }
    fn build(
        &self,
        id: &str,
        catalog: Catalog,
        binding: Binding,
        images: BTreeMap<String, String>,
        architecture: pier_pkg::Architecture,
        runtime: Arc<runtime::Runtime>,
    ) -> Result<()> {
        let blueprint = catalog
            .blueprints
            .get(&binding.blueprint)
            .context("blueprint missing")?;
        let variables = blueprint.resolve(&binding.variables)?;
        let mut job: Job = self.store.get("jobs", id)?.context("job missing")?;
        let mut plan = DeploymentPlan {
            id: id.into(),
            agent_id: job.agent_id.clone(),
            blueprint: binding.blueprint.clone(),
            blueprint_name: blueprint.name.clone(),
            action: pier_protocol::DeploymentAction::Deploy,
            commit: catalog.commit.clone(),
            architecture,
            apps: Vec::new(),
        };
        // Validate all recipes before starting any download or build.
        let mut options = BTreeMap::new();
        self.check_blueprint_name(&job.agent_id, &binding.blueprint, &blueprint.name)?;
        for app in &blueprint.apps {
            let metadata = &catalog.apps[&app.app];
            let image = images.get(&app.id).cloned();
            ensure!(
                (metadata.source == SourceKind::Git) == image.is_some(),
                "source app requires image; binary app forbids image"
            );
            let proxy = &runtime.settings.build_proxy;
            let config = PackOptions {
                image,
                variables: variables[&app.id].clone(),
                output_dir: self
                    .config
                    .state_dir
                    .join("artifacts")
                    .join(id)
                    .join(&app.id),
                proxy: ProxyOptions {
                    http_proxy: proxy.http_proxy.clone(),
                    https_proxy: proxy.https_proxy.clone(),
                    no_proxy: proxy.no_proxy.clone(),
                },
                ..PackOptions::new(architecture)
            };
            pier_pkg::validate(catalog.root.join(&app.app), &config)?;
            options.insert(app.id.clone(), config);
        }
        for app in &blueprint.apps {
            let artifact = pier_pkg::pack(catalog.root.join(&app.app), &options[&app.id])?;
            let instance = pier_protocol::hash(format!("{}\0{}", binding.blueprint, app.id));
            plan.apps.push(DeploymentApp {
                instance,
                id: app.id.clone(),
                size: fs::metadata(&artifact.path)?.len(),
                sha256: artifact.sha256,
            });
            job.artifacts.insert(app.id.clone(), artifact.path);
        }
        job.plan = Some(plan);
        job.state = "ready".into();
        self.store.put("jobs", id, &job)?;
        Ok(())
    }
    async fn dispatch(&self, job_id: &str) -> Result<()> {
        let job: Job = self.store.get("jobs", job_id)?.context("missing job")?;
        if !job.active() {
            return Ok(());
        }
        let sender = self
            .sessions
            .lock()
            .unwrap()
            .get(&job.agent_id)
            .filter(|s| s.multi_blueprint)
            .map(|s| s.sender.clone());
        if let (Some(sender), Some(plan)) = (sender, job.plan) {
            sender
                .send(Message::Deploy { plan })
                .await
                .context("agent disconnected")?;
        }
        Ok(())
    }
}

pub async fn run(config: Config) -> Result<()> {
    let state = Controller::open(config.clone())?;
    let cleanup_state = state.clone();
    let cleanup = async move {
        loop {
            if cleanup_state.expire_enrollments().is_err() {
                tracing::warn!("enrollment cleanup failed");
            }
            if cleanup_state.expire_web_sessions().is_err() {
                tracing::warn!("web session cleanup failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    };
    let http_listener = tokio::net::TcpListener::bind(config.http_listen).await?;
    state.start_saved_listener();
    let tcp = state.clone().serve_agent_listener();
    let dialer = dialer::run(state.clone());
    let http = axum::serve(http_listener, api::router(state)).into_future();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tracing::info!(http=%config.http_listen, "controller web listening");
    tokio::select! { _ = dialer => Ok(()), _ = tcp => Ok(()), _ = cleanup => Ok(()), result = http => Ok(result?), _ = tokio::signal::ctrl_c() => Ok(()), _ = terminate.recv() => Ok(()) }
}
