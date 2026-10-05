use crate::{Installed, RuntimeOptions, account};
use anyhow::Result;
use pier_protocol::AppStatus;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// One worker owns the complete blueprint; individual children never clean a UID.
pub struct Supervisor {
    stop: Arc<AtomicBool>,
    pub status: Arc<Mutex<Vec<AppStatus>>>,
    worker: Option<JoinHandle<()>>,
}
impl Supervisor {
    pub fn start(
        apps: Vec<Installed>,
        options: RuntimeOptions,
        observe: bool,
        terminals: Arc<crate::terminal::Manager>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(
            apps.iter()
                .map(|app| AppStatus {
                    instance: app.instance.clone(),
                    id: app.id.clone(),
                    state: "starting".into(),
                    ..AppStatus::default()
                })
                .collect::<Vec<_>>(),
        ));
        let (worker_stop, worker_status) = (stop.clone(), status.clone());
        let worker = thread::spawn(move || {
            let mut delay = 1;
            let mut ready = false;
            while !worker_stop.load(Ordering::SeqCst) {
                for status in worker_status.lock().unwrap().iter_mut() {
                    status.state = "starting".into();
                }
                let started = Instant::now();
                if let Err(error) = run_group(
                    &apps,
                    &options,
                    &worker_stop,
                    &worker_status,
                    &mut ready,
                    &terminals,
                ) {
                    tracing::warn!(%error, "blueprint process operation failed");
                }
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                if observe && !ready {
                    for status in worker_status.lock().unwrap().iter_mut() {
                        status.state = "failed".into();
                    }
                    return;
                }
                for status in worker_status.lock().unwrap().iter_mut() {
                    status.state = "backoff".into();
                    status.pid = None;
                    status.restarts += 1;
                }
                if started.elapsed() >= Duration::from_secs(60) {
                    delay = 1;
                }
                pause(&worker_stop, Duration::from_secs(delay));
                delay = (delay * 2).min(60);
            }
            for status in worker_status.lock().unwrap().iter_mut() {
                status.state = "stopped".into();
                status.pid = None;
            }
        });
        Self {
            stop,
            status,
            worker: Some(worker),
        }
    }
    pub fn stop(&mut self) {
        self.request_stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}
fn pause(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}
struct Process {
    child: Child,
    output_stop: Arc<AtomicBool>,
    out: Option<JoinHandle<()>>,
    err: Option<JoinHandle<()>>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.output_stop.store(true, Ordering::SeqCst);
        for worker in [self.out.take(), self.err.take()].into_iter().flatten() {
            let _ = worker.join();
        }
    }
}
fn spawn(installed: &Installed, status: &Mutex<Vec<AppStatus>>, index: usize) -> Result<Process> {
    let service = &installed.manifest.service;
    let mut command = Command::new(installed.release.join(&service.command[0]));
    command
        .args(&service.command[1..])
        .current_dir(installed.release.join(&service.working_dir))
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .envs(&service.env)
        .env("HOME", &installed.account.home)
        .env("USER", &installed.account.name)
        .env("LOGNAME", &installed.account.name)
        .env("PIER_DATA_DIR", &installed.account.home)
        .env("PIER_LOG_DIR", &installed.logs)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let uid = installed.account.uid;
    let gid = installed.account.gid;
    // SAFETY: callback invokes only async-signal-safe credential syscalls and
    // performs no allocation. Clear groups BEFORE irreversibly dropping root.
    unsafe {
        command.pre_exec(move || {
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    status.lock().unwrap()[index].pid = Some(child.id());
    let output_stop = Arc::new(AtomicBool::new(false));
    let out = logger(
        child.stdout.take().unwrap(),
        installed.logs.join("stdout.log"),
        output_stop.clone(),
        gid,
    );
    let err = logger(
        child.stderr.take().unwrap(),
        installed.logs.join("stderr.log"),
        output_stop.clone(),
        gid,
    );
    Ok(Process {
        child,
        output_stop,
        out: Some(out),
        err: Some(err),
    })
}
fn run_group(
    apps: &[Installed],
    options: &RuntimeOptions,
    stop: &AtomicBool,
    status: &Mutex<Vec<AppStatus>>,
    ready: &mut bool,
    terminals: &crate::terminal::Manager,
) -> Result<()> {
    let account = &apps[0].account;
    account::verify(account)?;
    terminals.cleanup(account)?;
    let mut children = Vec::new();
    let result = (|| {
        for (i, app) in apps.iter().enumerate() {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            children.push(spawn(app, status, i)?);
        }
        let start = Instant::now();
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            for (i, process) in children.iter_mut().enumerate() {
                if let Some(exit) = process.child.try_wait()? {
                    status.lock().unwrap()[i].exit_code = exit.code();
                    return Ok(());
                }
            }
            if start.elapsed() >= Duration::from_secs(options.startup_grace_seconds) {
                *ready = true;
                for status in status.lock().unwrap().iter_mut() {
                    status.state = "running".into();
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    })();
    // Request all stops before waiting; descendants are cleaned once per blueprint.
    for process in &mut children {
        if matches!(process.child.try_wait(), Ok(None)) {
            // SAFETY: this worker owns the unreaped process and therefore its PGID.
            unsafe {
                libc::kill(-(process.child.id() as i32), libc::SIGTERM);
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(options.stop_timeout_seconds);
    while Instant::now() < deadline
        && children
            .iter_mut()
            .any(|p| matches!(p.child.try_wait(), Ok(None)))
    {
        thread::sleep(Duration::from_millis(50));
    }
    for process in &mut children {
        if matches!(process.child.try_wait(), Ok(None)) {
            // SAFETY: owned, unreaped child as above.
            unsafe {
                libc::kill(-(process.child.id() as i32), libc::SIGKILL);
            }
        }
    }
    drop(children);
    let cleanup = terminals.cleanup(account);
    for status in status.lock().unwrap().iter_mut() {
        status.pid = None;
    }
    result.and(cleanup)
}
fn logger<R: Read + AsRawFd + Send + 'static>(
    mut reader: R,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    gid: u32,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // Nonblocking reads allow shutdown even if an escaped descendant holds a pipe.
        unsafe {
            let flags = libc::fcntl(reader.as_raw_fd(), libc::F_GETFL);
            if flags < 0
                || libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                return;
            }
        }
        let mut file = log_file(&path, gid).ok();
        let mut length = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
        let mut buffer = [0u8; 8192];
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if length + n as u64 > 10 * 1024 * 1024 {
                        file.take();
                        for i in (1..=3).rev() {
                            let source = if i == 1 {
                                path.clone()
                            } else {
                                path.with_extension(format!("log.{}", i - 1))
                            };
                            let _ = fs::rename(source, path.with_extension(format!("log.{i}")));
                        }
                        file = log_file(&path, gid).ok();
                        length = 0;
                    }
                    if let Some(f) = &mut file {
                        if f.write_all(&buffer[..n]).is_err() {
                            file = None;
                        }
                    }
                    length += n as u64;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    })
}
fn log_file(path: &Path, gid: u32) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o640)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // SAFETY: file owns a valid FD and log is root-owned.
    if unsafe { libc::fchown(file.as_raw_fd(), 0, gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}
