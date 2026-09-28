use anyhow::{Context, Result, ensure};
use std::{
    os::{
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStrExt,
            net::{SocketAddr, UnixDatagram},
        },
    },
    path::Path,
    process::Command,
};

pub(crate) fn notify_ready() -> Result<()> {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let bytes = path.as_bytes();
    let address = if bytes.first() == Some(&b'@') {
        SocketAddr::from_abstract_name(&bytes[1..])?
    } else {
        SocketAddr::from_pathname(&path)?
    };
    UnixDatagram::unbound()?.send_to_addr(
        b"READY=1\nSTATUS=Local state restored; connecting to controller",
        &address,
    )?;
    Ok(())
}
pub(crate) fn available() -> Result<()> {
    ensure!(
        Path::new("/run/systemd/system").is_dir(),
        "需要正在运行的 systemd；请在安装了 pier-agent 软件包的服务器上执行 init"
    );
    let status = Command::new("systemctl")
        .args(["cat", "pier-agent.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    ensure!(
        status.success(),
        "未安装 pier-agent.service，请先安装 DEB/RPM 包"
    );
    Ok(())
}
pub(crate) fn start() -> Result<()> {
    // A newly installed unit may not yet be loaded by the manager. Resetting
    // such a unit fails; only clear the start limit for a failed service.
    if Command::new("systemctl")
        .args(["is-failed", "--quiet", "pier-agent.service"])
        .status()?
        .success()
    {
        let reset = Command::new("systemctl")
            .args(["reset-failed", "pier-agent.service"])
            .status()?;
        ensure!(reset.success(), "无法重置服务失败状态，请检查 systemd");
    }
    let result = Command::new("systemctl")
        .args(["enable", "--now", "pier-agent.service"])
        .status()
        .context("无法运行 systemctl")?;
    ensure!(
        result.success(),
        "配置已保存，但服务启动失败。请执行 systemctl status pier-agent 和 journalctl -u pier-agent -n 50 检查后重试 init"
    );
    ensure!(
        Command::new("systemctl")
            .args(["is-active", "--quiet", "pier-agent.service"])
            .status()?
            .success(),
        "服务尚未运行，请查看 journalctl -u pier-agent"
    );
    println!(
        "pier-agent 已启动并启用开机自启。\n状态：systemctl status pier-agent\n日志：journalctl -u pier-agent -f"
    );
    Ok(())
}
pub(crate) fn status() -> Result<()> {
    Command::new("systemctl")
        .args(["status", "--no-pager", "pier-agent.service"])
        .status()?;
    Ok(())
}
