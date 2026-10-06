pub mod connection;
pub mod enrollment;
pub mod forward;
pub mod secure;
pub mod store;
pub mod terminal;
pub mod upgrade;

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use pier_pkg::Architecture;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub const VERSION: u32 = 2;
pub const MULTI_BLUEPRINT_CAPABILITY: &str = "multi_blueprint_v1";
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub type Variables = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInfo {
    pub architecture: Architecture,
    pub hostname: String,
    pub os_release: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentApp {
    pub instance: String,
    pub id: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentPlan {
    pub id: String,
    pub agent_id: String,
    pub blueprint: String,
    #[serde(default)]
    pub blueprint_name: String,
    #[serde(default)]
    pub action: DeploymentAction,
    pub commit: String,
    pub architecture: Architecture,
    pub apps: Vec<DeploymentApp>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentAction {
    #[default]
    Deploy,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueprintStatus {
    #[serde(default)]
    pub account_reserved: bool,
    pub id: String,
    pub blueprint: String,
    pub name: String,
    pub deployment_id: Option<String>,
    pub state: String,
    pub apps: Vec<AppStatus>,
    pub result: Option<DeploymentResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppStatus {
    pub instance: String,
    pub id: String,
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u64,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub blueprints: Vec<BlueprintStatus>,
    pub deployment_id: Option<String>,
    pub apps: Vec<AppStatus>,
    pub result: Option<DeploymentResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentResult {
    pub id: String,
    pub state: String,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Forward {
        frame: forward::Frame,
    },
    PortConfig {
        listeners: Vec<forward::Listener>,
    },
    PortStatus {
        statuses: Vec<forward::ListenerStatus>,
    },
    Session {
        id: String,
    },
    OpenChannel {
        session: String,
        id: String,
        purpose: secure::Purpose,
    },
    Channel {
        session: String,
        id: String,
        purpose: secure::Purpose,
    },
    Hello {
        version: u32,
        agent_id: String,
        info: AgentInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        software: Option<upgrade::Software>,
    },
    Welcome {
        version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upgrade: Option<upgrade::Offer>,
    },
    UpgradeDownload {
        sha256: String,
    },
    UpgradeReserve {
        sha256: String,
    },
    UpgradeGrant {
        grant: Option<String>,
    },
    UpgradeStatus {
        status: upgrade::Status,
    },
    Ping,
    Pong,
    TerminalOpen {
        id: String,
        instance: String,
        cols: u16,
        rows: u16,
    },
    Deploy {
        plan: DeploymentPlan,
    },
    Report {
        report: AgentReport,
    },
    Progress {
        id: String,
        phase: String,
    },
    Result {
        result: DeploymentResult,
    },
    Enroll {
        request: enrollment::InitRequest,
    },
    Enrolled {
        credentials: enrollment::Credentials,
    },
    EnrollmentAck {
        request_id: String,
    },
    Acked,
    ArtifactRequest {
        deployment: String,
        app: String,
    },
    ArtifactBegin {
        size: u64,
    },
    ArtifactChunk {
        data: String,
    },
    ArtifactEnd {
        size: u64,
    },
}

pub fn framed<S: AsyncRead + AsyncWrite>(stream: S) -> Framed<S, LengthDelimitedCodec> {
    Framed::new(
        stream,
        LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME)
            .new_codec(),
    )
}
pub async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut Framed<S, LengthDelimitedCodec>,
    message: &Message,
) -> Result<()> {
    let data = serde_json::to_vec(message)?;
    if data.len() > MAX_FRAME {
        bail!("message exceeds maximum frame length");
    }
    stream.send(data.into()).await?;
    Ok(())
}
pub async fn receive<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut Framed<S, LengthDelimitedCodec>,
) -> Result<Message> {
    let bytes = stream
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("connection closed"))??;
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn hash(value: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(value.as_ref()))
}
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
/// Blueprint names used as Linux user/group names, without normalization or truncation.
pub fn valid_system_username(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 32
        && matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
/// Controlled blueprint-account diagnostics safe to include in deployment results.
/// Never construct this from raw subprocess output or network errors.
#[derive(Debug, PartialEq, Eq)]
pub enum BlueprintAccountError {
    InvalidName,
    DuplicateName(String),
    ReservedName(String),
    NameChanged(String),
    OwnershipConflict(String),
    IdentityChanged(String),
    UserConflict(String),
    GroupConflict(String),
    CreateFailed(String),
}
impl std::fmt::Display for BlueprintAccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName => f.write_str(
                "blueprint name must be a system username matching [A-Za-z_][A-Za-z0-9_.-]{0,31}",
            ),
            Self::DuplicateName(name) => write!(f, "duplicate blueprint username: {name}"),
            Self::ReservedName(name) => {
                write!(
                    f,
                    "blueprint username is reserved by another blueprint: {name}"
                )
            }
            Self::NameChanged(name) => {
                write!(f, "blueprint username cannot change during upgrade: {name}")
            }
            Self::OwnershipConflict(name) => {
                write!(f, "blueprint account ownership conflict: {name}")
            }
            Self::IdentityChanged(name) => write!(f, "blueprint account identity changed: {name}"),
            Self::UserConflict(name) => write!(
                f,
                "blueprint username belongs to another blueprint or system user: {name}"
            ),
            Self::GroupConflict(name) => {
                write!(
                    f,
                    "blueprint group already exists without an owned user: {name}"
                )
            }
            Self::CreateFailed(name) => {
                write!(f, "cannot create dedicated blueprint user/group: {name}")
            }
        }
    }
}
impl std::error::Error for BlueprintAccountError {}
impl BlueprintAccountError {
    pub fn deployment_error(&self) -> String {
        format!("{self}; existing deployment retained")
    }
    /// Allow only known account diagnostics through the existing error string field.
    pub fn from_deployment_error(value: &str) -> Option<Self> {
        let reason = value.strip_suffix("; existing deployment retained")?;
        if reason == Self::InvalidName.to_string() {
            return Some(Self::InvalidName);
        }
        let (_, name) = reason.rsplit_once(": ")?;
        if !valid_system_username(name) {
            return None;
        }
        let constructors: [fn(String) -> Self; 8] = [
            Self::DuplicateName,
            Self::ReservedName,
            Self::NameChanged,
            Self::OwnershipConflict,
            Self::IdentityChanged,
            Self::UserConflict,
            Self::GroupConflict,
            Self::CreateFailed,
        ];
        constructors
            .into_iter()
            .map(|make| make(name.into()))
            .find(|error| error.to_string() == reason)
    }
}

pub fn relative(value: &str) -> Result<()> {
    if value == "." {
        return Ok(());
    }
    if value.is_empty()
        || Path::new(value).is_absolute()
        || value.contains(['\\', '\0', ':'])
        || value
            .split('/')
            .any(|v| v.is_empty() || v == "." || v == "..")
    {
        bail!("invalid repository relative directory");
    }
    Ok(())
}
pub fn token_matches(token: &str, digest: &str) -> bool {
    let actual = hash(token);
    actual.len() == digest.len()
        && actual
            .bytes()
            .zip(digest.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Hold for the lifetime of a daemon to serialize startup recovery and ownership.
pub fn state_lock(directory: &Path) -> Result<std::fs::File> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    std::fs::create_dir_all(directory)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("daemon.lock"))?;
    // SAFETY: valid owned FD; closing the file releases the advisory lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another daemon is already using this state directory");
    }
    Ok(file)
}

pub fn unique_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct Visitor<K, V>(std::marker::PhantomData<(K, V)>);
    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> serde::de::Visitor<'de>
        for Visitor<K, V>
    {
        type Value = BTreeMap<K, V>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("mapping without duplicate keys")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry()? {
                if values.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate mapping key"));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(Visitor(std::marker::PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_controlled_account_errors_are_public() {
        use BlueprintAccountError::*;
        for reason in [
            InvalidName,
            DuplicateName("Demo.Api".into()),
            ReservedName("demo".into()),
            NameChanged("demo".into()),
            OwnershipConflict("demo".into()),
            IdentityChanged("demo".into()),
            UserConflict("root".into()),
            GroupConflict("demo".into()),
            CreateFailed("demo".into()),
        ] {
            assert_eq!(
                BlueprintAccountError::from_deployment_error(&reason.deployment_error()),
                Some(reason)
            );
        }
        for error in [
            "download failed: https://user:secret@example.com/; existing deployment retained",
            "blueprint account identity changed: demo\nsecret; existing deployment retained",
            "blueprint account identity changed: demo; existing deployment retained; secret",
            "unexpected error: demo; existing deployment retained",
            "blueprint account identity changed: 123; existing deployment retained",
            "blueprint account identity changed: demo",
        ] {
            assert!(BlueprintAccountError::from_deployment_error(error).is_none());
        }
        assert!(
            BlueprintAccountError::from_deployment_error(
                &IdentityChanged("a".repeat(33)).deployment_error()
            )
            .is_none()
        );
    }
    #[test]
    fn system_usernames_preserve_case_and_have_portable_boundaries() {
        for name in ["a", "_", "CLIProxyAPI", "demo-v2.1_app", &"a".repeat(32)] {
            assert!(valid_system_username(name), "{name:?}");
        }
        for name in [
            "",
            "1demo",
            "-demo",
            ".demo",
            "demo+api",
            "a b",
            "a:b",
            "a/b",
            "a\\b",
            "a\n",
            "a\0",
            "服务",
            "{{ NAME }}",
            &"a".repeat(33),
        ] {
            assert!(!valid_system_username(name), "{name:?}");
        }
    }
    #[tokio::test]
    async fn frames_survive_fragmentation_and_coalescing() {
        let (a, b) = tokio::io::duplex(7);
        let sender = tokio::spawn(async move {
            let mut stream = framed(a);
            send(&mut stream, &Message::Ping).await.unwrap();
            send(&mut stream, &Message::Pong).await.unwrap();
        });
        let mut stream = framed(b);
        assert!(matches!(receive(&mut stream).await.unwrap(), Message::Ping));
        assert!(matches!(receive(&mut stream).await.unwrap(), Message::Pong));
        sender.await.unwrap();
    }
    #[tokio::test]
    async fn rejects_oversized_frame_before_payload() {
        use tokio::io::AsyncWriteExt;
        let (mut a, b) = tokio::io::duplex(32);
        a.write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .await
            .unwrap();
        assert!(receive(&mut framed(b)).await.is_err());
    }
}
