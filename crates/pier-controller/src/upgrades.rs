use crate::{AgentRecord, Controller, Job};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use pier_protocol::{
    Message,
    upgrade::{BUNDLE_DIR, Bundle, LEASE_SECONDS, Offer, Phase, Release, Software, Status},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt;

pub(crate) struct Catalog {
    pub releases: Vec<Release>,
    pub directory: PathBuf,
    pub error: Option<String>,
}
impl Catalog {
    pub fn open(state: &Path) -> Self {
        match Self::load(Path::new(BUNDLE_DIR), &state.join("agent-releases")) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!("bundled agent packages unavailable; automatic upgrades disabled");
                Self { releases: vec![], directory: state.join("agent-releases"), error: Some("bundled agent packages unavailable; install a complete controller package and restart".into()) }
            }
        }
    }
    pub(crate) fn load(source: &Path, cache: &Path) -> Result<Self> {
        let manifest = source.join("manifest.json");
        let metadata = fs::symlink_metadata(&manifest)?;
        ensure!(
            metadata.is_file() && metadata.len() <= 65536,
            "invalid agent manifest file"
        );
        use std::io::Read;
        let mut bytes = Vec::new();
        fs::File::open(manifest)?
            .take(65537)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "agent manifest too large");
        let bundle: Bundle = serde_json::from_slice(&bytes)?;
        bundle.validate()?;
        fs::create_dir_all(cache)?;
        for release in &bundle.releases {
            let src = source.join(release.filename());
            release.verify(&src)?;
            let dest = cache.join(release.filename());
            if release.verify(&dest).is_err() {
                let staged = tempfile::NamedTempFile::new_in(cache)?;
                fs::copy(src, staged.path())?;
                release.verify(staged.path())?;
                staged.as_file().sync_all()?;
                staged.persist(&dest)?;
            }
        }
        Ok(Self {
            releases: bundle.releases,
            directory: cache.into(),
            error: None,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    status: Status,
    expires_at: u64,
}
impl Controller {
    pub(crate) fn record_software(&self, id: &str, software: Option<&Software>) -> Result<()> {
        let _lock = self.mutation_lock.lock().unwrap();
        if let Some(software) = software {
            software.validate()?;
            self.store.put("agent_software", id, software)?;
        } else {
            self.store.delete("agent_software", id)?;
        }
        Ok(())
    }
    fn candidate(&self, id: &str) -> Result<Option<Release>> {
        let Some(software) = self.store.get::<Software>("agent_software", id)? else {
            return Ok(None);
        };
        let record: AgentRecord = self.store.get("agents", id)?.context("unknown agent")?;
        Ok(self
            .upgrades
            .releases
            .iter()
            .find(|r| {
                software.system.as_ref() == Some(&r.system)
                    && software.format == Some(r.format)
                    && record
                        .info
                        .as_ref()
                        .is_some_and(|info| info.architecture == r.architecture)
            })
            .cloned())
    }
    pub(crate) fn upgrade_offer(&self, id: &str) -> Result<Offer> {
        let Some(software) = self.store.get::<Software>("agent_software", id)? else {
            return Ok(Offer {
                release: None,
                reason: Some("agent requires a one-time manual upgrade".into()),
            });
        };
        if !software.supported {
            return Ok(Offer {
                release: None,
                reason: software.reason,
            });
        }
        if let Some(error) = &self.upgrades.error {
            return Ok(Offer {
                release: None,
                reason: Some(error.clone()),
            });
        }
        let Some(release) = self.candidate(id)? else {
            return Ok(Offer {
                release: None,
                reason: Some("no matching agent package".into()),
            });
        };
        if !software.accepts(&release)? {
            return Ok(Offer {
                release: None,
                reason: None,
            });
        }
        if let Some(record) = self.store.get::<Record>("agent_upgrades", id)? {
            if record.status.phase == Phase::Failed
                && record.status.release.package == release.package
            {
                return Ok(Offer {
                    release: None,
                    reason: Some("upgrade failed; manual recovery required".into()),
                });
            }
        }
        Ok(Offer {
            release: Some(release),
            reason: None,
        })
    }
    pub(crate) fn upgrade_view(&self, id: &str) -> Result<Value> {
        let _lock = self.mutation_lock.lock().unwrap();
        self.upgrade_busy(id)?;
        let software: Option<Software> = self.store.get("agent_software", id)?;
        let record: Option<Record> = self.store.get("agent_upgrades", id)?;
        let status = record.map(|r| {
            let mut value = json!(r.status);
            value.as_object_mut().unwrap().remove("grant");
            value
        });
        Ok(
            json!({"software":software, "upgrade":{"target":self.candidate(id)?, "status":status, "reason":self.upgrade_offer(id)?.reason}}),
        )
    }
    /// Call under mutation_lock, also used by deployment reservation.
    pub(crate) fn upgrade_busy(&self, id: &str) -> Result<bool> {
        let Some(mut record) = self.store.get::<Record>("agent_upgrades", id)? else {
            return Ok(false);
        };
        if record.status.phase.active() && record.expires_at <= pier_protocol::now() {
            record.status.phase = Phase::Failed;
            record.status.error = Some(
                "upgrade timed out; inspect the agent upgrade journal and recover manually".into(),
            );
            record.status.updated_at = pier_protocol::now();
            self.store.put("agent_upgrades", id, &record)?;
        }
        Ok(record.status.phase.active())
    }
    fn reserve_upgrade(&self, id: &str, sha256: &str) -> Result<Option<String>> {
        let _lock = self.mutation_lock.lock().unwrap();
        if self.upgrade_busy(id)? {
            let record: Record = self.store.get("agent_upgrades", id)?.unwrap();
            return Ok((record.status.release.sha256 == sha256)
                .then_some(record.status.grant)
                .flatten());
        }
        let release = self
            .upgrade_offer(id)?
            .release
            .context("no update available")?;
        ensure!(release.sha256 == sha256, "release changed");
        if self
            .store
            .list::<Job>("jobs")?
            .iter()
            .any(|j| j.agent_id == id && j.active())
        {
            return Ok(None);
        }
        let grant = pier_protocol::new_id();
        let record = Record {
            status: Status {
                release,
                phase: Phase::Installing,
                grant: Some(grant.clone()),
                error: None,
                updated_at: pier_protocol::now(),
            },
            expires_at: pier_protocol::now() + LEASE_SECONDS,
        };
        self.store.put("agent_upgrades", id, &record)?;
        Ok(Some(grant))
    }
    fn upgrade_status(&self, id: &str, mut status: Status) -> Result<()> {
        status.validate()?;
        let _lock = self.mutation_lock.lock().unwrap();
        let previous: Option<Record> = self.store.get("agent_upgrades", id)?;
        let software: Option<Software> = self.store.get("agent_software", id)?;
        let repaired = status.phase == Phase::Succeeded
            && software.is_some_and(|s| {
                s.supported
                    && s.package.as_ref().is_some_and(|p| {
                        p.version == s.version && p.key().ok() >= status.release.package.key().ok()
                    })
            });
        if let Some(grant) = &status.grant {
            let record = previous.as_ref().context("no upgrade reservation")?;
            ensure!(
                record.status.grant.as_ref() == Some(grant)
                    && record.status.release == status.release,
                "upgrade reservation mismatch"
            );
            // A new process can acknowledge manual repair, but delayed progress
            // from the old process must never overwrite a terminal state.
            if record.status.phase == Phase::Succeeded {
                return Ok(());
            }
            if (record.status.phase == Phase::Failed && !repaired)
                || (record.status.phase == Phase::Restarting && status.phase == Phase::Installing)
            {
                return Ok(());
            }
            ensure!(
                !matches!(status.phase, Phase::Waiting | Phase::Downloading),
                "invalid reserved upgrade progress"
            );
        } else {
            if previous.as_ref().is_some_and(|r| r.status.phase.active()) {
                return Ok(());
            }
            let recovery = repaired
                && previous.as_ref().is_some_and(|r| {
                    r.status.phase == Phase::Failed
                        && r.status.release == status.release
                        && r.status.grant.is_none()
                });
            if !recovery {
                ensure!(
                    !status.phase.active() && status.phase != Phase::Succeeded,
                    "upgrade reservation required"
                );
                ensure!(
                    self.upgrade_offer(id)?.release.as_ref() == Some(&status.release),
                    "unexpected upgrade status"
                );
            }
        }
        status.updated_at = pier_protocol::now();
        self.store.put(
            "agent_upgrades",
            id,
            &Record {
                status,
                expires_at: previous.map_or(0, |r| r.expires_at),
            },
        )
    }
}

pub(crate) async fn serve(
    state: &Controller,
    id: &str,
    stream: &mut tokio_util::codec::Framed<
        pier_protocol::secure::SecureStream,
        tokio_util::codec::LengthDelimitedCodec,
    >,
) -> Result<()> {
    match pier_protocol::receive(stream).await? {
        Message::UpgradeReserve { sha256 } => {
            let grant = state.reserve_upgrade(id, &sha256)?;
            pier_protocol::send(stream, &Message::UpgradeGrant { grant }).await?;
        }
        Message::UpgradeStatus { status } => {
            state.upgrade_status(id, status)?;
            pier_protocol::send(stream, &Message::Acked).await?;
        }
        Message::UpgradeDownload { sha256 } => {
            let release = state
                .upgrade_offer(id)?
                .release
                .context("no update available")?;
            ensure!(release.sha256 == sha256, "release mismatch");
            let mut file =
                tokio::fs::File::open(state.upgrades.directory.join(release.filename())).await?;
            ensure!(
                file.metadata().await?.len() == release.size,
                "cached package changed"
            );
            pier_protocol::send(stream, &Message::ArtifactBegin { size: release.size }).await?;
            let mut buffer = [0; 32768];
            let mut sent = 0;
            loop {
                let n = file.read(&mut buffer).await?;
                if n == 0 {
                    break;
                }
                sent += n as u64;
                ensure!(sent <= release.size, "cached package grew");
                pier_protocol::send(
                    stream,
                    &Message::ArtifactChunk {
                        data: STANDARD.encode(&buffer[..n]),
                    },
                )
                .await?;
            }
            ensure!(sent == release.size, "cached package truncated");
            pier_protocol::send(stream, &Message::ArtifactEnd { size: sent }).await?;
        }
        _ => anyhow::bail!("upgrade request required"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pier_pkg::Architecture;
    use pier_protocol::{
        AgentInfo,
        upgrade::{Format, Version},
    };
    fn release(format: Format, architecture: Architecture) -> Release {
        Release {
            package: Version {
                version: "1.2.3".into(),
                revision: 2,
            },
            format,
            architecture,
            system: if format == Format::Deb {
                "ubuntu24.04"
            } else {
                "almalinux8"
            }
            .into(),
            sha256: pier_protocol::hash("package"),
            size: 7,
        }
    }
    fn fixture(path: &Path) -> std::sync::Arc<Controller> {
        let config = serde_json::from_value(json!({"state_dir":path})).unwrap();
        let mut state = Controller::open(config).unwrap();
        std::sync::Arc::get_mut(&mut state).unwrap().upgrades = Catalog {
            releases: vec![release(Format::Deb, Architecture::Amd64)],
            directory: path.into(),
            error: None,
        };
        state
            .store
            .put(
                "agents",
                "agent",
                &AgentRecord {
                    proxy: None,
                    connection: Default::default(),
                    id: "agent".into(),
                    name: "test".into(),
                    token_hash: "test".into(),
                    info: Some(AgentInfo {
                        architecture: Architecture::Amd64,
                        hostname: "test".into(),
                        os_release: String::new(),
                    }),
                    last_seen: None,
                    report: Default::default(),
                },
            )
            .unwrap();
        state
            .record_software(
                "agent",
                Some(&Software {
                    version: "1.2.3".into(),
                    package: Some(Version {
                        version: "1.2.3".into(),
                        revision: 1,
                    }),
                    system: Some("ubuntu24.04".into()),
                    format: Some(Format::Deb),
                    supported: true,
                    reason: None,
                }),
            )
            .unwrap();
        state
    }
    #[test]
    fn bundles_are_complete_and_cached_independently_of_installed_files() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut bundle = Bundle {
            schema: 1,
            releases: vec![],
        };
        for (system, format) in [
            ("ubuntu24.04", Format::Deb),
            ("almalinux8", Format::Rpm),
            ("almalinux9", Format::Rpm),
        ] {
            for arch in [Architecture::Amd64, Architecture::Arm64] {
                let mut release = release(format, arch);
                release.system = system.into();
                fs::write(source.path().join(release.filename()), "package").unwrap();
                bundle.releases.push(release);
            }
        }
        fs::write(
            source.path().join("manifest.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();
        let catalog = Catalog::load(source.path(), cache.path()).unwrap();
        fs::write(source.path().join(bundle.releases[0].filename()), "corrupt").unwrap();
        catalog.releases[0]
            .verify(&cache.path().join(catalog.releases[0].filename()))
            .unwrap();
        assert!(Catalog::load(source.path(), cache.path()).is_err());
        bundle.releases[1] = bundle.releases[0].clone();
        assert!(bundle.validate().is_err());
        bundle.releases.pop();
        assert!(bundle.validate().is_err());
    }
    #[test]
    fn candidates_match_system_and_architecture() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = fixture(dir.path());
        let mut el9_arm = release(Format::Rpm, Architecture::Arm64);
        el9_arm.system = "almalinux9".into();
        let mut el9_amd = el9_arm.clone();
        el9_amd.architecture = Architecture::Amd64;
        std::sync::Arc::get_mut(&mut state)
            .unwrap()
            .upgrades
            .releases = vec![
            release(Format::Rpm, Architecture::Amd64),
            el9_arm,
            el9_amd.clone(),
        ];
        let mut software: Software = state.store.get("agent_software", "agent").unwrap().unwrap();
        assert!(state.candidate("agent").unwrap().is_none());
        software.system = Some("almalinux9".into());
        software.format = Some(Format::Rpm);
        state.record_software("agent", Some(&software)).unwrap();
        assert_eq!(state.upgrade_offer("agent").unwrap().release, Some(el9_amd));
    }
    #[test]
    fn reservation_waits_for_deployments_and_persists_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let state = fixture(dir.path());
        let mut job = Job {
            id: "job".into(),
            agent_id: "agent".into(),
            blueprint: "test".into(),
            commit: "commit".into(),
            state: "applying".into(),
            error: None,
            created_at: 0,
            plan: None,
            artifacts: Default::default(),
        };
        state.store.put("jobs", "job", &job).unwrap();
        let sha = pier_protocol::hash("package");
        assert!(state.reserve_upgrade("agent", &sha).unwrap().is_none());
        job.state = "succeeded".into();
        state.store.put("jobs", "job", &job).unwrap();
        let grant = state.reserve_upgrade("agent", &sha).unwrap().unwrap();
        assert!(state.upgrade_busy("agent").unwrap());
        assert_eq!(
            state.reserve_upgrade("agent", &sha).unwrap().unwrap(),
            grant
        );
        drop(state);
        let state = fixture(dir.path());
        assert!(state.upgrade_busy("agent").unwrap());
        assert_eq!(
            state.reserve_upgrade("agent", &sha).unwrap().unwrap(),
            grant
        );
        let mut record: Record = state.store.get("agent_upgrades", "agent").unwrap().unwrap();
        record.expires_at = 0;
        state.store.put("agent_upgrades", "agent", &record).unwrap();
        assert!(!state.upgrade_busy("agent").unwrap());
        assert!(state.upgrade_offer("agent").unwrap().release.is_none());
    }
    #[test]
    fn package_metadata_alone_does_not_acknowledge_restart_and_failed_target_stays_paused() {
        let dir = tempfile::tempdir().unwrap();
        let state = fixture(dir.path());
        state
            .reserve_upgrade("agent", &pier_protocol::hash("package"))
            .unwrap();
        let mut software: Software = state.store.get("agent_software", "agent").unwrap().unwrap();
        software.package.as_mut().unwrap().revision = 2;
        state.record_software("agent", Some(&software)).unwrap();
        assert!(state.upgrade_busy("agent").unwrap());
        let record: Record = state.store.get("agent_upgrades", "agent").unwrap().unwrap();
        let mut status = record.status;
        let mut incorrect = status.clone();
        incorrect.grant = Some("wrong".into());
        assert!(state.upgrade_status("agent", incorrect).is_err());
        status.phase = Phase::Failed;
        state.upgrade_status("agent", status.clone()).unwrap();
        status.phase = Phase::Restarting;
        state.upgrade_status("agent", status.clone()).unwrap();
        assert!(!state.upgrade_busy("agent").unwrap());
        status.phase = Phase::Succeeded;
        state.upgrade_status("agent", status).unwrap();
        let record: Record = state.store.get("agent_upgrades", "agent").unwrap().unwrap();
        assert_eq!(record.status.phase, Phase::Succeeded);
    }
}
