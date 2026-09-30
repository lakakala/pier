use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub marker: String,
    pub home: PathBuf,
}
fn lookup(name: &str) -> Result<Option<Vec<String>>> {
    let output = Command::new("getent").args(["passwd", name]).output()?;
    if output.status.code() == Some(2) {
        return Ok(None);
    }
    ensure!(output.status.success(), "cannot inspect system user");
    let text = String::from_utf8(output.stdout)?;
    let fields: Vec<_> = text.trim_end().split(':').map(String::from).collect();
    ensure!(fields.len() == 7, "invalid passwd record");
    Ok(Some(fields))
}
pub fn create(agent: &str, instance: &str, home: &Path) -> Result<Account> {
    let digest = pier_protocol::hash(format!("{agent}/{instance}"));
    let name = format!("pier_{}", &digest[..20]);
    let marker = format!("pier-agent={agent}/{instance}");
    if lookup(&name)?.is_none() {
        let shell = if Path::new("/usr/sbin/nologin").exists() {
            "/usr/sbin/nologin"
        } else {
            "/sbin/nologin"
        };
        let output = Command::new("useradd")
            .args([
                "--system",
                "--user-group",
                "--no-create-home",
                "--shell",
                shell,
                "--comment",
                &marker,
                "--home-dir",
            ])
            .arg(home)
            .arg(&name)
            .output()?;
        ensure!(output.status.success(), "cannot create dedicated app user");
    }
    let fields = lookup(&name)?.context("created user missing")?;
    ensure!(
        fields[0] == name && fields[4] == marker && Path::new(&fields[5]) == home,
        "existing user is not owned by this agent app"
    );
    let uid = fields[2].parse()?;
    let gid = fields[3].parse()?;
    ensure!(uid != 0 && gid != 0, "app cannot run as root");
    Ok(Account {
        name,
        marker,
        uid,
        gid,
        home: home.to_path_buf(),
    })
}
pub fn verify(account: &Account) -> Result<()> {
    let fields = lookup(&account.name)?.context("app account no longer exists")?;
    ensure!(
        fields[2].parse::<u32>()? == account.uid
            && fields[3].parse::<u32>()? == account.gid
            && fields[4] == account.marker
            && Path::new(&fields[5]) == account.home,
        "app account identity changed"
    );
    Ok(())
}
pub fn directory(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<()> {
    fs::create_dir_all(path)?;
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "managed directory cannot be a symlink"
    );
    chown(path, uid, gid)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
pub fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: valid NUL-terminated path; numeric IDs were checked against passwd.
    if unsafe { libc::lchown(path.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn release_permissions(path: &Path, gid: u32) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "release contains symlink"
    );
    ensure!(
        metadata.is_dir() || metadata.is_file(),
        "release contains special file"
    );
    chown(path, 0, gid)?;
    let mode = if metadata.is_dir() || metadata.permissions().mode() & 0o111 != 0 {
        0o750
    } else {
        0o640
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            release_permissions(&entry?.path(), gid)?;
        }
    }
    Ok(())
}
fn processes(uid: u32) -> Result<Vec<i32>> {
    let mut result = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let is_uid = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|line| line.split_whitespace().next())
            .and_then(|value| value.parse::<u32>().ok())
            == Some(uid);
        let zombie = status
            .lines()
            .any(|line| line.starts_with("State:") && line.split_whitespace().nth(1) == Some("Z"));
        if is_uid && !zombie {
            result.push(pid);
        }
    }
    Ok(result)
}
/// Accounts are exclusive to this agent, verified before every recovery cleanup.
pub fn cleanup(account: &Account) -> Result<()> {
    cleanup_preserving(account, &[])
}
pub(crate) fn cleanup_preserving(account: &Account, sessions: &[i32]) -> Result<()> {
    verify(account)?;
    for round in 0..20 {
        let pids: Vec<_> = processes(account.uid)?
            .into_iter()
            .filter(|pid| {
                !crate::terminal::process_identity(*pid)
                    .is_some_and(|(sid, _)| sessions.contains(&sid))
            })
            .collect();
        if pids.is_empty() {
            return Ok(());
        }
        let signal = if round == 0 {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        };
        for pid in pids {
            // Recheck ownership immediately before signalling; never signal root or
            // a UID obtained only from a stale PID file.
            let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            if status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|line| line.split_whitespace().next())
                .and_then(|s| s.parse::<u32>().ok())
                == Some(account.uid)
            {
                // SAFETY: positive non-init PID and verified dedicated UID.
                unsafe {
                    libc::kill(pid, signal);
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("cannot clean remaining app processes")
}
