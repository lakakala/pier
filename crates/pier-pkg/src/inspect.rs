use crate::{
    Architecture, Error, FileRecord, Result, Service, Stage, archive, config, files,
    types::IoResult,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariableDefinition {
    pub default: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Git,
    Binary,
}

/// Names and versions may still contain template expressions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMetadata {
    pub name: String,
    pub version: String,
    pub variables: BTreeMap<String, VariableDefinition>,
    pub source: SourceKind,
}

/// Read declarations without rendering templates, contacting a source, or writing files.
pub fn inspect(service_dir: impl AsRef<Path>) -> Result<AppMetadata> {
    let recipe = config::read(service_dir.as_ref())?;
    for name in recipe.variables.keys() {
        if !config::valid_env(name) || name == "PIER_ARCH" {
            return Err(Error::new(
                Stage::Variables,
                "invalid or reserved variable name",
            ));
        }
    }
    let source = match recipe.source {
        config::Source::Git { .. } => {
            if recipe.build.is_none() {
                return Err(Error::new(
                    Stage::Configuration,
                    "git source requires build",
                ));
            }
            SourceKind::Git
        }
        config::Source::Binary { .. } => {
            if recipe.build.is_some() {
                return Err(Error::new(
                    Stage::Configuration,
                    "binary source cannot have build",
                ));
            }
            SourceKind::Binary
        }
    };
    Ok(AppMetadata {
        name: recipe.name,
        version: recipe.version,
        source,
        variables: recipe
            .variables
            .into_iter()
            .map(|(k, v)| (k, VariableDefinition { default: v.default }))
            .collect(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub schema: u32,
    pub name: String,
    pub version: String,
    pub os: String,
    pub architecture: Architecture,
    pub image: Option<String>,
    pub service: Service,
    pub source_commit: Option<String>,
    pub files: Vec<FileRecord>,
}

/// Verify and unpack into a NEW directory. Reject links, duplicate entries, extra
/// files and archives expanding beyond 10 GiB. Failure leaves destination absent.
pub fn unpack(
    package: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    sha256: &str,
    architecture: Architecture,
) -> Result<PackageManifest> {
    let package = package.as_ref();
    let destination = destination.as_ref();
    let bad = |message| Error::new(Stage::Archive, message);
    if sha256.len() != 64 || files::hash(package)? != sha256 {
        return Err(bad("package SHA-256 mismatch"));
    }
    if destination.symlink_metadata().is_ok() {
        return Err(bad("unpack destination already exists"));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| bad("destination needs a parent"))?;
    fs::create_dir_all(parent).context(Stage::Archive, parent)?;
    let staging = tempfile::tempdir_in(parent).context(Stage::Archive, parent)?;
    let input = fs::File::open(package).context(Stage::Archive, package)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(input));
    let mut paths = BTreeSet::new();
    let mut size = 0u64;
    for entry in tar.entries().context(Stage::Archive, package)? {
        let mut entry = entry.context(Stage::Archive, package)?;
        let path = files::relative(&entry.path().context(Stage::Archive, package)?, false)?;
        if !paths.insert(path.clone()) || paths.len() > 100_000 {
            return Err(bad("duplicate entry or too many entries"));
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(bad("only regular files and directories are permitted"));
        }
        size = size
            .checked_add(entry.size())
            .ok_or_else(|| bad("archive size overflow"))?;
        if size > 10 * 1024 * 1024 * 1024 {
            return Err(bad("archive exceeds 10 GiB"));
        }
        let target = staging.path().join(path);
        if kind.is_dir() {
            fs::create_dir_all(&target).context(Stage::Archive, &target)?;
            files::set_mode(&target, 0o755)?;
        } else {
            fs::create_dir_all(target.parent().expect("entry parent"))
                .context(Stage::Archive, &target)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .context(Stage::Archive, &target)?;
            std::io::copy(&mut entry, &mut output).context(Stage::Archive, &target)?;
            let mode = entry.header().mode().context(Stage::Archive, &target)?;
            if mode != 0o644 && mode != 0o755 {
                return Err(bad("invalid packaged file permissions"));
            }
            files::set_mode(&target, mode)?;
        }
    }
    let path = staging.path().join("manifest.yml");
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .context(Stage::Archive, &path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .context(Stage::Archive, &path)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(bad("manifest too large"));
    }
    let manifest: PackageManifest =
        serde_yaml_ng::from_slice(&bytes).map_err(|_| bad("invalid manifest"))?;
    if manifest.schema != 2 || manifest.os != "linux" || manifest.architecture != architecture {
        return Err(bad("unsupported manifest platform or schema"));
    }
    if manifest.service.command.is_empty()
        || manifest.service.command.iter().any(|v| v.contains('\0'))
        || manifest
            .service
            .env
            .iter()
            .any(|(k, v)| !config::valid_env(k) || v.contains('\0'))
    {
        return Err(bad("invalid service command or environment"));
    }
    archive::check_service(staging.path(), &manifest.service)?;
    let mut actual: Vec<_> = archive::inventory(staging.path(), architecture)?
        .into_iter()
        .filter(|r| r.path != Path::new("manifest.yml"))
        .collect();
    actual.sort_by(|a, b| a.path.cmp(&b.path));
    let mut declared = manifest.files.clone();
    for record in &mut declared {
        record.path = files::relative(&record.path, false)?;
    }
    declared.sort_by(|a, b| a.path.cmp(&b.path));
    if declared != actual {
        return Err(bad("manifest inventory does not match package contents"));
    }
    fs::rename(staging.path(), destination).context(Stage::Archive, destination)?;
    Ok(manifest)
}
