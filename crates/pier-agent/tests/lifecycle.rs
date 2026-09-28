//! Run ONLY in a disposable root Linux container. Creates real system users.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Process {
    child: Option<Child>,
    log: PathBuf,
}
impl Process {
    fn spawn(binary: &Path, config: &Path, log: PathBuf) -> Self {
        let output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let child = Command::new(binary)
            .arg("--config")
            .arg(config)
            .env("RUST_LOG", "info")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            log,
        }
    }
    fn stop(&mut self, signal: i32) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, signal);
            }
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop(libc::SIGTERM);
        if thread::panicking() {
            eprintln!(
                "{}:\n{}",
                self.log.display(),
                fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
    }
}
fn command(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn alive(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
        !status
            .lines()
            .any(|line| line.starts_with("State:") && line.split_whitespace().nth(1) == Some("Z"))
    })
}
fn wait<T>(label: &str, seconds: u64, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out: {label}");
        thread::sleep(Duration::from_millis(100));
    }
}
struct Api {
    client: reqwest::blocking::Client,
    base: String,
    cookie: String,
    csrf: String,
}
impl Api {
    fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("Cookie", &self.cookie)
            .header("Origin", "https://pier.example.test")
            .header("X-CSRF-Token", &self.csrf);
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().map_err(|e| e.to_string())?;
        let status = response.status();
        let text = response.text().map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{status}: {text}"));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }
    fn get(&self, path: &str) -> Value {
        self.call(reqwest::Method::GET, path, None).unwrap()
    }
    fn post(&self, path: &str, body: Value) -> Value {
        self.call(reqwest::Method::POST, path, Some(body)).unwrap()
    }
    fn bind(&self, agent: &str, message: &str, fail: &str) {
        self.call(reqwest::Method::PUT, &format!("/v1/agents/{agent}/binding"), Some(json!({"blueprint":"blueprints/web","variables":{"MESSAGE":message,"FAIL_WORKER":fail}}))).unwrap();
    }
    fn deploy(&self, agent: &str) -> String {
        let commit = self.get("/v1/repository")["commit"]
            .as_str()
            .unwrap()
            .to_string();
        self.post("/v1/deployments", json!({"agent_id":agent,"commit":commit}))["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn finished(&self, job: &str) -> Value {
        wait("deployment result", 90, || {
            let value = self.get(&format!("/v1/deployments/{job}"));
            matches!(
                value["state"].as_str(),
                Some("succeeded" | "failed" | "rolled_back" | "rollback_failed")
            )
            .then_some(value)
        })
    }
    fn running(&self, agent: &str, count: usize) -> Value {
        wait("running app reports", 30, || {
            let value = self.get(&format!("/v1/agents/{agent}"));
            let apps = value["report"]["apps"].as_array()?;
            (value["online"] == true
                && apps.len() == count
                && apps.iter().all(|a| a["state"] == "running"))
            .then_some(value)
        })
    }
}

#[test]
#[ignore = "requires a disposable root Docker container, git and useradd"]
fn real_controller_agent_lifecycle() {
    assert!(
        Path::new("/.dockerenv").exists()
            && std::env::var("PIER_PRIVILEGED_TESTS").as_deref() == Ok("1"),
        "refusing to create users outside an explicitly enabled disposable container"
    );
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let dir = root.path();
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let source_port = source.local_addr().unwrap().port();
    let source_failure = Arc::new(AtomicBool::new(false));
    let failure = source_failure.clone();
    let script = b"#!/bin/sh\nset -eu\n[ \"$FAIL\" != yes ] || exit 42\nprintf '%s' \"$MESSAGE\" > \"$PIER_DATA_DIR/value\"\nid -u > \"$PIER_DATA_DIR/uid\"\nid -G > \"$PIER_DATA_DIR/groups\"\nif [ \"$MESSAGE\" = one ]; then dd if=/dev/zero bs=1048576 count=11 2>/dev/null; fi\ntrap 'exit 0' TERM INT\nwhile :; do echo running; sleep 1; done\n";
    thread::spawn(move || {
        for incoming in source.incoming() {
            let Ok(mut stream) = incoming else {
                break;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0u8; 8192];
            let _ = stream.read(&mut request);
            if failure.load(Ordering::SeqCst) {
                let _ = stream.write_all(
                    b"HTTP/1.1 500 Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            } else {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    script.len()
                );
                let _ = stream.write_all(script);
            }
        }
    });
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join("apps/demo/1")).unwrap();
    fs::create_dir_all(repo.join("blueprints/web")).unwrap();
    fs::write(repo.join("apps/demo/1/pier-pkg.yml"), format!("schema: 2\nname: demo\nversion: '{{{{ MESSAGE }}}}'\nvariables:\n  MESSAGE: {{}}\n  FAIL: {{default: 'no'}}\nsource:\n  type: binary\n  url: http://127.0.0.1:{source_port}/program\n  format: raw\nfiles:\n- from: download\n  to: bin/demo\n  executable: true\nservice:\n  command: [bin/demo]\n  env:\n    MESSAGE: '{{{{ MESSAGE }}}}'\n    FAIL: '{{{{ FAIL }}}}'\n")).unwrap();
    let blueprint = "schema: 1\nname: web\nvariables:\n  MESSAGE: {}\n  FAIL_WORKER: {default: 'no'}\napps:\n- id: api\n  app: apps/demo/1\n  variables: {MESSAGE: '{{ MESSAGE }}'}\n- id: worker\n  app: apps/demo/1\n  variables: {MESSAGE: '{{ MESSAGE }}', FAIL: '{{ FAIL_WORKER }}'}\n";
    fs::write(repo.join("blueprints/web/pier-blueprint.yml"), blueprint).unwrap();
    command(&repo, "git", &["init", "-q", "-b", "main"]);
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
    );
    let http_port = free_port();
    let tcp_port = free_port();
    let controller_config = dir.join("controller.yml");
    fs::write(&controller_config, format!("state_dir: controller\nrepository:\n  url: {}\n  reference: main\n  sync_interval_seconds: 3600\nhttp_listen: 127.0.0.1:{http_port}\ntcp_listen: 127.0.0.1:{tcp_port}\npublic_url: https://pier.example.test\nagent_endpoint: 127.0.0.1:{tcp_port}\n", repo.display())).unwrap();
    let controller_bin =
        PathBuf::from(std::env::var_os("PIER_CONTROLLER_BIN").expect("set PIER_CONTROLLER_BIN"));
    let agent_bin = PathBuf::from(std::env::var_os("PIER_AGENT_BIN").expect("set PIER_AGENT_BIN"));
    let mut controller = Process::spawn(
        &controller_bin,
        &controller_config,
        dir.join("controller.log"),
    );
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let response = wait("controller initialization", 30, || {
        client
            .post(format!("http://localhost:{http_port}/v1/auth/init"))
            .header("Origin", "https://pier.example.test")
            .header("Content-Type", "application/json")
            .body(json!({"username":"admin","password":"lifecycle-password-123"}).to_string())
            .send()
            .ok()
    });
    assert!(response.status().is_success());
    // The fixture talks directly to loopback HTTP behind the simulated proxy.
    // Production/browser clients receive this Secure cookie over HTTPS.
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let session: Value = serde_json::from_str(&response.text().unwrap()).unwrap();
    let api = Api {
        client,
        base: format!("http://localhost:{http_port}"),
        cookie,
        csrf: session["csrf_token"].as_str().unwrap().into(),
    };
    api.post("/v1/repository/sync", json!({}));
    wait("controller catalog", 30, || {
        api.call(reqwest::Method::GET, "/v1/repository", None)
            .ok()
            .filter(|v| v["commit"].is_string())
    });
    assert_eq!(
        api.client
            .get(format!("{}/v1/agents", api.base))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let request = pier_protocol::enrollment::InitRequest {
        request_id: pier_protocol::new_token(),
        name: "fixture".into(),
        public_url: "https://pier.example.test".into(),
        info: pier_protocol::AgentInfo {
            architecture: pier_agent::architecture().unwrap(),
            hostname: "test".into(),
            os_release: "fixture".into(),
        },
    };
    let grant = api.post("/v1/enrollments", serde_json::to_value(&request).unwrap());
    let pairing =
        pier_protocol::enrollment::Pairing::decode(grant["pairing"].as_str().unwrap(), &request)
            .unwrap();
    // Authorization and credential exchange survive a controller restart.
    controller.stop(libc::SIGTERM);
    controller = Process::spawn(
        &controller_bin,
        &controller_config,
        dir.join("controller.log"),
    );
    wait("controller restarted", 30, || {
        api.call(reqwest::Method::GET, "/v1/agents", None).ok()
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let credentials = runtime
        .block_on(pier_agent::init::redeem(&request, &pairing))
        .unwrap();
    let retry = runtime
        .block_on(pier_agent::init::redeem(&request, &pairing))
        .unwrap();
    assert_eq!(credentials.agent_id, retry.agent_id);
    assert_eq!(credentials.token, retry.token);
    runtime
        .block_on(pier_agent::init::acknowledge(
            &request.request_id,
            &credentials,
        ))
        .unwrap();
    runtime
        .block_on(pier_agent::init::acknowledge(
            &request.request_id,
            &credentials,
        ))
        .unwrap();
    assert!(
        runtime
            .block_on(pier_agent::init::redeem(&request, &pairing))
            .is_err()
    );
    let registration = json!({"id":credentials.agent_id,"token":credentials.token});
    let agent_id = registration["id"].as_str().unwrap();
    runtime.block_on(async {
        use pier_protocol::secure::{Purpose, connect, token_key};
        let endpoint = format!("127.0.0.1:{tcp_port}");
        assert!(
            connect(
                &endpoint,
                Purpose::Control,
                agent_id,
                &token_key("wrong-token")
            )
            .await
            .is_err()
        );
        let io = connect(
            &endpoint,
            Purpose::Control,
            agent_id,
            &token_key(registration["token"].as_str().unwrap()),
        )
        .await
        .unwrap();
        let mut stream = pier_protocol::framed(io);
        pier_protocol::send(
            &mut stream,
            &pier_protocol::Message::Hello {
                version: 999,
                agent_id: agent_id.into(),
                info: request.info.clone(),
                software: None,
            },
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), pier_protocol::receive(&mut stream))
                .await
                .unwrap()
                .is_err()
        );
    });
    fs::write(
        dir.join("agent.token"),
        registration["token"].as_str().unwrap(),
    )
    .unwrap();
    let agent_config = dir.join("agent.yml");
    fs::write(&agent_config, format!("agent_id: {agent_id}\ntoken_file: agent.token\ncontroller_tcp: 127.0.0.1:{tcp_port}\nstate_dir: agent\nheartbeat_seconds: 1\nruntime:\n  startup_grace_seconds: 1\n  stop_timeout_seconds: 1\n")).unwrap();
    let mut agent = Process::spawn(&agent_bin, &agent_config, dir.join("agent.log"));
    api.running(agent_id, 0);
    assert!(
        !api.get(&format!("/v1/agents/{agent_id}"))
            .to_string()
            .contains(registration["token"].as_str().unwrap())
    );
    assert!(
        api.call(
            reqwest::Method::PUT,
            &format!("/v1/agents/{agent_id}/binding"),
            Some(json!({"blueprint":"blueprints/web","variables":{}}))
        )
        .is_err()
    );
    api.bind(agent_id, "one", "no");
    assert!(
        api.get(&format!("/v1/agents/{agent_id}"))["report"]["apps"]
            .as_array()
            .unwrap()
            .is_empty(),
        "binding must not deploy"
    );
    let first = api.deploy(agent_id);
    assert_eq!(api.finished(&first)["state"], "succeeded");
    let first_plan: pier_protocol::DeploymentPlan =
        serde_json::from_value(api.get(&format!("/v1/deployments/{first}"))["plan"].clone())
            .unwrap();
    let running = api.running(agent_id, 2);
    let apps = running["report"]["apps"].as_array().unwrap();
    let paths: Vec<_> = apps
        .iter()
        .map(|a| {
            dir.join("agent/apps")
                .join(a["instance"].as_str().unwrap())
                .join("data")
        })
        .collect();
    let uids: Vec<_> = paths
        .iter()
        .map(|p| {
            fs::read_to_string(p.join("uid"))
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap()
        })
        .collect();
    assert_ne!(uids[0], 0);
    assert_ne!(uids[0], uids[1]);
    let account = Command::new("getent")
        .args(["passwd", &uids[0].to_string()])
        .output()
        .unwrap();
    let name = String::from_utf8(account.stdout)
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .to_string();
    let denied = Command::new("runuser")
        .args(["-u", &name, "--", "cat"])
        .arg(paths[1].join("value"))
        .output()
        .unwrap();
    assert!(
        !denied.status.success(),
        "app user must not read another app's data"
    );
    for (path, uid) in paths.iter().zip(&uids) {
        assert_eq!(fs::metadata(path.join("value")).unwrap().uid(), *uid);
        assert_eq!(
            fs::read_to_string(path.join("groups"))
                .unwrap()
                .split_whitespace()
                .count(),
            1
        );
        assert_eq!(fs::read_to_string(path.join("value")).unwrap(), "one");
        fs::write(path.join("persistent"), "keep me").unwrap();
        let log_dir = path.parent().unwrap().join("logs");
        wait("log rotation", 30, || {
            log_dir.join("stdout.log.1").exists().then_some(())
        });
        assert!(fs::metadata(log_dir.join("stdout.log")).unwrap().len() <= 10 * 1024 * 1024);
    }
    let killed_pid = apps[0]["pid"].as_i64().unwrap() as i32;
    unsafe {
        libc::kill(killed_pid, libc::SIGKILL);
    }
    wait("automatic app restart", 30, || {
        let value = api.get(&format!("/v1/agents/{agent_id}"));
        let app = &value["report"]["apps"][0];
        (app["state"] == "running"
            && app["pid"].as_i64().is_some_and(|p| p != killed_pid as i64)
            && app["restarts"].as_u64().unwrap_or(0) > 0)
            .then_some(())
    });
    api.bind(agent_id, "two", "no");
    assert_eq!(fs::read_to_string(paths[0].join("value")).unwrap(), "one");
    let second = api.deploy(agent_id);
    assert_eq!(api.finished(&second)["state"], "succeeded");
    api.running(agent_id, 2);
    api.bind(agent_id, "bad", "yes");
    let failed = api.deploy(agent_id);
    assert_eq!(api.finished(&failed)["state"], "rolled_back");
    api.running(agent_id, 2);
    for path in &paths {
        assert_eq!(fs::read_to_string(path.join("value")).unwrap(), "two");
        assert_eq!(
            fs::read_to_string(path.join("persistent")).unwrap(),
            "keep me"
        );
    }
    source_failure.store(true, Ordering::SeqCst);
    api.bind(agent_id, "three", "no");
    let failed_build = api.deploy(agent_id);
    assert_eq!(api.finished(&failed_build)["state"], "failed");
    source_failure.store(false, Ordering::SeqCst);
    assert_eq!(fs::read_to_string(paths[0].join("value")).unwrap(), "two");
    // Invalid Git snapshots must leave the usable catalog and services intact.
    let previous = api.get("/v1/repository")["commit"].clone();
    fs::write(
        repo.join("blueprints/web/pier-blueprint.yml"),
        "schema: 999\nname: bad\napps: []\n",
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "invalid",
        ],
    );
    assert!(
        api.call(
            reqwest::Method::POST,
            "/v1/repository/sync",
            Some(json!({}))
        )
        .is_err()
    );
    assert_eq!(api.get("/v1/repository")["commit"], previous);
    // Controller downtime must not stop apps; it can restart without redeploying.
    let before = api.running(agent_id, 2);
    let survivor = before["report"]["apps"][0]["pid"].as_i64().unwrap() as i32;
    controller.stop(libc::SIGTERM);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(unsafe { libc::kill(survivor, 0) }, 0);
    controller = Process::spawn(
        &controller_bin,
        &controller_config,
        dir.join("controller.log"),
    );
    wait("controller restart", 30, || {
        api.call(reqwest::Method::GET, "/v1/repository", None).ok()
    });
    let restored = api.running(agent_id, 2);
    assert_eq!(restored["report"]["deployment_id"], second);
    // Abrupt agent death leaves processes until recovery; recovery replaces them.
    let old_pid = restored["report"]["apps"][0]["pid"].as_i64().unwrap() as i32;
    agent.stop(libc::SIGKILL);
    agent = Process::spawn(&agent_bin, &agent_config, dir.join("agent.log"));
    wait("agent crash recovery", 40, || {
        let value = api.get(&format!("/v1/agents/{agent_id}"));
        (value["online"] == true
            && value["report"]["apps"].as_array().is_some_and(|apps| {
                apps.len() == 2 && apps.iter().all(|a| a["state"] == "running")
            })
            && value["report"]["apps"][0]["pid"]
                .as_i64()
                .is_some_and(|p| p != old_pid as i64))
        .then_some(())
    });
    assert!(!alive(old_pid));
    // Remove an app, preserve its user/data, then interrupt a later deployment.
    let one_app = blueprint.split("- id: worker").next().unwrap();
    fs::write(repo.join("blueprints/web/pier-blueprint.yml"), one_app).unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "remove worker",
        ],
    );
    api.post("/v1/repository/sync", json!({}));
    api.bind(agent_id, "four", "no");
    let removed = api.deploy(agent_id);
    assert_eq!(api.finished(&removed)["state"], "succeeded");
    api.running(agent_id, 1);
    assert!(paths.iter().all(|p| p.join("persistent").exists()));
    api.bind(agent_id, "interrupted", "no");
    let interrupted = api.deploy(agent_id);
    wait("deployment reaches activation", 30, || {
        let job = api.get(&format!("/v1/deployments/{interrupted}"));
        (job["state"] == "applying").then_some(())
    });
    agent.stop(libc::SIGKILL);
    agent = Process::spawn(&agent_bin, &agent_config, dir.join("agent.log"));
    assert_eq!(api.finished(&interrupted)["state"], "rolled_back");
    let final_state = api.running(agent_id, 1);
    assert_eq!(final_state["report"]["deployment_id"], removed);
    // Graceful stop must stop apps and restart from local state without controller.
    let final_pid = final_state["report"]["apps"][0]["pid"].as_i64().unwrap() as i32;
    controller.stop(libc::SIGTERM);
    agent.stop(libc::SIGTERM);
    assert!(!alive(final_pid));
    let active_instance = final_state["report"]["apps"][0]["instance"]
        .as_str()
        .unwrap();
    let value = dir
        .join("agent/apps")
        .join(active_instance)
        .join("data/value");
    fs::remove_file(&value).unwrap();
    agent = Process::spawn(&agent_bin, &agent_config, dir.join("agent.log"));
    wait("offline local recovery", 20, || {
        fs::read_to_string(&value).ok().filter(|v| v == "four")
    });
    agent.stop(libc::SIGTERM);
    // Replaying a completed plan must neither download nor replace the current
    // snapshot, and reusing its ID with different content must be rejected.
    source_failure.store(true, Ordering::SeqCst);
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let local = pier_agent::Runtime::open(
        pier_agent::Config {
            agent_id: agent_id.into(),
            token_file: dir.join("agent.token"),
            controller_tcp: format!("127.0.0.1:{tcp_port}"),
            state_dir: dir.join("agent"),
            heartbeat_seconds: 1,
            runtime: pier_agent::RuntimeOptions {
                startup_grace_seconds: 1,
                stop_timeout_seconds: 1,
            },
        },
        events,
    )
    .unwrap();
    assert_eq!(local.apply(first_plan.clone()).unwrap().state, "succeeded");
    assert_eq!(
        local.report().unwrap().deployment_id.as_deref(),
        Some(removed.as_str())
    );
    let mut changed = first_plan;
    changed.commit = "different".into();
    assert!(local.apply(changed).is_err());
    local.shutdown();
}
