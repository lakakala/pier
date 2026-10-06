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
    detect_with(fs::read_to_string("/etc/os-release"), package::installed)
}
fn detect_with(
    os_release: std::io::Result<String>,
    installed: impl FnOnce(Format) -> Result<pier_protocol::upgrade::Version>,
) -> Software {
    let mut software = Software {
        version: env!("CARGO_PKG_VERSION").into(),
        package: None,
        system: None,
        format: None,
        supported: false,
        reason: None,
    };
    let detected = (|| -> Result<()> {
        let os_release = os_release.context("无法读取 /etc/os-release，不能识别系统发行版")?;
        let (system, format) = pier_protocol::upgrade::system(&os_release)
            .context("当前发行版不支持自动升级；仅支持 Ubuntu 24.04、AlmaLinux 8 和 9")?;
        software.system = Some(system.into());
        software.format = Some(format);
        software.package = Some(installed(format)?);
        Ok(())
    })();
    software.supported = detected.is_ok();
    software.reason = detected.err().map(|error| error.to_string());
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
                private_dir(Path::new(ROOT))
                    .context("自动升级目录 /var/lib/pier-agent-upgrade 无法访问或权限不符合要求")?;
                let lock = pier_protocol::state_lock(Path::new(ROOT))
                    .context("自动升级目录已被其他进程占用或无法加锁")?;
                if let Some(mut job) =
                    load().context("自动升级记录无法读取、权限不符合要求或内容损坏")?
                {
                    ensure!(
                        job.config.agent_id == config.agent_id,
                        "自动升级记录属于其他 agent 身份；请检查重新初始化前保留的升级记录"
                    );
                    if job.status.phase.active() {
                        runtime.maintenance.store(true, Ordering::SeqCst);
                        if !package::helper_active()
                            || job.boot_id
                                != boot_id()
                                    .context("无法读取系统 boot_id，不能恢复自动升级记录")?
                        {
                            save(
                                &mut job,
                                Phase::Failed,
                                Some("upgrade interrupted; manual recovery required".into()),
                            )
                            .context("无法保存自动升级中断记录；请检查升级目录权限和磁盘空间")?;
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
                            )
                            .context("无法写入自动升级就绪记录；请检查升级目录权限和磁盘空间")?;
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
                        save(&mut job, Phase::Succeeded, None)
                            .context("无法保存自动升级完成记录；请检查升级目录权限和磁盘空间")?;
                    }
                }
                Ok(lock)
            })();
            match prepared {
                Ok(value) => lock = Some(value),
                Err(error) => {
                    tracing::warn!("upgrade journal unavailable; manual recovery required");
                    software.supported = false;
                    software.reason = Some(error.to_string());
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
            let _ = report(&self.runtime.transport, &job.status).await;
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
        let _ = report(&self.runtime.transport, &job.status).await;
        let path = Path::new(ROOT).join(release.filename());
        let mut retry = 30;
        while release.verify(&path).is_err() {
            if !self.wanted(&release) {
                return Ok(());
            }
            match download(&self.runtime.transport, &release, &path).await {
                Ok(()) => break,
                Err(_) => {
                    save(
                        &mut job,
                        Phase::Downloading,
                        Some("package download interrupted; retrying".into()),
                    )?;
                    let _ = report(&self.runtime.transport, &job.status).await;
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
            let _ = report(&self.runtime.transport, &job.status).await;
            return Ok(());
        }
        save(&mut job, Phase::Waiting, None)?;
        let _ = report(&self.runtime.transport, &job.status).await;
        loop {
            if !self.wanted(&release) {
                return Ok(());
            }
            let granted = exchange(
                &self.runtime.transport,
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
                    let _ = report(&self.runtime.transport, &job.status).await;
                    return Ok(());
                }
                if let Err(error) = save(&mut job, Phase::Installing, None) {
                    self.runtime.maintenance.store(false, Ordering::SeqCst);
                    job.status.phase = Phase::Failed;
                    job.status.error =
                        Some("cannot persist installation; manual recovery required".into());
                    let _ = report(&self.runtime.transport, &job.status).await;
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
                    let _ = report(&self.runtime.transport, &job.status).await;
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

type Stream =
    tokio_util::codec::Framed<secure::SecureStream, tokio_util::codec::LengthDelimitedCodec>;
async fn connect(transport: &crate::transport::Transport) -> Result<Stream> {
    transport.open(secure::Purpose::Upgrade).await
}
async fn exchange(transport: &crate::transport::Transport, message: Message) -> Result<Message> {
    timeout(Duration::from_secs(15), async {
        let mut stream = connect(transport).await?;
        pier_protocol::send(&mut stream, &message).await?;
        pier_protocol::receive(&mut stream).await
    })
    .await?
}
async fn report(transport: &crate::transport::Transport, status: &Status) -> Result<()> {
    ensure!(
        matches!(
            exchange(
                transport,
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
async fn download(
    transport: &crate::transport::Transport,
    release: &Release,
    path: &Path,
) -> Result<()> {
    timeout(Duration::from_secs(300), async {
        let staged = tempfile::NamedTempFile::new_in(ROOT)?;
        let mut file = tokio::fs::File::from_std(staged.reopen()?);
        let mut stream = connect(transport).await?;
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

// The standalone installer must never create an outbound socket for a passive agent.
// The running/new agent reports the durable journal through its control-session broker.
async fn report_saved(config: &Config, status: &Status) -> Result<()> {
    if config.connection_mode == pier_protocol::connection::ConnectionMode::ControllerToAgent {
        return Ok(());
    }
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    report(
        &crate::transport::Transport::new(config.clone(), events),
        status,
    )
    .await
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
    if let Err(error) = &result {
        let reason = if error.is::<InstalledVersionChanged>() {
            error.to_string()
        } else {
            "installation or startup failed; inspect pier-agent-upgrade journal and recover manually".into()
        };
        save(&mut job, Phase::Failed, Some(reason))?;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _ = rt.block_on(report_saved(&job.config, &job.status));
    result
}
#[derive(Debug)]
struct InstalledVersionChanged;
impl std::fmt::Display for InstalledVersionChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("已安装包版本不低于目标版本，本次升级已取消；未安装或重启")
    }
}
impl std::error::Error for InstalledVersionChanged {}

fn install_newer(
    current: &pier_protocol::upgrade::Version,
    target: &pier_protocol::upgrade::Version,
    installer: impl FnOnce() -> Result<()>,
) -> Result<()> {
    ensure!(target.newer_than(current)?, InstalledVersionChanged);
    installer()
}

fn install(job: &mut Transaction) -> Result<()> {
    ensure!(job.boot_id == boot_id()?, "upgrade interrupted by reboot");
    let release = &job.status.release;
    let path = PathBuf::from(ROOT).join(release.filename());
    package::inspect(&path, release)?;
    let current = package::installed(release.format)?;
    install_newer(&current, &release.package, || match release.format {
        Format::Deb => package::bounded(
            Command::new("dpkg")
                .args(["--force-confold", "--install"])
                .arg(&path),
            Duration::from_secs(600),
        ),
        Format::Rpm => package::bounded(
            Command::new("rpm")
                .args(["--upgrade", "--replacepkgs"])
                .arg(&path),
            Duration::from_secs(600),
        ),
    })?;
    ensure!(
        package::installed(release.format)? == release.package,
        "installed package version mismatch"
    );
    save(job, Phase::Restarting, None)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _ = rt.block_on(report_saved(&job.config, &job.status));
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

#[cfg(test)]
mod detection_tests {
    use super::*;
    use pier_protocol::upgrade::Version;

    fn installed(_: Format) -> Result<Version> {
        Ok(Version {
            version: "1.2.3".into(),
            revision: 1,
        })
    }

    #[test]
    fn detection_uses_package_version_without_checking_process_ownership() {
        for (os, reason) in [
            (
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                "无法读取 /etc/os-release",
            ),
            (Ok("ID=debian\nVERSION_ID=12".into()), "当前发行版不支持"),
        ] {
            let software =
                detect_with(os, |_| panic!("unsupported system must not query packages"));
            assert!(!software.supported);
            assert!(software.system.is_none());
            assert!(software.reason.as_ref().unwrap().contains(reason));
            software.validate().unwrap();
        }
        let os = || Ok("ID=ubuntu\nVERSION_ID=24.04".into());
        let software = detect_with(os(), |_| {
            anyhow::bail!("无法读取 pier-agent 原生安装包记录")
        });
        assert!(!software.supported);
        assert_eq!(software.system.as_deref(), Some("ubuntu24.04"));
        assert!(software.package.is_none());
        assert!(software.reason.as_ref().unwrap().contains("无法读取"));
        software.validate().unwrap();

        // This test process is neither /usr/bin/pier-agent nor systemd's MainPID.
        let software = detect_with(os(), installed);
        assert!(software.supported);
        assert!(software.package.is_some());
        assert!(software.reason.is_none());
        software.validate().unwrap();
    }

    #[test]
    fn recheck_cancels_equal_or_newer_installed_versions_before_running_installer() {
        let target = Version {
            version: "1.2.3".into(),
            revision: 2,
        };
        for current in [
            target.clone(),
            Version {
                version: "1.2.3".into(),
                revision: 3,
            },
            Version {
                version: "2.0.0".into(),
                revision: 1,
            },
        ] {
            let error = install_newer(&current, &target, || panic!("must not install or restart"))
                .unwrap_err();
            assert!(error.is::<InstalledVersionChanged>());
            assert!(error.to_string().contains("本次升级已取消"));
        }
        let current = Version {
            version: "1.2.3".into(),
            revision: 1,
        };
        let mut called = false;
        install_newer(&current, &target, || {
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(called);
        let error =
            install_newer(&current, &target, || anyhow::bail!("installer failed")).unwrap_err();
        assert!(!error.is::<InstalledVersionChanged>());
    }
}
