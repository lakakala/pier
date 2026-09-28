//! Stable metadata for authenticated native agent updates.
use anyhow::{Context, Result, ensure};
use pier_pkg::Architecture;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

pub const BUNDLE_DIR: &str = "/usr/share/pier-controller/agent-releases";
pub const MAX_PACKAGE_SIZE: u64 = 256 * 1024 * 1024;
pub const LEASE_SECONDS: u64 = 45 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Deb,
    Rpm,
}
impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Deb => "deb",
            Self::Rpm => "rpm",
        }
    }
}

/// Native packages currently use numeric x.y.z versions and positive revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub version: String,
    pub revision: u64,
}
impl Version {
    pub fn key(&self) -> Result<([u64; 3], u64)> {
        ensure!(
            self.version.len() <= 64 && self.revision > 0,
            "invalid package version"
        );
        let parts = self
            .version
            .split('.')
            .map(|v| {
                ensure!(
                    !v.is_empty()
                        && v.bytes().all(|b| b.is_ascii_digit())
                        && (v == "0" || !v.starts_with('0')),
                    "invalid software version"
                );
                Ok(v.parse::<u64>()?)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((
            parts
                .try_into()
                .map_err(|_| anyhow::anyhow!("version must be x.y.z"))?,
            self.revision,
        ))
    }
    pub fn newer_than(&self, other: &Self) -> Result<bool> {
        Ok(self.key()? > other.key()?)
    }
    pub fn native(&self, format: Format) -> String {
        format!(
            "{}-{}{}",
            self.version,
            self.revision,
            if format == Format::Rpm { ".el8" } else { "" }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub package: Version,
    pub format: Format,
    pub architecture: Architecture,
    pub system: String,
    pub sha256: String,
    pub size: u64,
}
impl Release {
    pub fn validate(&self) -> Result<()> {
        self.package.key()?;
        ensure!(
            matches!(
                (self.system.as_str(), self.format),
                ("ubuntu24.04", Format::Deb) | ("almalinux8", Format::Rpm)
            ),
            "unsupported release system"
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid release digest"
        );
        ensure!(
            self.size > 0 && self.size <= MAX_PACKAGE_SIZE,
            "invalid release size"
        );
        Ok(())
    }
    pub fn filename(&self) -> String {
        format!("{}.{}", self.sha256, self.format.extension())
    }
    pub fn verify(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            meta.is_file() && meta.len() == self.size,
            "package size or file type mismatch"
        );
        ensure!(file_hash(path)? == self.sha256, "package checksum mismatch");
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema: u32,
    pub releases: Vec<Release>,
}
impl Bundle {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.releases.len() == 4,
            "four agent packages are required"
        );
        let mut platforms = std::collections::BTreeSet::new();
        for release in &self.releases {
            release.validate()?;
            ensure!(
                release.package == self.releases[0].package,
                "agent package versions differ"
            );
            ensure!(
                platforms.insert(format!("{}:{:?}", release.system, release.architecture)),
                "duplicate agent package platform"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Software {
    pub version: String,
    pub package: Option<Version>,
    pub system: Option<String>,
    pub format: Option<Format>,
    pub supported: bool,
    pub reason: Option<String>,
}
impl Software {
    pub fn validate(&self) -> Result<()> {
        Version {
            version: self.version.clone(),
            revision: 1,
        }
        .key()?;
        if let Some(package) = &self.package {
            package.key()?;
        }
        ensure!(
            self.reason.as_ref().is_none_or(|v| v.len() <= 256),
            "software reason too long"
        );
        ensure!(
            self.system
                .as_ref()
                .is_none_or(|v| matches!(v.as_str(), "ubuntu24.04" | "almalinux8")),
            "unsupported system"
        );
        ensure!(
            !self.supported
                || (self.package.is_some() && self.system.is_some() && self.format.is_some()),
            "incomplete software metadata"
        );
        Ok(())
    }
    pub fn accepts(&self, release: &Release) -> Result<bool> {
        self.validate()?;
        release.validate()?;
        if !self.supported
            || self.system.as_ref() != Some(&release.system)
            || self.format != Some(release.format)
        {
            return Ok(false);
        }
        let current = self.package.as_ref().context("missing installed package")?;
        let running = Version {
            version: self.version.clone(),
            revision: 1,
        }
        .key()?
        .0;
        if running > release.package.key()?.0 {
            return Ok(false);
        }
        // Also activate a package that was installed manually without restarting.
        Ok(release.package.newer_than(current)?
            || (current == &release.package && running < release.package.key()?.0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub release: Option<Release>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Waiting,
    Downloading,
    Installing,
    Restarting,
    Succeeded,
    Failed,
}
impl Phase {
    pub fn active(self) -> bool {
        matches!(self, Self::Installing | Self::Restarting)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub release: Release,
    pub phase: Phase,
    pub grant: Option<String>,
    pub error: Option<String>,
    pub updated_at: u64,
}
impl Status {
    pub fn validate(&self) -> Result<()> {
        self.release.validate()?;
        ensure!(
            self.grant.as_ref().is_none_or(|s| crate::safe_id(s)),
            "invalid upgrade grant"
        );
        ensure!(
            self.error
                .as_ref()
                .is_none_or(|s| s.len() <= 512 && !s.contains(['\0', '\r', '\n'])),
            "invalid upgrade error"
        );
        Ok(())
    }
}

pub fn system(value: &str) -> Option<(&'static str, Format)> {
    let field = |name: &str| {
        value.lines().find_map(|line| {
            line.strip_prefix(name)
                .map(|v| v.trim().trim_matches(['\'', '"']))
        })
    };
    match (field("ID="), field("VERSION_ID=")) {
        (Some("ubuntu"), Some("24.04")) => Some(("ubuntu24.04", Format::Deb)),
        (Some("almalinux"), Some(v)) if v == "8" || v.starts_with("8.") => {
            Some(("almalinux8", Format::Rpm))
        }
        _ => None,
    }
}
pub fn file_hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn version(v: &str, revision: u64) -> Version {
        Version {
            version: v.into(),
            revision,
        }
    }
    #[test]
    fn versions_compare_numerically_and_never_downgrade() {
        assert!(
            version("1.10.0", 1)
                .newer_than(&version("1.9.9", 99))
                .unwrap()
        );
        assert!(
            version("1.2.3", 12)
                .newer_than(&version("1.2.3", 2))
                .unwrap()
        );
        assert!(
            !version("1.2.3", 2)
                .newer_than(&version("1.2.3", 2))
                .unwrap()
        );
        for v in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-beta",
            "1:1.2.3",
            "-1.2.3",
        ] {
            assert!(version(v, 1).key().is_err());
        }
        assert!(version("1.2.3", 0).key().is_err());
    }
    #[test]
    fn platforms_are_exact_and_not_based_on_id_like() {
        assert_eq!(
            system("ID=ubuntu\nVERSION_ID=\"24.04\""),
            Some(("ubuntu24.04", Format::Deb))
        );
        assert_eq!(
            system("ID=almalinux\nVERSION_ID=\"8.10\""),
            Some(("almalinux8", Format::Rpm))
        );
        for s in [
            "ID=ubuntu\nVERSION_ID=22.04",
            "ID=almalinux\nVERSION_ID=9.0",
            "ID=rocky\nID_LIKE=almalinux\nVERSION_ID=8.10",
        ] {
            assert!(system(s).is_none());
        }
    }
    #[test]
    fn release_validation_rejects_corruption_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package");
        fs::write(&file, "package").unwrap();
        let release = Release {
            package: version("1.2.3", 1),
            system: "ubuntu24.04".into(),
            format: Format::Deb,
            architecture: Architecture::Amd64,
            size: 7,
            sha256: file_hash(&file).unwrap(),
        };
        release.verify(&file).unwrap();
        fs::write(&file, "corrupt").unwrap();
        assert!(release.verify(&file).is_err());
        fs::write(&file, "package!").unwrap();
        assert!(release.verify(&file).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(release.verify(&link).is_err());
    }
    #[test]
    fn offers_require_matching_platform_and_newer_package() {
        let mut software = Software {
            version: "1.2.3".into(),
            package: Some(version("1.2.3", 1)),
            system: Some("ubuntu24.04".into()),
            format: Some(Format::Deb),
            supported: true,
            reason: None,
        };
        let mut release = Release {
            package: version("1.2.3", 2),
            system: "ubuntu24.04".into(),
            format: Format::Deb,
            architecture: Architecture::Amd64,
            size: 7,
            sha256: "a".repeat(64),
        };
        assert!(software.accepts(&release).unwrap());
        software.version = "2.0.0".into();
        assert!(!software.accepts(&release).unwrap());
        software.version = "1.2.3".into();
        software.package = Some(release.package.clone());
        assert!(!software.accepts(&release).unwrap());
        software.version = "1.2.2".into();
        assert!(software.accepts(&release).unwrap());
        software.package = Some(version("2.0.0", 1));
        assert!(!software.accepts(&release).unwrap());
        release.system = "almalinux8".into();
        release.format = Format::Rpm;
        assert!(!software.accepts(&release).unwrap());
    }
    #[test]
    fn legacy_welcome_omits_new_fields() {
        let message = crate::Message::Welcome {
            version: crate::VERSION,
            upgrade: None,
        };
        let value = serde_json::to_value(message).unwrap();
        assert!(value.get("upgrade").is_none());
        assert!(matches!(
            serde_json::from_value::<crate::Message>(value).unwrap(),
            crate::Message::Welcome { upgrade: None, .. }
        ));
    }
}
