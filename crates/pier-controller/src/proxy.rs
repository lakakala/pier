//! Controller-only routing secrets. Public API views must never serialize Proxy.
use anyhow::{Result, ensure};
use pier_protocol::secure::{self, Purpose, SecureStream};
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, net::IpAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Proxy(String);
impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Proxy([redacted])")
    }
}
impl From<Proxy> for String {
    fn from(value: Proxy) -> Self {
        value.0
    }
}
impl TryFrom<String> for Proxy {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        // URL parsers can normalize whitespace, malformed escapes and paths. Check
        // the original spelling as well, and never include it in an error.
        let invalid = || anyhow::anyhow!("invalid SOCKS5 proxy URL");
        ensure!(
            value.starts_with("socks5://")
                && value.len() <= 4096
                && !value.chars().any(|c| c.is_whitespace() || c.is_control()),
            "invalid SOCKS5 proxy URL"
        );
        ensure!(
            !value[9..].contains(['/', '?', '#', '\\']),
            "invalid SOCKS5 proxy URL"
        );
        let url = url::Url::parse(&value).map_err(|_| invalid())?;
        let host = url.host_str().ok_or_else(invalid)?;
        ensure!(
            url.scheme() == "socks5"
                && url.port().is_some_and(|p| p > 0)
                && !host.contains('%')
                && host.is_ascii()
                && !host.is_empty()
                && url.path().is_empty()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid SOCKS5 proxy URL"
        );
        if value[9..].contains('@') {
            ensure!(
                value[9..].matches('@').count() == 1,
                "invalid SOCKS5 proxy URL"
            );
            credential(url.username())?;
            credential(url.password().ok_or_else(invalid)?)?;
        }
        Ok(Self(value))
    }
}

fn credential(value: &str) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut bytes = value.bytes();
    while let Some(b) = bytes.next() {
        decoded.push(if b == b'%' {
            let hi = bytes.next().and_then(|v| (v as char).to_digit(16));
            let lo = bytes.next().and_then(|v| (v as char).to_digit(16));
            match (hi, lo) {
                (Some(hi), Some(lo)) => (hi * 16 + lo) as u8,
                _ => anyhow::bail!("invalid SOCKS5 credential encoding"),
            }
        } else {
            b
        });
    }
    ensure!(
        !decoded.is_empty() && decoded.len() <= 255,
        "SOCKS5 credentials must contain 1–255 bytes"
    );
    let text = std::str::from_utf8(&decoded)
        .map_err(|_| anyhow::anyhow!("invalid SOCKS5 credential encoding"))?;
    ensure!(
        !text.chars().any(char::is_control),
        "invalid SOCKS5 credential encoding"
    );
    Ok(decoded)
}

/// Default applies only to a missing field; explicit null deserializes to Clear.
#[derive(Default)]
pub(crate) enum Patch {
    #[default]
    Preserve,
    Clear,
    Set(Proxy),
}
impl<'de> Deserialize<'de> for Patch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<Proxy>::deserialize(deserializer)? {
            Some(proxy) => Self::Set(proxy),
            None => Self::Clear,
        })
    }
}
impl Patch {
    pub fn apply(self, previous: Option<Proxy>) -> Option<Proxy> {
        match self {
            Self::Preserve => previous,
            Self::Clear => None,
            Self::Set(proxy) => Some(proxy),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub endpoint: String,
    pub proxy: Option<Proxy>,
}

/// Only fixed diagnostic text may reach last_error; never display underlying URL errors.
#[derive(Debug)]
pub(crate) struct DialError(&'static str);
impl fmt::Display for DialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for DialError {}

pub(crate) async fn connect(
    endpoint: &str,
    proxy: Option<&Proxy>,
    purpose: Purpose,
    id: &str,
    key: &[u8; 32],
) -> Result<SecureStream> {
    timeout(Duration::from_secs(10), async {
        let io = if let Some(proxy) = proxy {
            tunnel(endpoint, proxy).await?
        } else {
            TcpStream::connect(endpoint)
                .await
                .map_err(|_| DialError("无法连接 Agent，请检查地址和网络"))?
        };
        secure::connect_stream(io, purpose, id, key)
            .await
            .map_err(|_| DialError("Agent 加密认证失败，请检查身份和连接地址").into())
    })
    .await
    .map_err(|_| DialError("连接超时，请检查 Agent 地址、代理及网络"))?
}

async fn tunnel(endpoint: &str, proxy: &Proxy) -> Result<TcpStream> {
    // Proxy is validated when configured or loaded from storage.
    let url = url::Url::parse(&proxy.0)?;
    let host = url.host_str().unwrap().trim_matches(['[', ']']);
    let mut io = TcpStream::connect((host, url.port().unwrap()))
        .await
        .map_err(|_| DialError("无法连接 SOCKS5 代理"))?;
    io.set_nodelay(true)?;
    negotiate(&mut io, endpoint, &url).await.map_err(|error| {
        if error.is::<DialError>() {
            error
        } else {
            DialError("SOCKS5 协商失败，代理响应异常或连接中断").into()
        }
    })?;
    Ok(io)
}

async fn negotiate(io: &mut TcpStream, endpoint: &str, proxy: &url::Url) -> Result<()> {
    let auth = proxy.password().is_some();
    let method = if auth { 2 } else { 0 };
    // Do not allow an authenticated configuration to silently downgrade to no auth.
    io.write_all(&[5, 1, method]).await?;
    let mut reply = [0; 2];
    io.read_exact(&mut reply).await?;
    if reply != [5, method] {
        return Err(DialError("SOCKS5 代理不支持所配置的认证方式").into());
    }
    if auth {
        let username = credential(proxy.username())?;
        let password = credential(proxy.password().unwrap())?;
        let mut request = vec![1, username.len() as u8];
        request.extend_from_slice(&username);
        request.push(password.len() as u8);
        request.extend_from_slice(&password);
        io.write_all(&request).await?;
        io.read_exact(&mut reply).await?;
        if reply != [1, 0] {
            return Err(DialError("SOCKS5 用户名或密码认证失败").into());
        }
    }
    pier_protocol::enrollment::endpoint(endpoint)?;
    let target = url::Url::parse(&format!("tcp://{endpoint}"))?;
    let host = target.host_str().unwrap().trim_matches(['[', ']']);
    let mut request = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            request.push(1);
            request.extend_from_slice(&address.octets());
        }
        Ok(IpAddr::V6(address)) => {
            request.push(4);
            request.extend_from_slice(&address.octets());
        }
        Err(_) => {
            ensure!(
                host.is_ascii() && !host.is_empty() && host.len() <= 255,
                "invalid SOCKS5 target domain"
            );
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&target.port().unwrap().to_be_bytes());
    io.write_all(&request).await?;
    let mut header = [0; 4];
    io.read_exact(&mut header).await?;
    ensure!(header[0] == 5 && header[2] == 0, "invalid SOCKS5 response");
    if header[1] != 0 {
        return Err(DialError(match header[1] {
            2 => "SOCKS5 代理规则拒绝连接 Agent",
            3 => "SOCKS5 代理无法到达目标网络",
            4 => "SOCKS5 代理无法到达 Agent 主机",
            5 => "SOCKS5 代理连接 Agent 被拒绝",
            _ => "SOCKS5 代理未能建立 Agent 连接",
        })
        .into());
    }
    let length = match header[3] {
        1 => 4,
        4 => 16,
        3 => {
            let n = io.read_u8().await? as usize;
            ensure!(n > 0, "invalid SOCKS5 response");
            n
        }
        _ => anyhow::bail!("invalid SOCKS5 response"),
    };
    // Consume precisely BND.ADDR and BND.PORT, leaving any tunnel bytes untouched.
    let mut bound = vec![0; length + 2];
    io.read_exact(&mut bound).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
