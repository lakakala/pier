use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, path::PathBuf, str::FromStr};

/// Required Linux CPU architecture for one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Architecture {
    #[serde(rename = "amd64")]
    Amd64,
    #[serde(rename = "arm64")]
    Arm64,
}

impl Architecture {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
    pub fn rust_target(self) -> &'static str {
        match self {
            Self::Amd64 => "x86_64-unknown-linux-gnu",
            Self::Arm64 => "aarch64-unknown-linux-gnu",
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for Architecture {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "amd64" => Ok(Self::Amd64),
            "arm64" => Ok(Self::Arm64),
            _ => Err(format!("unsupported architecture: {s}")),
        }
    }
}

/// Addresses only. The recipe's `proxy.enabled` is the sole enable switch.
#[derive(Clone, Default)]
pub struct ProxyOptions {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
}
impl fmt::Debug for ProxyOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyOptions")
            .field(
                "http_proxy",
                &self.http_proxy.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "https_proxy",
                &self.https_proxy.as_ref().map(|_| "[redacted]"),
            )
            .field("no_proxy", &self.no_proxy.as_ref().map(|_| "[configured]"))
            .finish()
    }
}

#[derive(Clone)]
pub struct PackOptions {
    pub variables: BTreeMap<String, String>,
    pub architecture: Architecture,
    /// Required for Git sources; forbidden for binary downloads.
    pub image: Option<String>,
    pub output_dir: PathBuf,
    pub overwrite: bool,
    pub proxy: ProxyOptions,
}
impl PackOptions {
    /// Select an architecture explicitly; there is no default build platform.
    pub fn new(architecture: Architecture) -> Self {
        Self {
            variables: BTreeMap::new(),
            architecture,
            image: None,
            output_dir: "dist".into(),
            overwrite: false,
            proxy: ProxyOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedPackage {
    pub architecture: Architecture,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub name: String,
    pub version: String,
    pub package: PlannedPackage,
    pub configuration_files: Vec<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageArtifact {
    pub architecture: Architecture,
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Configuration,
    Variables,
    Template,
    Proxy,
    Git,
    Download,
    Build,
    Files,
    Compatibility,
    Archive,
}

/// Errors deliberately omit raw variable values, proxy URLs and child output.
#[derive(Debug)]
pub struct Error {
    pub stage: Stage,
    pub architecture: Option<Architecture>,
    pub path: Option<PathBuf>,
    pub message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}
impl Error {
    pub(crate) fn new(stage: Stage, message: impl Into<String>) -> Self {
        Self {
            stage,
            architecture: None,
            path: None,
            message: message.into(),
            source: None,
        }
    }
    pub(crate) fn at(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
    pub(crate) fn cause(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.stage, self.message)?;
        if let Some(architecture) = self.architecture {
            write!(f, " [architecture={architecture}]")?;
        }
        if let Some(p) = &self.path {
            write!(f, " [path={} ]", p.display())?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|s| &**s as _)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) trait IoResult<T> {
    fn context(self, stage: Stage, path: impl Into<PathBuf>) -> Result<T>;
}
impl<T> IoResult<T> for std::io::Result<T> {
    fn context(self, stage: Stage, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|e| Error::new(stage, e.to_string()).at(path).cause(e))
    }
}
