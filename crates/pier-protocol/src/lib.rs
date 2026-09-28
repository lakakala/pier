pub mod enrollment;
pub mod secure;
pub mod store;
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
    pub commit: String,
    pub architecture: Architecture,
    pub apps: Vec<DeploymentApp>,
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
