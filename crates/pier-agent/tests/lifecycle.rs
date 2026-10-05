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
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Process {
    child: Option<Child>,
    log: PathBuf,
}
impl Process {
    fn spawn(binary: &Path, config: &Path, log: PathBuf, args: &[&str]) -> Self {
        let output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let child = Command::new(binary)
            .args(args)
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

fn system_account(name: &str) -> Vec<String> {
    let result = Command::new("getent")
        .args(["passwd", name])
        .output()
        .unwrap();
    assert!(result.status.success(), "missing user {name}");
    String::from_utf8(result.stdout)
        .unwrap()
        .trim_end()
        .split(':')
        .map(str::to_owned)
        .collect()
}

fn no_system_user(name: &str) {
    assert_eq!(
        Command::new("getent")
            .args(["passwd", name])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}

fn sync_blueprint(api: &Api, repo: &Path, blueprint: &str) {
    fs::write(repo.join("blueprints/web/pier-blueprint.yml"), blueprint).unwrap();
    command(repo, "git", &["add", "."]);
    command(
        repo,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "update blueprint",
        ],
    );
    api.post("/v1/repository/sync", json!({}));
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
        self.bind_blueprint(agent, "blueprints/web", message, fail);
    }
    fn bind_blueprint(&self, agent: &str, blueprint: &str, message: &str, fail: &str) {
        let key = pier_protocol::hash(blueprint);
        let exists = self.get(&format!("/v1/agents/{agent}/bindings"))["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == key);
        let path = if exists {
            format!("/v1/agents/{agent}/bindings/{key}")
        } else {
            format!("/v1/agents/{agent}/bindings")
        };
        self.call(
            if exists {
                reqwest::Method::PUT
            } else {
                reqwest::Method::POST
            },
            &path,
            Some(json!({"blueprint":blueprint,"variables":{"MESSAGE":message,"FAIL_WORKER":fail}})),
        )
        .unwrap();
    }
    fn deploy(&self, agent: &str) -> String {
        self.deploy_blueprint(agent, "blueprints/web")
    }
    fn deploy_blueprint(&self, agent: &str, blueprint: &str) -> String {
        let commit = self.get("/v1/repository")["commit"]
            .as_str()
            .unwrap()
            .to_string();
        self.post(
            "/v1/deployments",
            json!({"agent_id":agent,"blueprint":blueprint,"commit":commit}),
        )["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn stop_blueprint(&self, agent: &str, blueprint: &str) -> String {
        self.post(
            "/v1/deployments",
            json!({"agent_id":agent,"blueprint":blueprint,"action":"stop"}),
        )["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn blueprint(&self, agent: &str, path: &str) -> Value {
        self.get(&format!("/v1/agents/{agent}"))["report"]["blueprints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["blueprint"] == path)
            .unwrap()
            .clone()
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

struct Terminal {
    socket: tokio_tungstenite::tungstenite::WebSocket<
        tokio_tungstenite::tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    >,
    pid: i32,
}
impl Terminal {
    fn open(api: &Api, agent: &str, instance: &str, user: &str, home: &Path) -> Self {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, connect};
        let ticket = api.post(
            &format!("/v1/agents/{agent}/apps/{instance}/terminals"),
            json!({"cols":80,"rows":24}),
        );
        let url = format!(
            "{}{}",
            api.base.replace("http://", "ws://"),
            ticket["websocket_url"].as_str().unwrap()
        );
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Cookie", api.cookie.parse().unwrap());
        request
            .headers_mut()
            .insert("Origin", "https://pier.example.test".parse().unwrap());
        let (mut socket, _) = connect(request).unwrap();
        if let tokio_tungstenite::tungstenite::stream::MaybeTlsStream::Plain(stream) =
            socket.get_mut()
        {
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
        }
        let ready: Value = loop {
            use tokio_tungstenite::tungstenite::Message;
            match socket.read().unwrap() {
                Message::Text(text) => break serde_json::from_str(&text).unwrap(),
                Message::Ping(_) | Message::Pong(_) => (),
                message => panic!("unexpected terminal handshake: {message:?}"),
            }
        };
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["user"], user);
        assert_eq!(ready["home"].as_str().unwrap(), home.to_str().unwrap());
        let mut terminal = Self { socket, pid: 0 };
        terminal.send("stty -echo; printf '\\n__READY__\\n'\n");
        terminal.until("\r\n__READY__\r\n");
        let output = terminal.command("printf '__PID__%s__' \"$$\"");
        terminal.pid = output
            .split("__PID__")
            .nth(1)
            .unwrap()
            .split("__")
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(alive(terminal.pid));
        terminal
    }
    fn send(&mut self, command: &str) {
        self.socket
            .send(tokio_tungstenite::tungstenite::Message::Binary(
                command.as_bytes().to_vec().into(),
            ))
            .unwrap();
    }
    fn until(&mut self, marker: &str) -> String {
        use tokio_tungstenite::tungstenite::Message;
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains(marker) {
            match self.socket.read().unwrap() {
                Message::Binary(bytes) => {
                    self.socket
                        .send(Message::Text(
                            json!({"type":"ack","bytes":bytes.len()}).to_string().into(),
                        ))
                        .unwrap();
                    output.extend_from_slice(&bytes);
                    assert!(output.len() < 1024 * 1024);
                }
                Message::Ping(_) | Message::Pong(_) => (),
                message => panic!("unexpected terminal event: {message:?}"),
            }
        }
        String::from_utf8(output).unwrap()
    }
    fn command(&mut self, command: &str) -> String {
        let marker = format!("__DONE_{}__", pier_protocol::new_id());
        self.send(&format!("{command}\nprintf '\\n{marker}\\n'\n"));
        self.until(&format!("\r\n{marker}\r\n"))
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
    let source_requests = Arc::new(AtomicUsize::new(0));
    let requests = source_requests.clone();
    let script = br##"#!/bin/sh
set -eu
[ "$FAIL" != yes ] || exit 42
printf '%s' "$MESSAGE" > "$PIER_DATA_DIR/$ROLE.value"
id -u > "$PIER_DATA_DIR/$ROLE.uid"
id -G > "$PIER_DATA_DIR/$ROLE.groups"
echo $$ > "$PIER_DATA_DIR/$ROLE.pid"
if [ "$MESSAGE" = one ]; then dd if=/dev/zero bs=1048576 count=11 2>/dev/null; fi
trap 'exit 0' TERM INT
while :; do echo running; sleep 1; done
"##;
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
            requests.fetch_add(1, Ordering::SeqCst);
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
    fs::write(repo.join("apps/demo/1/pier-pkg.yml"), format!("schema: 2\nname: demo+pkg\nversion: '{{{{ MESSAGE }}}}'\nvariables:\n  ROLE: {{}}\n  MESSAGE: {{}}\n  FAIL: {{default: 'no'}}\nsource:\n  type: binary\n  url: http://127.0.0.1:{source_port}/program\n  format: raw\nfiles:\n- from: download\n  to: bin/demo\n  executable: true\nservice:\n  command: [bin/demo]\n  env:\n    ROLE: '{{{{ ROLE }}}}'\n    MESSAGE: '{{{{ MESSAGE }}}}'\n    FAIL: '{{{{ FAIL }}}}'\n")).unwrap();
    let blueprint = "schema: 1\nname: Web.Site\nvariables:\n  MESSAGE: {}\n  FAIL_WORKER: {default: 'no'}\napps:\n- id: api\n  app: apps/demo/1\n  variables: {ROLE: api, MESSAGE: '{{ MESSAGE }}'}\n- id: worker\n  app: apps/demo/1\n  variables: {ROLE: worker, MESSAGE: '{{ MESSAGE }}', FAIL: '{{ FAIL_WORKER }}'}\n";
    fs::write(repo.join("blueprints/web/pier-blueprint.yml"), blueprint).unwrap();
    fs::create_dir_all(repo.join("blueprints/other")).unwrap();
    let other_blueprint = blueprint
        .split("- id: worker")
        .next()
        .unwrap()
        .replace("name: Web.Site", "name: Other.Site");
    fs::write(
        repo.join("blueprints/other/pier-blueprint.yml"),
        &other_blueprint,
    )
    .unwrap();
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
        &[],
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
        connection_mode: Default::default(),
        listen: None,
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
        &[],
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
    let mut agent = Process::spawn(&agent_bin, &agent_config, dir.join("agent.log"), &["run"]);
    api.running(agent_id, 0);
    assert!(
        !api.get(&format!("/v1/agents/{agent_id}"))
            .to_string()
            .contains(registration["token"].as_str().unwrap())
    );
    assert!(
        api.call(
            reqwest::Method::POST,
            &format!("/v1/agents/{agent_id}/bindings"),
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
    // Names are checked before source acquisition; package names do not name users.
    for invalid in ["bad+name", &"a".repeat(33)] {
        sync_blueprint(
            &api,
            &repo,
            &blueprint.replace("name: Web.Site", &format!("name: {invalid}")),
        );
        let failed = api.deploy(agent_id);
        let result = api.finished(&failed);
        assert_eq!(result["state"], "failed");
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("name must be a system username"),
            "{result}"
        );
        assert_eq!(source_requests.load(Ordering::SeqCst), 0);
    }
    command(dir, "groupadd", &["--system", "occupied-blueprint-group"]);
    let original_root = system_account("root");
    for (name, message) in [
        ("root", "belongs to another blueprint or system user"),
        ("occupied-blueprint-group", "group already exists"),
    ] {
        sync_blueprint(
            &api,
            &repo,
            &blueprint.replace("name: Web.Site", &format!("name: {name}")),
        );
        let result = api.finished(&api.deploy(agent_id));
        assert_eq!(result["state"], "failed");
        assert!(
            result["error"].as_str().unwrap().contains(message),
            "{result}"
        );
        no_system_user("Web.Site");
        assert_eq!(system_account("root"), original_root);
        assert_eq!(
            fs::read_dir(dir.join("agent/blueprints")).unwrap().count(),
            0
        );
    }
    sync_blueprint(&api, &repo, blueprint);
    let first = api.deploy(agent_id);
    assert_eq!(api.finished(&first)["state"], "succeeded");
    let first_plan: pier_protocol::DeploymentPlan =
        serde_json::from_value(api.get(&format!("/v1/deployments/{first}"))["plan"].clone())
            .unwrap();
    api.running(agent_id, 2);
    let web_id = pier_protocol::hash("blueprints/web");
    let other_id = pier_protocol::hash("blueprints/other");
    let data = dir.join("agent/blueprints").join(&web_id).join("data");
    let other_data = dir.join("agent/blueprints").join(&other_id).join("data");
    let account = system_account("Web.Site");
    assert_ne!(account[2], "0");
    assert!(account[6].ends_with("/nologin"));
    assert_eq!(Path::new(&account[5]), data);
    assert_eq!(account[4], format!("pier-agent={agent_id}/{web_id}"));
    let assert_account = || {
        assert_eq!(system_account("Web.Site"), account);
        for role in ["api", "worker"] {
            assert_eq!(
                fs::read_to_string(data.join(format!("{role}.uid")))
                    .unwrap()
                    .trim(),
                account[2]
            );
            assert_eq!(
                fs::read_to_string(data.join(format!("{role}.groups")))
                    .unwrap()
                    .trim(),
                account[3]
            );
            assert_eq!(
                fs::metadata(data.join(format!("{role}.value")))
                    .unwrap()
                    .uid()
                    .to_string(),
                account[2]
            );
        }
        let group = Command::new("getent")
            .args(["group", "Web.Site"])
            .output()
            .unwrap();
        assert!(group.status.success());
        assert_eq!(
            String::from_utf8(group.stdout)
                .unwrap()
                .split(':')
                .nth(2)
                .unwrap(),
            account[3]
        );
    };
    assert_account();
    no_system_user("demo+pkg");
    fs::write(data.join("persistent"), "keep shared data").unwrap();
    for role in ["api", "worker"] {
        let log = dir
            .join("agent/blueprints")
            .join(&web_id)
            .join("apps")
            .join(role)
            .join("logs/stdout.log.1");
        wait("separate rotated logs", 30, || log.exists().then_some(()));
    }
    // Same app id and package name on a second blueprint are independent.
    api.bind_blueprint(agent_id, "blueprints/other", "other", "no");
    let other_job = api.deploy_blueprint(agent_id, "blueprints/other");
    assert_eq!(api.finished(&other_job)["state"], "succeeded");
    api.running(agent_id, 3);
    let other_account = system_account("Other.Site");
    assert_ne!(other_account[2], account[2]);
    let other_pid = api.blueprint(agent_id, "blueprints/other")["apps"][0]["pid"]
        .as_i64()
        .unwrap() as i32;
    let other_instance = api.blueprint(agent_id, "blueprints/other")["apps"][0]["instance"]
        .as_str()
        .unwrap()
        .to_string();
    let mut other_terminal =
        Terminal::open(&api, agent_id, &other_instance, "Other.Site", &other_data);
    let assert_other = || {
        assert!(alive(other_pid));
        assert_eq!(
            api.blueprint(agent_id, "blueprints/other")["apps"][0]["pid"],
            other_pid
        );
        assert_eq!(system_account("Other.Site"), other_account);
        assert_eq!(
            fs::read_to_string(other_data.join("api.value")).unwrap(),
            "other"
        );
    };
    let denied = Command::new("runuser")
        .args(["-u", "Web.Site", "--", "cat"])
        .arg(other_data.join("api.value"))
        .output()
        .unwrap();
    assert!(!denied.status.success());
    // Reject same-name blueprints before acquiring sources, and refuse renaming.
    for (name, message) in [
        ("Other.Site", "blueprint"),
        ("Renamed.Site", "cannot change"),
    ] {
        let fetched = source_requests.load(Ordering::SeqCst);
        sync_blueprint(
            &api,
            &repo,
            &blueprint.replace("name: Web.Site", &format!("name: {name}")),
        );
        let result = api.finished(&api.deploy(agent_id));
        assert_eq!(result["state"], "failed");
        assert!(
            result["error"].as_str().unwrap().contains(message),
            "{result}"
        );
        assert_eq!(source_requests.load(Ordering::SeqCst), fetched);
        assert_other();
        assert_account();
    }
    no_system_user("Renamed.Site");
    sync_blueprint(&api, &repo, blueprint);
    let store = pier_protocol::store::Store::open(&dir.join("agent/agent.db")).unwrap();
    let saved: Value = store.get("blueprint_accounts", &web_id).unwrap().unwrap();
    for field in ["uid", "gid", "marker", "home"] {
        let mut changed = saved.clone();
        changed[field] = match field {
            "uid" | "gid" => json!(saved[field].as_u64().unwrap() + 100_000),
            _ => json!("changed"),
        };
        store.put("blueprint_accounts", &web_id, &changed).unwrap();
        let result = api.finished(&api.deploy(agent_id));
        store.put("blueprint_accounts", &web_id, &saved).unwrap();
        assert_eq!(result["state"], "failed", "{result}");
        assert!(
            result["error"].as_str().unwrap().contains("account"),
            "{result}"
        );
        assert_other();
        assert_account();
    }
    drop(store);
    // Version-directory changes reuse the blueprint account and shared data.
    fs::create_dir_all(repo.join("apps/demo/2")).unwrap();
    fs::copy(
        repo.join("apps/demo/1/pier-pkg.yml"),
        repo.join("apps/demo/2/pier-pkg.yml"),
    )
    .unwrap();
    let upgraded_blueprint = blueprint.replace("app: apps/demo/1", "app: apps/demo/2");
    sync_blueprint(&api, &repo, &upgraded_blueprint);
    api.bind(agent_id, "two", "no");
    let second = api.deploy(agent_id);
    assert_eq!(api.finished(&second)["state"], "succeeded");
    api.running(agent_id, 3);
    assert_account();
    assert_other();
    assert_eq!(fs::read_to_string(data.join("api.value")).unwrap(), "two");
    assert_eq!(
        fs::read_to_string(data.join("worker.value")).unwrap(),
        "two"
    );
    api.bind(agent_id, "bad", "yes");
    let failed = api.deploy(agent_id);
    assert_eq!(api.finished(&failed)["state"], "rolled_back");
    api.running(agent_id, 3);
    assert_eq!(
        api.blueprint(agent_id, "blueprints/web")["deployment_id"],
        second
    );
    assert_eq!(
        fs::read_to_string(data.join("worker.value")).unwrap(),
        "two"
    );
    assert_account();
    assert_other();
    source_failure.store(true, Ordering::SeqCst);
    api.bind(agent_id, "three", "no");
    assert_eq!(api.finished(&api.deploy(agent_id))["state"], "failed");
    source_failure.store(false, Ordering::SeqCst);
    assert_other();
    let web_instance = api.blueprint(agent_id, "blueprints/web")["apps"][0]["instance"]
        .as_str()
        .unwrap()
        .to_string();
    let mut web_terminal = Terminal::open(&api, agent_id, &web_instance, "Web.Site", &data);
    assert!(
        other_terminal
            .command("echo OTHER_ALIVE")
            .contains("OTHER_ALIVE")
    );
    // Exiting either child restarts the whole blueprint and cleans orphan services.
    for signal in [libc::SIGKILL, libc::SIGTERM] {
        let old = api.blueprint(agent_id, "blueprints/web")["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["pid"].as_i64().unwrap() as i32)
            .collect::<Vec<_>>();
        let mut orphan = Command::new("runuser")
            .args([
                "-u",
                "Web.Site",
                "--",
                "/bin/sh",
                "-c",
                "setsid sleep 300 & echo $!; wait",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(orphan.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let orphan_pid: i32 = line.trim().parse().unwrap();
        assert!(alive(orphan_pid));
        unsafe {
            libc::kill(old[1], signal);
        }
        wait("whole blueprint automatic restart", 30, || {
            let bp = api.blueprint(agent_id, "blueprints/web");
            (bp["state"] == "running"
                && bp["apps"].as_array().unwrap().iter().all(|a| {
                    a["pid"]
                        .as_i64()
                        .is_some_and(|p| !old.contains(&(p as i32)))
                        && a["restarts"].as_u64().unwrap() > 0
                }))
            .then_some(())
        });
        assert!(old.iter().all(|p| !alive(*p)));
        assert!(
            !alive(orphan_pid),
            "escaped service descendant was not cleaned"
        );
        orphan.wait().unwrap();
        assert!(alive(web_terminal.pid));
        assert!(
            web_terminal
                .command("echo SAME_BLUEPRINT_ALIVE")
                .contains("SAME_BLUEPRINT_ALIVE")
        );
        assert!(
            other_terminal
                .command("echo OTHER_ALIVE")
                .contains("OTHER_ALIVE")
        );
        assert_account();
        assert_other();
    }
    let binding_path = format!("/v1/agents/{agent_id}/bindings/{web_id}");
    assert!(
        api.call(reqwest::Method::DELETE, &binding_path, None)
            .is_err()
    );
    // Stop does not depend on a still-present Git recipe or catalog entry.
    fs::remove_file(repo.join("blueprints/web/pier-blueprint.yml")).unwrap();
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
            "remove definition",
        ],
    );
    api.post("/v1/repository/sync", json!({}));
    assert_eq!(
        api.finished(&api.stop_blueprint(agent_id, "blueprints/web"))["state"],
        "succeeded"
    );
    wait("stopped blueprint terminal closes", 10, || {
        (!alive(web_terminal.pid)).then_some(())
    });
    drop(web_terminal);
    assert!(alive(other_terminal.pid));
    assert!(
        other_terminal
            .command("echo OTHER_AFTER_STOP")
            .contains("OTHER_AFTER_STOP")
    );
    api.running(agent_id, 1);
    assert_eq!(
        api.blueprint(agent_id, "blueprints/web")["state"],
        "stopped"
    );
    assert_other();
    assert_account();
    api.call(reqwest::Method::DELETE, &binding_path, None)
        .unwrap();
    assert_eq!(
        api.get(&format!("/v1/agents/{agent_id}/bindings"))["bindings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    sync_blueprint(&api, &repo, blueprint);
    api.bind(agent_id, "four", "no");
    let readded = api.deploy(agent_id);
    assert_eq!(api.finished(&readded)["state"], "succeeded");
    api.running(agent_id, 3);
    assert_eq!(
        fs::read_to_string(data.join("persistent")).unwrap(),
        "keep shared data"
    );
    assert_account();
    assert_other();
    assert!(
        other_terminal
            .command("echo OTHER_AFTER_REBIND")
            .contains("OTHER_AFTER_REBIND")
    );
    drop(other_terminal);
    // Journal rollback only replaces the targeted blueprint's snapshot.
    api.bind(agent_id, "interrupted", "no");
    let interrupted = api.deploy(agent_id);
    wait("deployment activation", 30, || {
        (api.get(&format!("/v1/deployments/{interrupted}"))["state"] == "applying").then_some(())
    });
    agent.stop(libc::SIGKILL);
    agent = Process::spawn(&agent_bin, &agent_config, dir.join("agent.log"), &["run"]);
    assert_eq!(api.finished(&interrupted)["state"], "rolled_back");
    api.running(agent_id, 3);
    assert_eq!(
        api.blueprint(agent_id, "blueprints/web")["deployment_id"],
        readded
    );
    assert_eq!(
        api.blueprint(agent_id, "blueprints/other")["deployment_id"],
        other_job
    );
    assert_account();
    assert_eq!(system_account("Other.Site"), other_account);
    // Both blueprints recover from local state without a controller.
    controller.stop(libc::SIGTERM);
    agent.stop(libc::SIGTERM);
    fs::remove_file(data.join("api.value")).unwrap();
    fs::remove_file(other_data.join("api.value")).unwrap();
    agent = Process::spawn(&agent_bin, &agent_config, dir.join("agent.log"), &["run"]);
    wait("offline multi-blueprint recovery", 25, || {
        (fs::read_to_string(data.join("api.value")).ok().as_deref() == Some("four")
            && fs::read_to_string(other_data.join("api.value"))
                .ok()
                .as_deref()
                == Some("other"))
        .then_some(())
    });
    assert_account();
    agent.stop(libc::SIGTERM);
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let config = pier_agent::Config {
        connection_mode: Default::default(),
        listen: None,
        agent_id: agent_id.into(),
        token_file: dir.join("agent.token"),
        controller_tcp: format!("127.0.0.1:{tcp_port}"),
        state_dir: dir.join("agent"),
        heartbeat_seconds: 1,
        runtime: pier_agent::RuntimeOptions {
            startup_grace_seconds: 1,
            stop_timeout_seconds: 1,
        },
    };
    let local = pier_agent::Runtime::open(config.clone(), events).unwrap();
    assert_eq!(local.apply(first_plan.clone()).unwrap().state, "succeeded");
    let report = local.report().unwrap();
    assert_eq!(report.blueprints.len(), 2);
    assert_eq!(
        report
            .blueprints
            .iter()
            .find(|b| b.id == web_id)
            .unwrap()
            .deployment_id
            .as_deref(),
        Some(readded.as_str())
    );
    let mut changed = first_plan;
    changed.commit = "different".into();
    assert!(local.apply(changed).is_err());
    local.shutdown();
    drop(local);
    // Legacy accounts must not be silently adopted or their data re-owned.
    let store = pier_protocol::store::Store::open(&dir.join("agent/agent.db")).unwrap();
    store.put("accounts", "old-app", &saved).unwrap();
    drop(store);
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    assert!(
        pier_agent::Runtime::open(config, events)
            .err()
            .unwrap()
            .to_string()
            .contains("legacy per-app deployment")
    );
    assert_eq!(system_account("Web.Site"), account);
}
