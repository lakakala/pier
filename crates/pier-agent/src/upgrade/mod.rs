//! Authenticated native updates, with installation in an independent systemd unit.
mod package;

use crate::{Config, Runtime};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use pier_protocol::{
    Message, secure,
    upgrade::{Format, Offer, Phase, Release, Software, Status},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncWriteExt, time::timeout};

const ROOT: &str = "/var/lib/pier-agent-upgrade";
const JOB: &str = "transaction.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transaction {
    config: Config,
    status: Status,
    boot_id: String,
    previous_pid: u32,
}
#[derive(Serialize, Deserialize)]
struct Ready {
    grant: String,
    pid: u32,
    package: pier_protocol::upgrade::Version,
}

fn boot_id() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .into())
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == 0,
        "upgrade directory must belong to root"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        meta.is_file() && meta.uid() == 0 && meta.mode() & 0o077 == 0 && meta.len() <= 65536,
        "unsafe upgrade journal"
    );
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("journal parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn load() -> Result<Option<Transaction>> {
    let job: Option<Transaction> = read_json(&Path::new(ROOT).join(JOB))?;
    if let Some(job) = &job {
        job.status.validate()?;
    }
    Ok(job)
}
fn save(job: &mut Transaction, phase: Phase, error: Option<String>) -> Result<()> {
    job.status.phase = phase;
    job.status.error = error;
    job.status.updated_at = pier_protocol::now();
    write_json(&Path::new(ROOT).join(JOB), job)
}
fn detect() -> Software {
    let mut software = Software {
        version: env!("CARGO_PKG_VERSION").into(),
        package: None,
        system: None,
        format: None,
        supported: false,
        reason: Some(
            "automatic upgrades require a supported native package managed by pier-agent.service"
                .into(),
        ),
    };
    if let Some((system, format)) = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| pier_protocol::upgrade::system(&s))
    {
        software.system = Some(system.into());
        software.format = Some(format);
        software.package = package::installed(system, format).ok();
        software.supported = software.package.is_some() && package::managed();
        if software.supported {
            software.reason = None;
        }
    }
    software
}

pub(crate) struct Manager {
    config: Config,
    runtime: Arc<Runtime>,
    software: Software,
    _lock: Option<fs::File>,
    target: Mutex<Option<Release>>,
    working: AtomicBool,
}
impl Manager {
    pub fn open(config: Config, runtime: Arc<Runtime>) -> Result<Arc<Self>> {
        let mut software = detect();
        let mut lock = None;
        if software.supported {
            let prepared = (|| -> Result<_> {
                private_dir(Path::new(ROOT))?;
                let lock = pier_protocol::state_lock(Path::new(ROOT))?;
                if let Some(mut job) = load()? {
                    ensure!(
                        job.config.agent_id == config.agent_id,
                        "upgrade journal belongs to another agent; manual recovery required"
                    );
                    if job.status.phase.active() {
                        runtime.maintenance.store(true, Ordering::SeqCst);
                        if !package::helper_active() || job.boot_id != boot_id()? {
                            save(
                                &mut job,
                                Phase::Failed,
                                Some("upgrade interrupted; manual recovery required".into()),
                            )?;
                            runtime.maintenance.store(false, Ordering::SeqCst);
                        } else if job.status.phase == Phase::Restarting
                            && job.previous_pid != std::process::id()
                            && software.package.as_ref() == Some(&job.status.release.package)
                            && software.version == job.status.release.package.version
                        {
                            write_json(
                                &Path::new(ROOT).join("ready.json"),
                                &Ready {
                                    grant: job.status.grant.clone().context("missing grant")?,
                                    pid: std::process::id(),
                                    package: job.status.release.package.clone(),
                                },
                            )?;
                        }
                    } else if job.status.phase == Phase::Failed
                        && job.previous_pid != std::process::id()
                        && software
                            .package
                            .as_ref()
                            .is_some_and(|p| p.key().ok() >= job.status.release.package.key().ok())
                        && software
                            .package
                            .as_ref()
                            .is_some_and(|p| p.version == software.version)
                    {
                        save(&mut job, Phase::Succeeded, None)?;
                    }
                }
                Ok(lock)
            })();
            match prepared {
                Ok(value) => lock = Some(value),
                Err(_) => {
                    tracing::warn!("upgrade journal unavailable; manual recovery required");
                    software.supported = false;
                    software.reason = Some(
                        "upgrade journal unavailable; inspect local permissions and recovery state"
                            .into(),
                    );
                }
            }
        }
        Ok(Arc::new(Self {
            config,
            runtime,
            software,
            _lock: lock,
            target: Mutex::new(None),
            working: AtomicBool::new(false),
        }))
    }
    pub fn software(&self) -> Software {
        self.software.clone()
    }
    pub fn offer(self: &Arc<Self>, offer: Option<Offer>) {
        *self.target.lock().unwrap() = offer.and_then(|v| v.release);
        self.poll();
    }
    pub fn poll(self: &Arc<Self>) {
        if !self.software.supported || self.working.swap(true, Ordering::SeqCst) {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            struct Working(Arc<Manager>);
            impl Drop for Working {
                fn drop(&mut self) {
                    self.0.working.store(false, Ordering::SeqCst);
                }
            }
            let _working = Working(manager.clone());
            if let Err(error) = manager.tick().await {
                tracing::warn!(%error, "agent upgrade check failed; existing services retained");
            }
        });
    }
    async fn tick(&self) -> Result<()> {
        if let Some(mut job) = load()? {
            if job.status.phase.active()
                && job.status.updated_at + 30 < pier_protocol::now()
                && !tokio::task::spawn_blocking(package::helper_active).await?
            {
                save(
                    &mut job,
                    Phase::Failed,
                    Some("upgrade worker stopped; manual recovery required".into()),
                )?;
            }
            let _ = report(&self.config, &job.status).await;
            self.runtime
                .maintenance
                .store(job.status.phase.active(), Ordering::SeqCst);
            if job.status.phase.active() {
                return Ok(());
            }
        }
        let Some(release) = self.target.lock().unwrap().clone() else {
            return Ok(());
        };
        ensure!(
            release.architecture == crate::architecture()? && self.software.accepts(&release)?,
            "invalid update offer"
        );
        if load()?.is_some_and(|job| {
            job.status.phase == Phase::Failed && job.status.release.package == release.package
        }) {
            return Ok(());
        }
        self.prepare(release).await
    }
    fn wanted(&self, release: &Release) -> bool {
        !self.runtime.shutting_down.load(Ordering::SeqCst)
            && self.target.lock().unwrap().as_ref() == Some(release)
    }
    async fn prepare(&self, release: Release) -> Result<()> {
        let mut job = Transaction {
            config: self.config.clone(),
            status: Status {
                release: release.clone(),
                phase: Phase::Downloading,
                grant: None,
                error: None,
                updated_at: pier_protocol::now(),
            },
            boot_id: boot_id()?,
            previous_pid: std::process::id(),
        };
        save(&mut job, Phase::Downloading, None)?;
        let _ = report(&self.config, &job.status).await;
        let path = Path::new(ROOT).join(release.filename());
        let mut retry = 30;
        while release.verify(&path).is_err() {
            if !self.wanted(&release) {
                return Ok(());
            }
            match download(&self.config, &release, &path).await {
                Ok(()) => break,
                Err(_) => {
                    save(
                        &mut job,
                        Phase::Downloading,
                        Some("package download interrupted; retrying".into()),
                    )?;
                    let _ = report(&self.config, &job.status).await;
                    tokio::time::sleep(Duration::from_secs(retry)).await;
                    retry = (retry * 2).min(300);
                }
            }
        }
        let inspected = {
            let path = path.clone();
            let release = release.clone();
            tokio::task::spawn_blocking(move || package::inspect(&path, &release)).await?
        };
        if inspected.is_err() {
            save(
                &mut job,
                Phase::Failed,
                Some("downloaded package validation failed; manual recovery required".into()),
            )?;
            let _ = report(&self.config, &job.status).await;
            return Ok(());
        }
        save(&mut job, Phase::Waiting, None)?;
        let _ = report(&self.config, &job.status).await;
        loop {
            if !self.wanted(&release) {
                return Ok(());
            }
            let granted = exchange(
                &self.config,
                Message::UpgradeReserve {
                    sha256: release.sha256.clone(),
                },
            )
            .await;
            if let Ok(Message::UpgradeGrant { grant: Some(grant) }) = granted {
                job.status.grant = Some(grant);
                if !self.runtime.enter_upgrade()? {
                    save(
                        &mut job,
                        Phase::Failed,
                        Some("local deployment was not idle; manual recovery required".into()),
                    )?;
                    let _ = report(&self.config, &job.status).await;
                    return Ok(());
                }
                if let Err(error) = save(&mut job, Phase::Installing, None) {
                    self.runtime.maintenance.store(false, Ordering::SeqCst);
                    job.status.phase = Phase::Failed;
                    job.status.error =
                        Some("cannot persist installation; manual recovery required".into());
                    let _ = report(&self.config, &job.status).await;
                    return Err(error);
                }
                let started = tokio::task::spawn_blocking(|| {
                    package::systemctl(&["start", "--no-block", "pier-agent-upgrade.service"])
                })
                .await?;
                if started.is_err() {
                    save(
                        &mut job,
                        Phase::Failed,
                        Some("cannot start upgrade worker; manual recovery required".into()),
                    )?;
                    self.runtime.maintenance.store(false, Ordering::SeqCst);
                    let _ = report(&self.config, &job.status).await;
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

type Stream =
    tokio_util::codec::Framed<secure::SecureStream, tokio_util::codec::LengthDelimitedCodec>;
async fn connect(config: &Config) -> Result<Stream> {
    let token = fs::read_to_string(&config.token_file)?;
    Ok(pier_protocol::framed(
        secure::connect(
            &config.controller_tcp,
            secure::Purpose::Upgrade,
            &config.agent_id,
            &secure::token_key(&token),
        )
        .await?,
    ))
}
async fn exchange(config: &Config, message: Message) -> Result<Message> {
    timeout(Duration::from_secs(15), async {
        let mut stream = connect(config).await?;
        pier_protocol::send(&mut stream, &message).await?;
        pier_protocol::receive(&mut stream).await
    })
    .await?
}
async fn report(config: &Config, status: &Status) -> Result<()> {
    ensure!(
        matches!(
            exchange(
                config,
                Message::UpgradeStatus {
                    status: status.clone()
                }
            )
            .await?,
            Message::Acked
        ),
        "upgrade acknowledgement required"
    );
    Ok(())
}
async fn download(config: &Config, release: &Release, path: &Path) -> Result<()> {
    timeout(Duration::from_secs(300), async {
        let staged = tempfile::NamedTempFile::new_in(ROOT)?;
        let mut file = tokio::fs::File::from_std(staged.reopen()?);
        let mut stream = connect(config).await?;
        pier_protocol::send(&mut stream, &Message::UpgradeDownload { sha256: release.sha256.clone() }).await?;
        ensure!(matches!(pier_protocol::receive(&mut stream).await?, Message::ArtifactBegin { size } if size == release.size), "upgrade size mismatch");
        let mut received = 0;
        loop {
            match pier_protocol::receive(&mut stream).await? {
                Message::ArtifactChunk { data } => {
                    ensure!(data.len() <= 43692, "upgrade chunk too large");
                    let data = STANDARD.decode(data)?;
                    ensure!(!data.is_empty() && data.len() <= 32768, "invalid upgrade chunk");
                    received += data.len() as u64;
                    ensure!(received <= release.size, "upgrade size exceeded");
                    file.write_all(&data).await?;
                }
                Message::ArtifactEnd { size } => {
                    ensure!(received == release.size && size == release.size, "upgrade truncated");
                    file.sync_all().await?;
                    drop(file);
                    staged.persist(path)?;
                    return Ok(());
                }
                _ => anyhow::bail!("unexpected update transfer message"),
            }
        }
    }).await?
}

/// Internal entry point, launched only by the independently managed upgrade unit.
#[doc(hidden)]
pub fn apply() -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "upgrade worker requires root"
    );
    ensure!(
        package::systemctl(&[
            "show",
            "--property=MainPID",
            "--value",
            "pier-agent-upgrade.service"
        ])? == std::process::id().to_string(),
        "upgrade worker must run in its systemd unit"
    );
    let lock_dir = Path::new(ROOT).join("installer");
    private_dir(&lock_dir)?;
    let _lock = pier_protocol::state_lock(&lock_dir)?;
    let mut job = load()?.context("no pending upgrade")?;
    job.status.validate()?;
    ensure!(
        job.status.phase == Phase::Installing && job.status.grant.is_some(),
        "upgrade not reserved"
    );
    let result = install(&mut job);
    if result.is_err() {
        save(&mut job, Phase::Failed, Some("installation or startup failed; inspect pier-agent-upgrade journal and recover manually".into()))?;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _ = rt.block_on(report(&job.config, &job.status));
    result
}
fn install(job: &mut Transaction) -> Result<()> {
    ensure!(job.boot_id == boot_id()?, "upgrade interrupted by reboot");
    let release = &job.status.release;
    let path = PathBuf::from(ROOT).join(release.filename());
    package::inspect(&path, release)?;
    let current = package::installed(&release.system, release.format)?;
    ensure!(!current.newer_than(&release.package)?, "refusing downgrade");
    match release.format {
        Format::Deb => package::bounded(
            Command::new("dpkg")
                .args(["--force-confold", "--install"])
                .arg(&path),
            Duration::from_secs(600),
        )?,
        Format::Rpm => package::bounded(
            Command::new("rpm")
                .args(["--upgrade", "--replacepkgs"])
                .arg(&path),
            Duration::from_secs(600),
        )?,
    }
    ensure!(
        package::installed(&release.system, release.format)? == release.package,
        "installed package version mismatch"
    );
    save(job, Phase::Restarting, None)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _ = rt.block_on(report(&job.config, &job.status));
    package::bounded(
        Command::new("systemctl").args(["restart", "pier-agent.service"]),
        Duration::from_secs(1510),
    )?;
    let ready: Ready = read_json(&Path::new(ROOT).join("ready.json"))?
        .context("new agent did not acknowledge readiness")?;
    ensure!(
        job.status.grant.as_ref() == Some(&ready.grant)
            && ready.pid != job.previous_pid
            && ready.package == job.status.release.package,
        "unexpected new agent identity"
    );
    ensure!(
        package::systemctl(&[
            "show",
            "--property=MainPID",
            "--value",
            "pier-agent.service"
        ])? == ready.pid.to_string(),
        "new agent stopped during startup"
    );
    save(job, Phase::Succeeded, None)
}
