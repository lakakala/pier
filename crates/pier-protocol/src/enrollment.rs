use crate::connection::ConnectionMode;
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitRequest {
    #[serde(default, skip_serializing_if = "ConnectionMode::is_default")]
    pub connection_mode: ConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<std::net::SocketAddr>,
    pub request_id: String,
    pub name: String,
    pub public_url: String,
    pub info: crate::AgentInfo,
}
impl InitRequest {
    pub fn validate(&self) -> Result<()> {
        match self.connection_mode {
            ConnectionMode::AgentToController => {
                ensure!(self.listen.is_none(), "unexpected agent listener")
            }
            ConnectionMode::ControllerToAgent => ensure!(
                self.listen.is_some_and(|a| a.port() > 0),
                "agent listener required"
            ),
        }
        ensure!(self.request_id.len() == 64, "invalid request id");
        crate::secure::decode_key(&self.request_id)?;
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 256
                && !self.name.chars().any(char::is_control),
            "invalid agent name"
        );
        ensure!(
            self.info.hostname.len() <= 256 && self.info.os_release.len() <= 8192,
            "invalid host information"
        );
        ensure!(
            origin(&self.public_url)? == self.public_url,
            "noncanonical public URL"
        );
        Ok(())
    }
    pub fn link(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "{}/agent/init#{}",
            self.public_url,
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    #[serde(default, skip_serializing_if = "ConnectionMode::is_default")]
    pub connection_mode: ConnectionMode,
    pub version: u32,
    pub grant_id: String,
    pub request_id: String,
    pub public_url: String,
    pub endpoint: String,
    pub secret: String,
    pub expires_at: u64,
}
impl Pairing {
    pub fn encode(&self) -> Result<String> {
        Ok(format!(
            "pier-pair-v2.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }
    pub fn decode(value: &str, request: &InitRequest) -> Result<Self> {
        ensure!(value.len() <= 16384, "pairing credential too large");
        let encoded = value
            .strip_prefix("pier-pair-v2.")
            .ok_or_else(|| anyhow::anyhow!("invalid pairing credential"))?;
        let pairing: Self = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded)?)?;
        ensure!(
            pairing.version == crate::VERSION && crate::safe_id(&pairing.grant_id),
            "invalid pairing version or id"
        );
        ensure!(
            pairing.connection_mode == request.connection_mode
                && pairing.request_id == request.request_id
                && pairing.public_url == request.public_url,
            "pairing belongs to another initialization or controller"
        );
        ensure!(
            pairing.expires_at > crate::now(),
            "pairing expired; authorize again in the browser"
        );
        crate::secure::decode_key(&pairing.secret)?;
        endpoint(&pairing.endpoint)?;
        Ok(pairing)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "ConnectionMode::is_default")]
    pub connection_mode: ConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<std::net::SocketAddr>,
    pub agent_id: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub controller_tcp: String,
}

pub fn origin(value: &str) -> Result<String> {
    let url = url::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "enter an HTTP or HTTPS origin, e.g. http://pier.example.com:8080"
    );
    Ok(url.origin().ascii_serialization())
}
pub fn endpoint(value: &str) -> Result<()> {
    let url = url::Url::parse(&format!("tcp://{value}"))?;
    ensure!(
        url.host_str().is_some()
            && url.port().is_some_and(|p| p > 0)
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().is_empty(),
        "endpoint must be host:port"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_binding_expiry_and_origins() {
        let request = InitRequest {
            connection_mode: ConnectionMode::default(),
            listen: None,
            request_id: crate::new_token(),
            name: "host".into(),
            public_url: "https://pier.example.test".into(),
            info: crate::AgentInfo {
                architecture: pier_pkg::Architecture::Amd64,
                hostname: "host".into(),
                os_release: String::new(),
            },
        };
        let mut pairing = Pairing {
            connection_mode: ConnectionMode::default(),
            version: crate::VERSION,
            request_id: request.request_id.clone(),
            grant_id: crate::new_id(),
            public_url: request.public_url.clone(),
            endpoint: "[::1]:7443".into(),
            secret: crate::new_token(),
            expires_at: crate::now() + 600,
        };
        assert!(Pairing::decode(&pairing.encode().unwrap(), &request).is_ok());
        pairing.request_id = crate::new_token();
        assert!(Pairing::decode(&pairing.encode().unwrap(), &request).is_err());
        pairing.request_id = request.request_id.clone();
        pairing.expires_at = 0;
        assert!(Pairing::decode(&pairing.encode().unwrap(), &request).is_err());
        for bad in [
            "ftp://example.test",
            "https://user@example.test",
            "http://user:password@example.test",
            "https://example.test/path",
            "http://example.test/path",
            "https://example.test?secret=x",
            "http://example.test#fragment",
        ] {
            assert!(origin(bad).is_err());
        }
        for bad in [
            "example.test",
            "user@example.test:7443",
            "example.test:7443/path",
            "example.test:0",
        ] {
            assert!(endpoint(bad).is_err());
        }
    }

    #[test]
    fn http_and_https_origins_preserve_scheme_and_nondefault_ports() {
        for (input, expected) in [
            ("http://example.test:80/", "http://example.test"),
            ("https://example.test:443/", "https://example.test"),
            ("http://example.test:8080", "http://example.test:8080"),
            ("https://example.test:8443", "https://example.test:8443"),
            ("http://[::1]:8080", "http://[::1]:8080"),
            ("https://[::1]:8443", "https://[::1]:8443"),
        ] {
            assert_eq!(origin(input).unwrap(), expected);
            let request = InitRequest {
                connection_mode: ConnectionMode::default(),
                listen: None,
                request_id: crate::new_token(),
                name: "host".into(),
                public_url: expected.into(),
                info: crate::AgentInfo {
                    architecture: pier_pkg::Architecture::Amd64,
                    hostname: "host".into(),
                    os_release: String::new(),
                },
            };
            assert!(
                request
                    .link()
                    .unwrap()
                    .starts_with(&format!("{expected}/agent/init#"))
            );
        }
    }
}
