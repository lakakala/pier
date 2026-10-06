use crate::{
    Error, PackOptions, PackageArtifact, PlannedPackage, Result, Stage, ValidationReport, acquire,
    archive, build,
    config::{self, FileBase, Recipe, Source},
    files,
    process::Redactor,
    proxy::Proxy,
    templates,
    types::IoResult,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

struct Prepared {
    root: PathBuf,
    recipe: Recipe,
    proxy: Proxy,
    redactor: Redactor,
    configs: BTreeMap<PathBuf, Vec<u8>>,
    report: ValidationReport,
}

fn prepare(root: &Path, options: &PackOptions) -> Result<Prepared> {
    let root = root.canonicalize().context(Stage::Configuration, root)?;
    if !root.is_dir() {
        return Err(Error::new(Stage::Configuration, "service_dir must be a directory").at(root));
    }
    let (recipe, variables) = config::load(&root, options)?;
    let proxy = Proxy::new(
        recipe.proxy.enabled,
        &options.proxy,
        matches!(recipe.source, Source::Git { .. }),
    )?;
    let mut secrets = proxy.redactions();
    secrets.extend(
        variables
            .iter()
            .filter(|(key, _)| key.as_str() != "PIER_ARCH")
            .map(|(_, value)| value.clone()),
    );
    if let Some(build) = &recipe.build {
        secrets.extend(build.env.values().cloned());
    }
    if let Source::Git { repo, .. } = &recipe.source {
        secrets.push(repo.clone());
    }
    let redactor = Redactor::new(secrets);
    let configs = templates::render(&root, &variables)?;
    let output = if options.output_dir.is_absolute() {
        options.output_dir.clone()
    } else {
        std::env::current_dir()
            .context(Stage::Configuration, ".")?
            .join(&options.output_dir)
    };
    for mapping in &recipe.files {
        if matches!(mapping.base, FileBase::Recipe) {
            let path = files::resolve(&root, &mapping.from, Stage::Configuration)?;
            if path.is_dir() {
                files::walk(&path)?;
            }
        }
    }
    let path = output.join(format!(
        "{}-{}-linux-{}.tar.gz",
        recipe.name, recipe.version, options.architecture
    ));
    match fs::symlink_metadata(&path) {
        Ok(m) if !options.overwrite || !m.is_file() || m.file_type().is_symlink() => {
            return Err(Error::new(
                Stage::Configuration,
                "output already exists or is not a regular file",
            )
            .at(path));
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(Error::new(Stage::Configuration, "cannot inspect output")
                .at(path)
                .cause(e));
        }
        _ => (),
    }
    let report = ValidationReport {
        name: recipe.name.clone(),
        version: recipe.version.clone(),
        package: PlannedPackage {
            architecture: options.architecture,
            path,
        },
        configuration_files: configs.keys().cloned().collect(),
    };
    Ok(Prepared {
        root,
        recipe,
        proxy,
        redactor,
        configs,
        report,
    })
}

/// Check the recipe, variables, templates, image and proxy settings for one architecture.
/// This does not make network requests, start Docker, or write output files.
pub fn validate(service_dir: impl AsRef<Path>, options: &PackOptions) -> Result<ValidationReport> {
    prepare(service_dir.as_ref(), options)
        .map(|prepared| prepared.report)
        .map_err(|mut error| {
            error.architecture = Some(options.architecture);
            error
        })
}

/// Build/download and atomically publish one package for the requested architecture.
/// No global environment or process working directory is changed.
pub fn pack(service_dir: impl AsRef<Path>, options: &PackOptions) -> Result<PackageArtifact> {
    prepare(service_dir.as_ref(), options)
        .and_then(|prepared| package(prepared, options))
        .map_err(|mut error| {
            error.architecture = Some(options.architecture);
            error
        })
}

fn package(prepared: Prepared, options: &PackOptions) -> Result<PackageArtifact> {
    tracing::info!(architecture=%options.architecture, "packaging service");
    let workspace = tempfile::Builder::new()
        .prefix("pier-pkg-")
        .tempdir()
        .context(Stage::Files, &prepared.root)?;
    let artifacts = workspace.path().join("artifacts");
    let commit = match &prepared.recipe.source {
        Source::Git { repo, r#ref } => {
            // The checkout belongs to this single invocation; preserve Git executable modes.
            let source = workspace.path().join("source");
            let commit = acquire::checkout(
                repo,
                r#ref,
                &prepared.root,
                &source,
                &prepared.proxy,
                &prepared.redactor,
            )?;
            build::compile(
                &source,
                &artifacts,
                options.architecture,
                options.image.as_deref().expect("validated image"),
                prepared.recipe.build.as_ref().expect("validated build"),
                &prepared.proxy,
                &prepared.redactor,
            )?;
            Some(commit)
        }
        Source::Binary {
            url,
            format,
            sha256,
        } => {
            acquire::download(url, *format, sha256.as_deref(), &artifacts, &prepared.proxy)?;
            None
        }
    };
    let staging = workspace.path().join("package");
    fs::create_dir(&staging).context(Stage::Files, &staging)?;
    for (path, bytes) in &prepared.configs {
        files::write(&staging.join(path), bytes)?;
    }
    for mapping in &prepared.recipe.files {
        let base = match mapping.base {
            FileBase::Artifact => &artifacts,
            FileBase::Recipe => &prepared.root,
        };
        let from = files::resolve(base, &mapping.from, Stage::Files)?;
        files::copy(
            &from,
            &staging.join(files::relative(&mapping.to, false)?),
            mapping.executable,
        )?;
    }
    archive::check_service(&staging, &prepared.recipe.service)?;
    let manifest = archive::Manifest {
        ports: crate::resolve_ports(&prepared.recipe.ports, &BTreeMap::new())?,
        schema: 2,
        name: &prepared.recipe.name,
        version: &prepared.recipe.version,
        os: "linux",
        architecture: options.architecture,
        image: options.image.as_deref(),
        service: &prepared.recipe.service,
        source_commit: commit.as_deref(),
        files: archive::inventory(&staging, options.architecture)?,
    };
    let yaml = serde_yaml_ng::to_string(&manifest)
        .map_err(|e| Error::new(Stage::Archive, "cannot serialize manifest").cause(e))?;
    files::write(&staging.join("manifest.yml"), yaml.as_bytes())?;
    let path = prepared.report.package.path;
    let sha256 = archive::publish(&staging, &path, options.overwrite)?;
    Ok(PackageArtifact {
        architecture: options.architecture,
        path,
        sha256,
    })
}
