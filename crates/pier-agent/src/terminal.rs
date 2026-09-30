//! App-owned PTYs, isolated from service supervision and control traffic.
use crate::{
    Runtime,
    account::{self, Account},
};
use anyhow::{Context, Result, ensure};
use pier_protocol::{
    secure::{self, Purpose},
    terminal::{self as protocol, Frame},
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::MetadataExt, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use tokio::{
    io::unix::AsyncFd,
    sync::watch,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

struct Entry {
    uid: u32,
    sid: i32,
    started: u64,
    close: watch::Sender<Option<String>>,
}
#[derive(Default)]
pub(crate) struct Manager {
    // Serializes PTY registration with the supervisor's account cleanup.
    entries: Mutex<BTreeMap<String, Entry>>,
}
impl Manager {
    pub fn cleanup(&self, account: &Account) -> Result<()> {
        let entries = self.entries.lock().unwrap();
        let preserved: Vec<_> = entries
            .values()
            .filter(|e| e.uid == account.uid && process_identity(e.sid) == Some((e.sid, e.started)))
            .map(|e| e.sid)
            .collect();
        account::cleanup_preserving(account, &preserved)
    }
    pub fn close_all(&self, reason: &str) {
        for entry in self.entries.lock().unwrap().values() {
            let _ = entry.close.send(Some(reason.into()));
            signal_entry(entry, libc::SIGTERM);
        }
    }
}

/// Linux stat fields 6 (session) and 22 (starttime); comm can contain spaces/parentheses.
pub(crate) fn process_identity(pid: i32) -> Option<(i32, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(") ")?;
    let fields: Vec<_> = rest.split_whitespace().collect();
    Some((fields.get(3)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}
fn signal_session(uid: u32, sid: i32, signal: i32) {
    if uid == 0 || sid <= 1 {
        return;
    }
    let Ok(processes) = fs::read_dir("/proc") else {
        return;
    };
    for process in processes.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        let identity = process_identity(pid);
        if pid > 1
            && identity.is_some_and(|(session, _)| session == sid)
            && process.metadata().is_ok_and(|m| m.uid() == uid)
            && process_identity(pid) == identity
        {
            // SAFETY: positive PID, rechecked UID, session and process start time.
            unsafe {
                libc::kill(pid, signal);
            }
        }
    }
}

fn signal_entry(entry: &Entry, signal: i32) {
    if process_identity(entry.sid).is_some_and(|identity| identity != (entry.sid, entry.started)) {
        return;
    }
    signal_session(entry.uid, entry.sid, signal);
}

struct Pty {
    id: String,
    account: Account,
    master: File,
    child: Child,
    manager: Arc<Manager>,
    close: watch::Receiver<Option<String>>,
}
impl Drop for Pty {
    fn drop(&mut self) {
        let mut entries = self.manager.entries.lock().unwrap();
        if let Some(entry) = entries.get(&self.id) {
            signal_entry(entry, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        entries.remove(&self.id);
    }
}
impl Runtime {
    fn terminal(&self, id: String, instance: &str, cols: u16, rows: u16) -> Result<Pty> {
        protocol::size(cols, rows)?;
        ensure!(
            pier_protocol::safe_id(&id) && pier_protocol::safe_id(instance),
            "invalid terminal identity"
        );
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| anyhow::anyhow!("deployment in progress"))?;
        self.stopped()?;
        ensure!(!self.maintenance.load(Ordering::SeqCst), "agent upgrading");
        let durable: crate::DurableState = self.store.get("runtime", "state")?.unwrap_or_default();
        ensure!(durable.pending.is_none(), "deployment in progress");
        let app = durable
            .snapshot
            .apps
            .iter()
            .find(|app| app.instance == instance)
            .context("app not installed")?;
        let account = app.account.clone();
        account::verify(&account)?;
        ensure!(
            account.uid != 0 && account.gid != 0 && account.home.is_dir(),
            "invalid app account"
        );
        let mut entries = self.terminals.entries.lock().unwrap();
        ensure!(
            entries.len() < 8 && !entries.contains_key(&id),
            "terminal limit or duplicate request"
        );
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: valid out pointers and winsize; null name avoids unbounded writes.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openpty returned two new owned file descriptors.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: valid owned descriptor; prevent leaks into unrelated services.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        // SAFETY: valid slave descriptor, verified non-root owner.
        if unsafe { libc::fchown(slave.as_raw_fd(), account.uid, account.gid) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: master is private to this terminal.
        if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut command = Command::new("/bin/bash");
        command
            .arg("-il")
            .current_dir(&account.home)
            .env_clear()
            .env("HOME", &account.home)
            .env("USER", &account.name)
            .env("LOGNAME", &account.name)
            .env("SHELL", "/bin/bash")
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        let (uid, gid) = (account.uid, account.gid);
        // SAFETY: only credential/terminal syscalls, no allocation after fork.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0
                    || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0
                    || libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let Some((sid, started)) = process_identity(child.id() as i32) else {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("shell exited during startup");
        };
        let (close_tx, close) = watch::channel(None);
        entries.insert(
            id.clone(),
            Entry {
                uid,
                sid,
                started,
                close: close_tx,
            },
        );
        Ok(Pty {
            id,
            account,
            master,
            child,
            manager: self.terminals.clone(),
            close,
        })
    }
}

async fn read(fd: &AsyncFd<File>, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut ready = fd.readable().await?;
        match ready.try_io(|inner| inner.get_ref().read(buffer)) {
            Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
            Ok(result) => return result,
            Err(_) => (),
        }
    }
}
async fn write(fd: &AsyncFd<File>, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let mut ready = fd.writable().await?;
        if let Ok(result) = ready.try_io(|inner| inner.get_ref().write(bytes)) {
            let n = result?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[n..];
        }
    }
    Ok(())
}

pub(crate) struct Request {
    pub id: String,
    pub instance: String,
    pub cols: u16,
    pub rows: u16,
}
pub(crate) async fn serve(
    runtime: Arc<Runtime>,
    request: Request,
    cancelled: CancellationToken,
) -> Result<()> {
    let token = fs::read_to_string(&runtime.config.token_file)?;
    let stream = secure::connect(
        &runtime.config.controller_tcp,
        Purpose::Terminal,
        &runtime.config.agent_id,
        &secure::token_key(&token),
    )
    .await?;
    let mut wire = pier_protocol::framed(stream);
    wire.codec_mut().set_max_frame_length(protocol::MAX_FRAME);
    protocol::send(
        &mut wire,
        &Frame::Attach {
            id: request.id.clone(),
        },
    )
    .await?;
    let worker = runtime.clone();
    let stop = cancelled.clone();
    let opened = tokio::task::spawn_blocking(move || {
        ensure!(!stop.is_cancelled(), "control connection closed");
        worker.terminal(request.id, &request.instance, request.cols, request.rows)
    })
    .await?;
    let mut pty = match opened {
        Ok(pty) => pty,
        Err(_) => {
            protocol::send(
                &mut wire,
                &Frame::Exit {
                    code: None,
                    reason: "terminal_unavailable".into(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let result: Result<(Option<i32>, String)> = async {
        let fd = AsyncFd::new(pty.master.try_clone()?)?;
        protocol::send(&mut wire, &Frame::Ready { user: pty.account.name.clone(), home: pty.account.home.to_string_lossy().into() }).await?;
        let mut buffer = vec![0u8; protocol::CHUNK];
        let mut outstanding = 0usize;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        let mut poll = tokio::time::interval(Duration::from_millis(100));
        let mut received = Instant::now();
        let mut shell_exit = None;
        'session: loop {
            tokio::select! {
                biased;
                _ = cancelled.cancelled() => break Ok((None, "connection_lost".to_string())),
                _ = pty.close.changed() => break Ok((None, pty.close.borrow().clone().unwrap_or_else(|| "closed".into()))),
                _ = tokio::time::sleep_until(received + Duration::from_secs(45)) => anyhow::bail!("terminal heartbeat timeout"),
                message = protocol::receive(&mut wire) => {
                    received = Instant::now();
                    match message? {
                        Frame::Input { data } => {
                            let bytes = protocol::decode(&data)?;
                            timeout(Duration::from_secs(10), write(&fd, &bytes)).await??;
                        }
                        Frame::Resize { cols, rows } => {
                            protocol::size(cols, rows)?;
                            let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
                            // SAFETY: live PTY descriptor and valid winsize.
                            ensure!(unsafe { libc::ioctl(fd.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &size) } == 0, "cannot resize PTY");
                        }
                        Frame::Ack { bytes } => { ensure!(bytes > 0 && bytes <= outstanding, "invalid output acknowledgement"); outstanding -= bytes; }
                        Frame::Ping => protocol::send(&mut wire, &Frame::Pong).await?,
                        Frame::Pong => (),
                        _ => anyhow::bail!("unexpected terminal message"),
                    }
                }
                _ = heartbeat.tick() => {
                    ensure!(received.elapsed() < Duration::from_secs(45), "terminal heartbeat timeout");
                    protocol::send(&mut wire, &Frame::Ping).await?;
                }
                n = read(&fd, &mut buffer), if outstanding <= protocol::WINDOW - protocol::CHUNK => {
                    let n = n?;
                    if n == 0 { break Ok((pty.child.try_wait()?.and_then(|s| s.code()), "shell_exited".into())); }
                    outstanding += n;
                    protocol::send(&mut wire, &Frame::Output { data: protocol::encode(&buffer[..n]) }).await?;
                }
                _ = poll.tick() => {
                    if shell_exit.is_none() {
                        shell_exit = pty.child.try_wait()?.map(|s| s.code());
                    }
                    if let Some(code) = shell_exit {
                        // A full window must wait for acknowledgements, including after exit.
                        while outstanding <= protocol::WINDOW - protocol::CHUNK {
                            match (&pty.master).read(&mut buffer) {
                                Ok(0) => break 'session Ok((code, "shell_exited".into())),
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == Some(libc::EIO) => break 'session Ok((code, "shell_exited".into())),
                                Err(error) => return Err(error.into()),
                                Ok(n) => { outstanding += n; protocol::send(&mut wire, &Frame::Output { data: protocol::encode(&buffer[..n]) }).await?; }
                            }
                        }
                    }
                }
            }
        }
    }.await;
    if let Ok((code, reason)) = &result {
        let _ = protocol::send(
            &mut wire,
            &Frame::Exit {
                code: *code,
                reason: reason.clone(),
            },
        )
        .await;
    }
    // /proc scanning and waitpid must not block Tokio's network workers.
    tokio::task::spawn_blocking(move || drop(pty)).await?;
    result.map(|_| ())
}
