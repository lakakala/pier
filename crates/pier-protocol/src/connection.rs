use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub type Wire =
    tokio_util::codec::Framed<crate::secure::SecureStream, tokio_util::codec::LengthDelimitedCodec>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    AgentToController,
    ControllerToAgent,
}
impl ConnectionMode {
    pub fn is_default(&self) -> bool {
        *self == Self::AgentToController
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    #[serde(default)]
    pub mode: ConnectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}
impl Connection {
    pub fn validate(&self) -> Result<()> {
        match self.mode {
            ConnectionMode::AgentToController => ensure!(
                self.endpoint.is_none(),
                "outbound agent cannot have a dial address"
            ),
            ConnectionMode::ControllerToAgent => {
                let endpoint = self
                    .endpoint
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("agent endpoint required"))?;
                crate::enrollment::endpoint(endpoint)?;
                let url = url::Url::parse(&format!("tcp://{endpoint}"))?;
                ensure!(
                    !matches!(url.host_str(), Some("0.0.0.0" | "[::]")),
                    "agent endpoint must be reachable, not a wildcard"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_defaults_and_passive_endpoints_are_unambiguous() {
        let old: Connection = serde_json::from_str("{}").unwrap();
        assert_eq!(old.mode, ConnectionMode::AgentToController);
        old.validate().unwrap();
        for endpoint in ["host:7444", "127.0.0.1:7444", "[::1]:7444"] {
            Connection {
                mode: ConnectionMode::ControllerToAgent,
                endpoint: Some(endpoint.into()),
            }
            .validate()
            .unwrap();
        }
        for endpoint in [
            "0.0.0.0:7444",
            "[::]:7444",
            "host",
            "host:0",
            "user@host:7444",
            "host:7444/path",
        ] {
            assert!(
                Connection {
                    mode: ConnectionMode::ControllerToAgent,
                    endpoint: Some(endpoint.into())
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            Connection {
                mode: ConnectionMode::ControllerToAgent,
                endpoint: None
            }
            .validate()
            .is_err()
        );
        assert!(
            Connection {
                mode: ConnectionMode::AgentToController,
                endpoint: Some("host:7444".into())
            }
            .validate()
            .is_err()
        );
    }
}
