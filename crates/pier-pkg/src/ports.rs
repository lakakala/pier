use crate::{Error, Result, Stage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PortValue {
    Number(u16),
    Template(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortDefinition {
    #[serde(default)]
    pub protocol: PortProtocol,
    pub port: PortValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    #[serde(default)]
    pub protocol: PortProtocol,
    pub port: u16,
}

pub type Ports = BTreeMap<String, Port>;
pub type PortDefinitions = BTreeMap<String, PortDefinition>;

pub(crate) fn check_declarations(ports: &PortDefinitions) -> Result<()> {
    if ports.len() > 128
        || ports.iter().any(|(name, value)| {
            name.is_empty()
                || name.len() > 80
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                || matches!(value.port, PortValue::Number(0))
        })
    {
        return Err(Error::new(Stage::Configuration, "invalid port declaration"));
    }
    Ok(())
}

/// Resolve port templates with the same app variables used for packaging.
pub fn resolve_ports(
    ports: &PortDefinitions,
    variables: &BTreeMap<String, String>,
) -> Result<Ports> {
    check_declarations(ports)?;
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    ports
        .iter()
        .map(|(name, declaration)| {
            let port = match &declaration.port {
                PortValue::Number(value) => Some(*value),
                PortValue::Template(template) => env
                    .render_str(template, variables)
                    .ok()
                    .and_then(|value| value.parse::<u16>().ok()),
            }
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                Error::new(
                    Stage::Configuration,
                    format!("port {name} must resolve to an integer in 1..65535"),
                )
            })?;
            Ok((
                name.clone(),
                Port {
                    protocol: declaration.protocol,
                    port,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literals_templates_and_invalid_ports() {
        let definitions: PortDefinitions =
            serde_yaml_ng::from_str("http: {port: '{{ PORT }}'}\ndns: {protocol: udp, port: 53}")
                .unwrap();
        let ports = resolve_ports(
            &definitions,
            &BTreeMap::from([("PORT".into(), "8080".into())]),
        )
        .unwrap();
        assert_eq!(
            ports["http"],
            Port {
                protocol: PortProtocol::Tcp,
                port: 8080
            }
        );
        assert_eq!(ports["dns"].protocol, PortProtocol::Udp);
        for value in ["0", "65536", "-1", "1.5", "80\nport: 90"] {
            assert!(
                resolve_ports(
                    &definitions,
                    &BTreeMap::from([("PORT".into(), value.into())])
                )
                .is_err()
            );
        }
        assert!(resolve_ports(&definitions, &BTreeMap::new()).is_err());
        assert!(serde_yaml_ng::from_str::<PortDefinition>("{protocol: http, port: 80}").is_err());
    }
}
