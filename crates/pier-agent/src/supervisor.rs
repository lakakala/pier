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

pub struct Supervisor {
    stop: Arc<AtomicBool>,
    pub status: Arc<Mutex<AppStatus>>,
    worker: Option<JoinHandle<()>>,
}
impl Supervisor {
    pub fn start(
        installed: Installed,
        options: RuntimeOptions,
        observe: bool,
        terminals: Arc<crate::terminal::Manager>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(AppStatus {
            instance: installed.instance.clone(),
            id: installed.id.clone(),
            state: "starting".into(),
            ..AppStatus::default()
        }));
        let ready = Arc::new(AtomicBool::new(false));
        let (worker_stop, worker_status, worker_ready) =
            (stop.clone(), status.clone(), ready.clone());
        let worker = thread::spawn(move || {
            let mut delay = 1;
            loop {
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                worker_status.lock().unwrap().state = "starting".into();
                let started = Instant::now();
                let outcome = run_child(
                    &installed,
                    &options,
                    &worker_stop,
                    &worker_status,
                    &worker_ready,
                    &terminals,
                );
                if let Err(error) = &outcome {
                    tracing::warn!(instance=%installed.id, %error, "app process operation failed");
                }
                if let Ok(code) = outcome {
                    worker_status.lock().unwrap().exit_code = code;
                }
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                if observe && !worker_ready.load(Ordering::SeqCst) {
                    worker_status.lock().unwrap().state = "failed".into();
                    return;
                }
                {
                    let mut status = worker_status.lock().unwrap();
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
            let mut status = worker_status.lock().unwrap();
            status.state = "stopped".into();
            status.pid = None;
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
fn run_child(
    installed: &Installed,
    options: &RuntimeOptions,
    stop: &AtomicBool,
    status: &Mutex<AppStatus>,
    ready: &AtomicBool,
    terminals: &crate::terminal::Manager,
) -> Result<Option<i32>> {
    account::verify(&installed.account)?;
    // Remove orphaned descendants before each restart as well as after agent crashes.
    terminals.cleanup(&installed.account)?;
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
    status.lock().unwrap().pid = Some(child.id());
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
    let start = Instant::now();
    let result = loop {
        if stop.load(Ordering::SeqCst) {
            break terminate(&mut child, options.stop_timeout_seconds);
        }
        match child.try_wait() {
            Ok(Some(exit)) => break Ok(exit.code()),
            Ok(None) => (),
            Err(error) => {
                let _ = terminate(&mut child, 0);
                break Err(error.into());
            }
        }
        if start.elapsed() >= Duration::from_secs(options.startup_grace_seconds) {
            ready.store(true, Ordering::SeqCst);
            status.lock().unwrap().state = "running".into();
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = terminals.cleanup(&installed.account);
    output_stop.store(true, Ordering::SeqCst);
    let _ = out.join();
    let _ = err.join();
    status.lock().unwrap().pid = None;
    result
}
fn terminate(child: &mut Child, seconds: u64) -> Result<Option<i32>> {
    // The child is still owned and unreaped, so its PID/PGID cannot be reused.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code());
        }
        thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    Ok(child.wait()?.code())
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
