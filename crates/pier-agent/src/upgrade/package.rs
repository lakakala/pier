use anyhow::{Context, Result, ensure};
use pier_protocol::upgrade::{Format, Release, Version};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn query(command: &mut Command) -> Result<String> {
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
            anyhow::bail!("package metadata query timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    ensure!(
        status.success() && output.metadata()?.len() <= 8192,
        "package metadata unavailable"
    );
    use std::io::{Read, Seek};
    let mut output = output;
    output.rewind()?;
    let mut text = String::new();
    output.read_to_string(&mut text)?;
    Ok(text.trim().to_string())
}
pub(super) fn version(value: &str, format: Format) -> Result<Version> {
    let (version, revision) = value.rsplit_once('-').context("missing package revision")?;
    let revision = match format {
        Format::Deb => revision,
        Format::Rpm => revision
            .strip_suffix(".el8")
            .context("unsupported RPM release")?,
    };
    let value = Version {
        version: version.into(),
        revision: revision.parse()?,
    };
    value.key()?;
    Ok(value)
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
    let text = match format {
        Format::Deb => query(Command::new("dpkg-query").args([
            "-W",
            "-f=${Status}\n${Version}\n${Architecture}",
            "pier-agent",
        ]))?,
        Format::Rpm => query(Command::new("rpm").args([
            "-q",
            "--qf",
            "%{EPOCHNUM}\n%{VERSION}-%{RELEASE}\n%{ARCH}",
            "pier-agent",
        ]))?,
    };
    let lines: Vec<_> = text.lines().collect();
    ensure!(lines.len() == 3, "invalid installed package metadata");
    ensure!(
        lines[0]
            == if format == Format::Deb {
                "install ok installed"
            } else {
                "0"
            },
        "package not fully installed or unsupported epoch"
    );
    arch(lines[2], format)?;
    version(lines[1], format)
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
        lines[0] == "pier-agent" && version(lines[1], release.format)? == release.package,
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
    query(Command::new("systemctl").args(args))
}
pub(super) fn managed() -> bool {
    let exe = std::env::current_exe().ok();
    let owned = exe.as_deref().is_some_and(|p| {
        p == Path::new("/usr/bin/pier-agent") || p == Path::new("/usr/bin/pier-agent (deleted)")
    });
    owned
        && systemctl(&[
            "show",
            "--property=MainPID",
            "--value",
            "pier-agent.service",
        ])
        .is_ok_and(|p| p == std::process::id().to_string())
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
    fn native_versions_reject_epochs_foreign_releases_and_invalid_revisions() {
        assert_eq!(version("1.2.3-12.el8", Format::Rpm).unwrap().revision, 12);
        assert_eq!(version("1.2.3-2", Format::Deb).unwrap().version, "1.2.3");
        for (s, f) in [
            ("1:1.2.3-1", Format::Deb),
            ("1.2.3-1.el9", Format::Rpm),
            ("1.2.3-0", Format::Deb),
            ("1.2.3-foo", Format::Deb),
        ] {
            assert!(version(s, f).is_err());
        }
    }
}
