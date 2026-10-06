use anyhow::{Context, Result, ensure};
use pier_protocol::upgrade::{Format, Release, Version};
use std::{
    fmt, io,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug)]
enum QueryError {
    Io(io::Error),
    Timeout,
    Exit(ExitStatus),
    InvalidOutput,
}
impl From<io::Error> for QueryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only bounded diagnostics reach the controller; never include command output.
        match self {
            Self::Io(error) => match error.kind() {
                io::ErrorKind::NotFound => f.write_str("未找到查询命令或所需文件"),
                io::ErrorKind::PermissionDenied => f.write_str("查询被拒绝访问"),
                _ => f.write_str("查询发生 I/O 错误"),
            },
            Self::Timeout => f.write_str("查询超过 20 秒未完成"),
            Self::Exit(status) => match status.code() {
                Some(code) => write!(f, "查询失败，退出码 {code}"),
                None => f.write_str("查询进程被信号终止"),
            },
            Self::InvalidOutput => f.write_str("查询返回的数据无效或过大"),
        }
    }
}
impl std::error::Error for QueryError {}

fn query(command: &mut Command) -> std::result::Result<String, QueryError> {
    use std::os::unix::process::CommandExt;
    let output = tempfile::tempfile()?;
    let mut child = command
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(output.try_clone()?)
        .process_group(0)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // SAFETY: child is unreaped and owns this process group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(QueryError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(QueryError::Exit(status));
    }
    if output.metadata()?.len() > 8192 {
        return Err(QueryError::InvalidOutput);
    }
    use std::io::{Read, Seek};
    let mut output = output;
    output.rewind()?;
    let mut text = String::new();
    output
        .read_to_string(&mut text)
        .map_err(|_| QueryError::InvalidOutput)?;
    Ok(text.trim().to_string())
}
fn target_version(value: &str, system: &str) -> Result<Version> {
    let (version, revision) = value.rsplit_once('-').context("missing package revision")?;
    let package = Version {
        version: version.into(),
        revision: revision.split('.').next().unwrap_or_default().parse()?,
    };
    ensure!(
        value == package.native(system)?,
        "native package version does not match target system"
    );
    Ok(package)
}
fn arch(value: &str, format: Format) -> Result<()> {
    let expected = match (crate::architecture()?, format) {
        (pier_pkg::Architecture::Amd64, Format::Deb) => "amd64",
        (pier_pkg::Architecture::Amd64, Format::Rpm) => "x86_64",
        (pier_pkg::Architecture::Arm64, Format::Deb) => "arm64",
        (pier_pkg::Architecture::Arm64, Format::Rpm) => "aarch64",
    };
    ensure!(value == expected, "package architecture mismatch");
    Ok(())
}
pub(super) fn installed(format: Format) -> Result<Version> {
    let (tool, queried) = match format {
        Format::Deb => (
            "dpkg-query",
            query(Command::new("dpkg-query").args(["-W", "-f=${Version}\n", "pier-agent"])),
        ),
        Format::Rpm => (
            "rpm",
            query(Command::new("rpm").args([
                "-q",
                "--qf",
                "%{EPOCHNUM}:%{VERSION}-%{RELEASE}\n",
                "pier-agent",
            ])),
        ),
    };
    let text = queried.map_err(|error| {
        anyhow::anyhow!(
            "无法读取 pier-agent 原生安装包记录：{tool} {error}；请检查安装包及包管理器"
        )
    })?;
    installed_version(&text).context("无法解析 pier-agent 已安装包版本；需要 x.y.z 和数字修订号")
}
fn installed_version(text: &str) -> Result<Version> {
    ensure!(
        !text.chars().any(char::is_whitespace),
        "invalid package version"
    );
    // Epoch zero is equivalent to no epoch. Other epochs cannot be represented
    // by the protocol's numeric version and revision, so parsing fails closed.
    let text = text.strip_prefix("0:").unwrap_or(text);
    let (version, release) = text.split_once('-').context("missing package revision")?;
    let revision = release.split('.').next().unwrap_or_default();
    ensure!(
        !revision.is_empty() && revision.bytes().all(|c| c.is_ascii_digit()),
        "invalid package revision"
    );
    let package = Version {
        version: version.into(),
        revision: revision.parse()?,
    };
    package.key()?;
    Ok(package)
}
pub(super) fn inspect(path: &Path, release: &Release) -> Result<()> {
    release.verify(path)?;
    let text = match release.format {
        Format::Deb => query(
            Command::new("dpkg-deb")
                .args(["-W", "--showformat=${Package}\n${Version}\n${Architecture}"])
                .arg(path),
        )?,
        Format::Rpm => query(
            Command::new("rpm")
                .args([
                    "-qp",
                    "--qf",
                    "%{NAME}\n%{VERSION}-%{RELEASE}\n%{ARCH}\n%{EPOCHNUM}",
                ])
                .arg(path),
        )?,
    };
    let lines: Vec<_> = text.lines().collect();
    ensure!(
        lines.len() == if release.format == Format::Deb { 3 } else { 4 },
        "invalid update package metadata"
    );
    ensure!(
        lines[0] == "pier-agent" && target_version(lines[1], &release.system)? == release.package,
        "update package name or version mismatch"
    );
    arch(lines[2], release.format)?;
    ensure!(
        release.format == Format::Deb || lines[3] == "0",
        "unsupported RPM epoch"
    );
    Ok(())
}
pub(super) fn systemctl(args: &[&str]) -> Result<String> {
    Ok(query(Command::new("systemctl").args(args))?)
}
pub(super) fn helper_active() -> bool {
    systemctl(&[
        "show",
        "--property=ActiveState",
        "--value",
        "pier-agent-upgrade.service",
    ])
    .is_ok_and(|s| matches!(s.as_str(), "active" | "activating"))
}
/// Run an installer in its own process group, bounded even if its scripts hang.
pub(super) fn bounded(command: &mut Command, limit: Duration) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut child = command
        .env("LC_ALL", "C")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()?;
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(
                status.success(),
                "upgrade command failed; inspect pier-agent-upgrade journal"
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            // SAFETY: child is an unreaped direct child with its own process group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            anyhow::bail!("upgrade command timed out; manual recovery required");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_versions_ignore_distribution_suffixes() {
        for value in [
            "1.2.3-12",
            "1.2.3-12.el8",
            "1.2.3-12.el9",
            "1.2.3-12.ubuntu22.04",
            "0:1.2.3-12.custom-build",
        ] {
            let parsed = installed_version(value).unwrap();
            assert_eq!(parsed.version, "1.2.3");
            assert_eq!(parsed.revision, 12);
        }
        for value in [
            "",
            "1.2.3",
            "1:1.2.3-12.el9",
            "1.2.3-0",
            "1.2.3-foo",
            "1.2.3-+1",
            "1.2.3-18446744073709551616.el9",
            "1.2.3-12.el9\n1.2.4-1.el9",
        ] {
            assert!(installed_version(value).is_err(), "{value}");
        }
    }

    #[test]
    fn query_diagnostics_distinguish_failures_without_exposing_output() {
        let missing = tempfile::tempdir().unwrap().path().join("missing-query");
        let error = query(&mut Command::new(missing)).unwrap_err();
        assert!(error.to_string().contains("未找到查询命令"));
        let error = query(Command::new("/bin/sh").args([
            "-c",
            "printf private-output; printf private-error >&2; exit 7",
        ]))
        .unwrap_err();
        assert_eq!(error.to_string(), "查询失败，退出码 7");
        let error =
            query(Command::new("/bin/sh").args(["-c", "head -c 9000 /dev/zero"])).unwrap_err();
        assert!(error.to_string().contains("数据无效或过大"));
        assert!(QueryError::Timeout.to_string().contains("20 秒"));
    }

    #[test]
    fn native_versions_reject_epochs_foreign_releases_and_invalid_revisions() {
        for (s, system) in [
            ("1.2.3-12.el8", "almalinux8"),
            ("1.2.3-12.el9", "almalinux9"),
            ("1.2.3-12.ubuntu24.04", "ubuntu24.04"),
        ] {
            assert_eq!(target_version(s, system).unwrap().revision, 12);
        }
        assert!(target_version("1.2.3-2", "ubuntu24.04").is_err());
        for (s, system) in [
            ("1:1.2.3-1", "ubuntu24.04"),
            ("1.2.3-1.el9", "almalinux8"),
            ("1.2.3-1.el8", "almalinux9"),
            ("1.2.3-1.ubuntu22.04", "ubuntu24.04"),
            ("1.2.3-1.el9", "ubuntu24.04"),
            ("1.2.3-0", "ubuntu24.04"),
            ("1.2.3-foo", "ubuntu24.04"),
            ("1.2.3-01.el9", "almalinux9"),
            ("1.2.3-18446744073709551616.el9", "almalinux9"),
        ] {
            assert!(target_version(s, system).is_err());
        }
    }
}
