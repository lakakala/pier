use crate::{
    Architecture, Error, Result, Stage,
    config::{Build, Language},
    files,
    process::{self, Redactor},
    proxy::Proxy,
    types::IoResult,
};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

struct Container {
    name: String,
}
impl Drop for Container {
    fn drop(&mut self) {
        // Only remove the uniquely named container owned by this invocation.
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub(crate) fn compile(
    source: &Path,
    output: &Path,
    architecture: Architecture,
    image: &str,
    build: &Build,
    proxy: &Proxy,
    redactor: &Redactor,
) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    return Err(Error::new(
        Stage::Build,
        "Docker source builds currently require a Linux host",
    ));
    #[cfg(target_os = "linux")]
    {
        let workdir = files::resolve(source, &build.workdir, Stage::Build)?;
        if !workdir.is_dir() {
            return Err(Error::new(Stage::Build, "build.workdir must be a directory").at(workdir));
        }
        fs::create_dir_all(output).context(Stage::Build, output)?;
        let name_file = tempfile::Builder::new()
            .prefix("pier-container-")
            .tempfile()
            .context(Stage::Build, output)?;
        let container = Container {
            name: name_file
                .path()
                .file_name()
                .expect("temporary name")
                .to_string_lossy()
                .into_owned(),
        };
        let mut env = build.env.clone();
        env.extend(proxy.environment());
        env.insert("PIER_TARGET".into(), format!("linux/{architecture}"));
        env.insert("PIER_ARCH".into(), architecture.to_string());
        env.insert("PIER_RUST_TARGET".into(), architecture.rust_target().into());
        env.insert("PIER_OUTPUT".into(), "/output".into());
        match build.language {
            Language::Rust => {
                env.insert(
                    "CARGO_BUILD_TARGET".into(),
                    architecture.rust_target().into(),
                );
                env.entry("CARGO_HOME".into())
                    .or_insert("/tmp/pier-cargo".into());
            }
            Language::Go => {
                env.insert("GOOS".into(), "linux".into());
                env.insert("GOARCH".into(), architecture.as_str().into());
                env.entry("CGO_ENABLED".into()).or_insert("0".into());
                env.entry("GOCACHE".into())
                    .or_insert("/tmp/pier-go-cache".into());
                env.entry("GOMODCACHE".into())
                    .or_insert("/tmp/pier-go-mod".into());
            }
        }
        let mut command = Command::new("docker");
        // Values are passed using the Docker CLI environment, never shell interpolation.
        proxy.command(&mut command);
        command
            .args(["run", "--rm", "--platform"])
            .arg(format!("linux/{}", architecture.as_str()))
            .args(["--name", &container.name, "--network", "bridge", "--user"])
            .arg(format!(
                "{}:{}",
                rustix::process::getuid().as_raw(),
                rustix::process::getgid().as_raw()
            ));
        for (path, dest) in [(source, "/src"), (output, "/output")] {
            let path = path.canonicalize().context(Stage::Build, path)?;
            let p = path.to_string_lossy();
            if p.contains(':') {
                return Err(Error::new(
                    Stage::Build,
                    "temporary workspace path cannot contain ':'",
                )
                .at(path));
            }
            command.arg("--volume").arg(format!("{p}:{dest}"));
        }
        command
            .arg("--workdir")
            .arg(Path::new("/src").join(files::relative(&build.workdir, true)?));
        for (key, value) in env {
            command.arg("--env").arg(&key).env(key, value);
        }
        // Explicit values above override Docker's configured default container proxies.
        let expected_machine = match architecture {
            crate::Architecture::Amd64 => "x86_64",
            crate::Architecture::Arm64 => "aarch64",
        };
        command
            .args(["--entrypoint", "/bin/sh", image, "-ec"])
            .arg(format!(
                "set -eu\ntest \"$(uname -m)\" = {expected_machine}\n{}",
                build.commands.join("\n")
            ));
        process::run(
            &mut command,
            Stage::Build,
            "target-platform Docker build (requires Docker/QEMU support for the selected architecture)",
            redactor,
            false,
        )?;
        drop(container);
        Ok(())
    }
}
