//! Interactive enrollment and reusable enrollment/persistence APIs.
use crate::{Config, RuntimeOptions};
use anyhow::{Context, Result, ensure};
use dialoguer::{Input, Select};
use pier_protocol::{
    Message,
    connection::ConnectionMode,
    enrollment::{Credentials, InitRequest, Pairing},
    secure::{self, Purpose},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{IsTerminal, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

pub const DEFAULT_CONFIG: &str = "/etc/pier/agent.yml";
const PENDING: &str = "/etc/pier/agent.init.json";
const TOKEN: &str = "/etc/pier/agent.token";

#[derive(Serialize, Deserialize)]
struct PendingInit {
    request: InitRequest,
    state_dir: PathBuf,
    pairing: Option<String>,
    credentials: Option<Credentials>,
}

pub fn load_config(path: &Path) -> Result<Config> {
    let path = path
        .canonicalize()
        .context("找不到 agent 配置，请先运行 sudo pier-agent init")?;
    let mut config: Config = serde_yaml_ng::from_slice(&fs::read(&path)?).context("无效的 v2 配置；请移除旧 ca_cert、controller_https、controller_server_name 字段，参见 docs/services.md")?;
    let base = path.parent().unwrap();
    for value in [&mut config.state_dir, &mut config.token_file] {
        if value.is_relative() {
            *value = base.join(&*value);
        }
    }
    Ok(config)
}

pub async fn redeem(request: &InitRequest, pairing: &Pairing) -> Result<Credentials> {
    let pairing = Pairing::decode(&pairing.encode()?, request)?;
    ensure!(
        request.connection_mode == ConnectionMode::AgentToController,
        "passive enrollment requires listener"
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        let io = secure::connect(
            &pairing.endpoint,
            Purpose::Enrollment,
            &pairing.grant_id,
            &secure::decode_key(&pairing.secret)?,
        )
        .await?;
        let mut stream = pier_protocol::framed(io);
        pier_protocol::send(
            &mut stream,
            &Message::Enroll {
                request: request.clone(),
            },
        )
        .await?;
        let Message::Enrolled { credentials } = pier_protocol::receive(&mut stream).await? else {
            anyhow::bail!("invalid enrollment response");
        };
        ensure!(
            pier_protocol::safe_id(&credentials.agent_id)
                && credentials.token.len() == 64
                && credentials.controller_tcp == pairing.endpoint
                && credentials.connection_mode == request.connection_mode
                && credentials.listen == request.listen,
            "invalid credentials response"
        );
        secure::decode_key(&credentials.token)?;
        Ok(credentials)
    })
    .await?
}
pub async fn acknowledge(request_id: &str, credentials: &Credentials) -> Result<()> {
    ensure!(
        credentials.connection_mode == ConnectionMode::AgentToController,
        "passive enrollment is acknowledged on control connection"
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        let io = secure::connect(
            &credentials.controller_tcp,
            Purpose::EnrollmentAck,
            &credentials.agent_id,
            &secure::token_key(&credentials.token),
        )
        .await?;
        let mut stream = pier_protocol::framed(io);
        pier_protocol::send(
            &mut stream,
            &Message::EnrollmentAck {
                request_id: request_id.into(),
            },
        )
        .await?;
        ensure!(
            matches!(pier_protocol::receive(&mut stream).await?, Message::Acked),
            "invalid acknowledgement"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// The caller keeps this listener reserved throughout browser authorization.
pub async fn redeem_incoming(
    request: &InitRequest,
    pairing: &Pairing,
    listener: &tokio::net::TcpListener,
) -> Result<Credentials> {
    let pairing = Pairing::decode(&pairing.encode()?, request)?;
    ensure!(
        request.connection_mode == ConnectionMode::ControllerToAgent,
        "passive enrollment required"
    );
    let key = secure::decode_key(&pairing.secret)?;
    tokio::time::timeout(
        Duration::from_secs(pairing.expires_at.saturating_sub(pier_protocol::now())),
        async {
            loop {
                let (mut socket, _) = listener.accept().await?;
                let result = tokio::time::timeout(Duration::from_secs(10), async {
                    let (prelude, raw) = secure::read_prelude(&mut socket).await?;
                    ensure!(
                        prelude.purpose == Purpose::Enrollment && prelude.id == pairing.grant_id,
                        "unrelated pairing connection"
                    );
                    let mut wire = pier_protocol::framed(secure::accept(socket, &raw, &key).await?);
                    pier_protocol::send(
                        &mut wire,
                        &Message::Enroll {
                            request: request.clone(),
                        },
                    )
                    .await?;
                    let Message::Enrolled { credentials } =
                        pier_protocol::receive(&mut wire).await?
                    else {
                        anyhow::bail!("credentials required");
                    };
                    ensure!(
                        credentials.connection_mode == request.connection_mode
                            && credentials.listen == request.listen
                            && credentials.controller_tcp.is_empty()
                            && pier_protocol::safe_id(&credentials.agent_id),
                        "invalid passive credentials"
                    );
                    secure::decode_key(&credentials.token)?;
                    Ok::<_, anyhow::Error>(credentials)
                })
                .await;
                if let Ok(Ok(credentials)) = result {
                    return Ok(credentials);
                }
            }
        },
    )
    .await?
}

fn directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions.
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "配置目录必须由当前用户拥有，且不能由组或其他用户写入"
    );
    Ok(())
}
fn secret_read(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 65536,
        "不安全的初始化文件权限或大小"
    );
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}
fn secret_write(path: &Path, data: &[u8], replace: bool) -> Result<()> {
    let parent = path.parent().context("文件路径缺少目录")?;
    directory(parent)?;
    if fs::symlink_metadata(path).is_ok() {
        let old = secret_read(path)?;
        if old == data {
            return Ok(());
        }
        ensure!(replace, "已有配置或 token 与本次身份不一致，拒绝覆盖");
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.write_all(data)?;
    temporary.as_file().sync_all()?;
    if replace {
        temporary.persist(path)?;
    } else {
        temporary.persist_noclobber(path)?;
    }
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Commit configuration last; retries never overwrite another agent identity.
pub fn save_config(
    config_path: &Path,
    token_path: &Path,
    state_dir: &Path,
    credentials: &Credentials,
) -> Result<Config> {
    ensure!(
        config_path.is_absolute() && token_path.is_absolute() && state_dir.is_absolute(),
        "配置、token 和数据目录必须使用绝对路径"
    );
    ensure!(
        pier_protocol::safe_id(&credentials.agent_id) && credentials.token.len() >= 32,
        "invalid agent credentials"
    );
    match credentials.connection_mode {
        ConnectionMode::AgentToController => {
            pier_protocol::enrollment::endpoint(&credentials.controller_tcp)?;
            ensure!(credentials.listen.is_none(), "unexpected listener");
        }
        ConnectionMode::ControllerToAgent => ensure!(
            credentials.controller_tcp.is_empty()
                && credentials.listen.is_some_and(|a| a.port() > 0),
            "invalid passive credentials"
        ),
    }
    if state_dir.join("agent.db").exists() {
        let store = pier_protocol::store::Store::open(&state_dir.join("agent.db"))?;
        let identity: Option<String> = store.get("identity", "agent_id")?;
        ensure!(
            identity.as_deref() == Some(&credentials.agent_id),
            "数据目录属于其他 agent，拒绝更换身份"
        );
    }
    let config = Config {
        connection_mode: credentials.connection_mode,
        listen: credentials.listen,
        agent_id: credentials.agent_id.clone(),
        token_file: token_path.into(),
        controller_tcp: credentials.controller_tcp.clone(),
        state_dir: state_dir.into(),
        heartbeat_seconds: 15,
        runtime: RuntimeOptions::default(),
    };
    let yaml = serde_yaml_ng::to_string(&config)?;
    if fs::symlink_metadata(config_path).is_ok() {
        ensure!(
            secret_read(config_path)? == yaml.as_bytes(),
            "已有配置不同，拒绝覆盖"
        );
    }
    secret_write(
        token_path,
        format!("{}\n", credentials.token).as_bytes(),
        false,
    )?;
    secret_write(config_path, yaml.as_bytes(), false)?;
    Ok(config)
}
// Keep echo disabled for the whole paste, and treat Ctrl+C as a key so the
// terminal guard is dropped before returning. read_secure_line uses SIGINT and
// can otherwise leave echo disabled when the process exits during a password.
fn pairing_input() -> Result<String> {
    use dialoguer::console::{Key, Term};
    struct Restore(libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.0);
            }
        }
    }
    let mut original = std::mem::MaybeUninit::uninit();
    ensure!(
        unsafe { libc::tcgetattr(0, original.as_mut_ptr()) } == 0,
        "无法读取终端设置"
    );
    let original = unsafe { original.assume_init() };
    let _restore = Restore(original);
    let mut hidden = original;
    hidden.c_lflag &= !(libc::ECHO | libc::ISIG);
    ensure!(
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &hidden) } == 0,
        "无法设置安全输入"
    );
    let term = Term::stdout();
    term.write_str("粘贴网页提供的配对凭据（输入不回显）: ")?;
    let mut value = String::new();
    loop {
        match term.read_key_raw()? {
            Key::Enter if !value.is_empty() => {
                term.write_line("")?;
                return Ok(value);
            }
            Key::CtrlC | Key::Escape => {
                term.write_line("\n已取消，初始化进度已保存。")?;
                anyhow::bail!("初始化已取消");
            }
            Key::Backspace => {
                value.pop();
            }
            Key::Char(c) if c.is_ascii() && !c.is_control() && value.len() < 16384 => value.push(c),
            _ => (),
        }
    }
}
fn select(prompt: &str, choices: &[&str]) -> Result<Option<usize>> {
    if std::env::var("TERM").is_ok_and(|value| value == "dumb") {
        println!("{prompt}");
        for (i, choice) in choices.iter().enumerate() {
            println!("  {}. {}", i + 1, choice);
        }
        loop {
            let value: String = Input::new()
                .with_prompt("输入编号（回车选择 1，q 取消）")
                .allow_empty(true)
                .interact_text()?;
            if value == "q" {
                return Ok(None);
            }
            if value.is_empty() {
                return Ok(Some(0));
            }
            if let Ok(index) = value.parse::<usize>() {
                if index > 0 && index <= choices.len() {
                    return Ok(Some(index - 1));
                }
            }
            println!("请输入有效编号。");
        }
    }
    Ok(Select::new()
        .with_prompt(prompt)
        .items(choices)
        .default(0)
        .interact_opt()?)
}
fn initial_request() -> Result<Option<PendingInit>> {
    let info = crate::network::host_info()?;
    loop {
        let public_url: String = Input::new()
            .with_prompt("Controller 网页地址，例如 http://pier.example.com:8080（也支持 HTTPS）")
            .validate_with(|s: &String| {
                pier_protocol::enrollment::origin(s)
                    .map(|_| ())
                    .map_err(|_| "请输入 HTTP 或 HTTPS 地址，不含路径、账号或查询参数")
            })
            .interact_text()?;
        let Some(mode) = select(
            "连接方式",
            &[
                "Agent 主动连接 Controller",
                "Controller 主动连接 Agent",
                "取消",
            ],
        )?
        else {
            return Ok(None);
        };
        if mode == 2 {
            return Ok(None);
        }
        let connection_mode = if mode == 0 {
            ConnectionMode::AgentToController
        } else {
            ConnectionMode::ControllerToAgent
        };
        let listen = if mode == 1 {
            Some(
                Input::<String>::new()
                    .with_prompt("Agent 本机监听地址（可达地址稍后在网页填写）")
                    .default("0.0.0.0:7444".into())
                    .validate_with(|value: &String| -> std::result::Result<(), &str> {
                        if value
                            .parse::<std::net::SocketAddr>()
                            .is_ok_and(|a| a.port() > 0)
                        {
                            Ok(())
                        } else {
                            Err("请输入 IP:端口，端口须为 1–65535")
                        }
                    })
                    .interact_text()?
                    .parse()?,
            )
        } else {
            None
        };
        let name: String = Input::new()
            .with_prompt("Agent 名称")
            .default(info.hostname.clone())
            .validate_with(|s: &String| {
                if !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control) {
                    Ok(())
                } else {
                    Err("名称须为 1–256 字节，不能包含控制字符")
                }
            })
            .interact_text()?;
        let Some(choice) = select(
            "数据目录",
            &["默认 /var/lib/pier-agent", "自定义目录", "取消"],
        )?
        else {
            return Ok(None);
        };
        let state_dir = match choice {
            0 => PathBuf::from("/var/lib/pier-agent"),
            1 => PathBuf::from(
                Input::<String>::new()
                    .with_prompt("数据目录（绝对路径）")
                    .validate_with(|s: &String| {
                        if Path::new(s).is_absolute() && !s.chars().any(char::is_control) {
                            Ok(())
                        } else {
                            Err("请输入绝对路径")
                        }
                    })
                    .interact_text()?,
            ),
            _ => return Ok(None),
        };
        ensure!(
            !state_dir.join("agent.db").exists(),
            "数据目录中已有 agent 身份，请恢复原配置或选择新的空目录"
        );
        directory(&state_dir)?;
        let _probe = tempfile::NamedTempFile::new_in(&state_dir)?;
        let request = InitRequest {
            connection_mode,
            listen,
            request_id: pier_protocol::new_token(),
            name,
            public_url: pier_protocol::enrollment::origin(&public_url)?,
            info: info.clone(),
        };
        request.validate()?;
        println!(
            "\nController: {}\nAgent: {}\n配置: {}\n数据: {}",
            request.public_url,
            request.name,
            DEFAULT_CONFIG,
            state_dir.display()
        );
        println!(
            "连接方式: {}",
            if request.connection_mode == ConnectionMode::ControllerToAgent {
                "Controller 主动连接 Agent"
            } else {
                "Agent 主动连接 Controller"
            }
        );
        if let Some(address) = request.listen {
            println!("本机监听: {address}；请允许 Controller 访问此端口。");
        }
        match select(
            "确认初始化信息",
            &["继续授权并启用系统服务", "修改信息", "取消"],
        )? {
            Some(0) => {
                return Ok(Some(PendingInit {
                    request,
                    state_dir,
                    pairing: None,
                    credentials: None,
                }));
            }
            Some(1) => (),
            _ => return Ok(None),
        }
    }
}
pub fn wizard() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "请使用 sudo pier-agent init"
    );
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "init 需要交互终端，请通过终端或 ssh -t 执行"
    );
    crate::systemd::available()?;
    directory(Path::new("/etc/pier"))?;
    let _lock = pier_protocol::state_lock(Path::new("/etc/pier/.agent-init-lock"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    if Path::new(DEFAULT_CONFIG).exists() {
        let config = load_config(Path::new(DEFAULT_CONFIG))?;
        if Path::new(PENDING).exists() {
            let pending: PendingInit = serde_json::from_slice(&secret_read(Path::new(PENDING))?)?;
            if let Some(credentials) = &pending.credentials {
                if credentials.agent_id == config.agent_id
                    && (credentials.connection_mode == ConnectionMode::ControllerToAgent
                        || runtime
                            .block_on(acknowledge(&pending.request.request_id, credentials))
                            .is_ok())
                {
                    fs::remove_file(PENDING)?;
                }
            }
        }
        println!("已有 Agent: {}", config.agent_id);
        loop {
            match select("现有服务", &["启动服务并启用开机自启", "查看状态", "退出"])?
            {
                Some(0) => return crate::systemd::start(),
                Some(1) => crate::systemd::status()?,
                _ => return Ok(()),
            }
        }
    }
    let mut pending = if Path::new(PENDING).exists() {
        if select("检测到未完成的初始化", &["继续初始化", "取消"])? != Some(0) {
            return Ok(());
        }
        serde_json::from_slice::<PendingInit>(&secret_read(Path::new(PENDING))?)?
    } else {
        let Some(pending) = initial_request()? else {
            return Ok(());
        };
        secret_write(Path::new(PENDING), &serde_json::to_vec(&pending)?, false)?;
        pending
    };
    let listener = if pending.request.connection_mode == ConnectionMode::ControllerToAgent
        && pending.credentials.is_none()
    {
        let _entered = runtime.enter();
        let listener = std::net::TcpListener::bind(pending.request.listen.context("缺少监听地址")?)
            .context("Agent 监听端口不可用，请检查地址或端口占用")?;
        listener.set_nonblocking(true)?;
        Some(tokio::net::TcpListener::from_std(listener)?)
    } else {
        None
    };
    while pending.credentials.is_none() {
        let pairing = pending
            .pairing
            .as_deref()
            .and_then(|value| Pairing::decode(value, &pending.request).ok());
        let pairing = if let Some(pairing) = pairing {
            pairing
        } else {
            let link = pending.request.link()?;
            println!("\n授权链接（可在其他电脑打开）：\n{link}\n");
            let desktop = std::env::var_os("DISPLAY").is_some()
                || std::env::var_os("WAYLAND_DISPLAY").is_some();
            let choices = if desktop {
                vec!["打开本机浏览器", "复制链接到其他电脑", "取消"]
            } else {
                vec!["复制链接到其他电脑", "尝试打开本机浏览器", "取消"]
            };
            let selected = select("授权方式", &choices)?;
            if selected.is_none() || selected == Some(2) {
                return Ok(());
            }
            if selected == Some(if desktop { 0 } else { 1 })
                && !Command::new("xdg-open")
                    .arg(&link)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            {
                println!("无法打开浏览器，请复制上述链接。");
            }
            loop {
                let value = pairing_input()?;
                match Pairing::decode(value.trim(), &pending.request) {
                    Ok(pairing) => {
                        pending.pairing = Some(value.trim().into());
                        secret_write(Path::new(PENDING), &serde_json::to_vec(&pending)?, true)?;
                        break pairing;
                    }
                    Err(_) => println!(
                        "配对凭据无效、已过期或与本次请求不匹配。请重新在网页授权后粘贴，或按 Ctrl+C 退出。"
                    ),
                }
            }
        };
        let redeemed = if let Some(listener) = &listener {
            println!("等待 Controller 连接，请在网页确认 Agent 可达地址。");
            runtime.block_on(redeem_incoming(&pending.request, &pairing, listener))
        } else {
            runtime.block_on(redeem(&pending.request, &pairing))
        };
        match redeemed {
            Ok(credentials) => {
                pending.credentials = Some(credentials);
                secret_write(Path::new(PENDING), &serde_json::to_vec(&pending)?, true)?;
            }
            Err(_) => {
                println!("配对连接失败，初始化进度已保存。请检查连接地址、监听端口和授权有效期。");
                match select(
                    "下一步",
                    &["重试连接", "重新取得配对凭据", "退出，稍后继续"],
                )? {
                    Some(0) => (),
                    Some(1) => {
                        pending.pairing = None;
                        secret_write(Path::new(PENDING), &serde_json::to_vec(&pending)?, true)?;
                    }
                    _ => return Ok(()),
                }
            }
        }
    }
    let credentials = pending.credentials.as_ref().unwrap();
    save_config(
        Path::new(DEFAULT_CONFIG),
        Path::new(TOKEN),
        &pending.state_dir,
        credentials,
    )?;
    if credentials.connection_mode == ConnectionMode::ControllerToAgent
        || runtime
            .block_on(acknowledge(&pending.request.request_id, credentials))
            .is_ok()
    {
        fs::remove_file(PENDING)?;
    } else {
        println!("配置已保存，授权确认暂未完成；下次 init 将重试确认。");
    }
    drop(listener);
    crate::systemd::start()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passive_configuration_is_private_and_legacy_configuration_keeps_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let config = dir.path().join("agent.yml");
        let token = dir.path().join("agent.token");
        let credentials = Credentials {
            agent_id: pier_protocol::new_id(),
            token: pier_protocol::new_token(),
            connection_mode: ConnectionMode::ControllerToAgent,
            listen: Some("0.0.0.0:7444".parse().unwrap()),
            controller_tcp: String::new(),
        };
        save_config(&config, &token, &dir.path().join("state"), &credentials).unwrap();
        save_config(&config, &token, &dir.path().join("state"), &credentials).unwrap();
        let restored = load_config(&config).unwrap();
        assert_eq!(restored.connection_mode, ConnectionMode::ControllerToAgent);
        assert_eq!(restored.listen, credentials.listen);
        assert!(
            !fs::read_to_string(config)
                .unwrap()
                .contains("controller_tcp")
        );
        let old: Config = serde_yaml_ng::from_str(
            "agent_id: old\ntoken_file: token\ncontroller_tcp: host:7443\nstate_dir: state\n",
        )
        .unwrap();
        assert_eq!(old.connection_mode, ConnectionMode::AgentToController);
        assert!(old.listen.is_none());
    }
    #[test]
    fn persistence_is_private_idempotent_and_never_overwrites_identity() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let config = dir.path().join("agent.yml");
        let token = dir.path().join("agent.token");
        let data = dir.path().join("data");
        let credentials = Credentials {
            connection_mode: ConnectionMode::default(),
            listen: None,
            agent_id: pier_protocol::new_id(),
            token: pier_protocol::new_token(),
            controller_tcp: "localhost:7443".into(),
        };
        save_config(&config, &token, &data, &credentials).unwrap();
        save_config(&config, &token, &data, &credentials).unwrap();
        for file in [&config, &token] {
            assert_eq!(fs::metadata(file).unwrap().mode() & 0o777, 0o600);
        }
        assert!(
            !fs::read_to_string(&config)
                .unwrap()
                .contains(&credentials.token)
        );
        let mut other = credentials.clone();
        other.agent_id = pier_protocol::new_id();
        assert!(save_config(&config, &token, &data, &other).is_err());
        assert_eq!(load_config(&config).unwrap().agent_id, credentials.agent_id);
        fs::remove_file(&config).unwrap();
        // A failed config write after the token was committed remains recoverable.
        save_config(&config, &token, &data, &credentials).unwrap();
        fs::remove_file(&token).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, &token).unwrap();
        assert!(save_config(&config, &token, &data, &credentials).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
    }
}
